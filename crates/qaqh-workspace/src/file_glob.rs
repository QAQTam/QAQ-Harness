//! Native glob tool: workspace-rooted path listing with gitignore-style patterns.
//!
//! Pure in-process implementation (`globset` matcher + `ignore::WalkBuilder`
//! traversal) — **no shell, no external binaries**. Sandbox-safe: `ReadOnly`
//! risk, workspace-rooted, hidden/gitignored paths skipped by default.
//!
//! Syntax is identical to `rg -g` / gitignore / VS Code file search:
//! - `*` matches within one path segment (does NOT cross `/`)
//! - `**` matches across any number of directories
//! - `?` matches a single character
//! - `[a-z]` / `[abc]` character classes
//! - `{a,b}` alternation

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};
use std::path::Path;

/// 默认返回上限：防超大仓库结果爆炸（`rg --files` 语义下的熔断）。
const DEFAULT_MAX_RESULTS: usize = 500;
const MAX_RESULTS_CAP: usize = 10_000;

fn exec_glob(args: &serde_json::Value) -> ToolResult {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if pattern.is_empty() {
        return ToolResult::error("glob: pattern is required (e.g. \"src/**/*.rs\")");
    }
    let root = crate::resolve_workspace_path(
        args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    );
    let root = if root.is_empty() { "." } else { root.as_str() };
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP);

    // gitignore 风格：`*` 不跨 `/`（与 rg -g / VS Code 一致）。
    let glob = match globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
    {
        Ok(glob) => glob,
        Err(e) => return ToolResult::error(format!("glob: invalid pattern: {e}")),
    };
    let matcher = glob.compile_matcher();

    let root_path = Path::new(root);
    let walker = ignore::WalkBuilder::new(root_path)
        // rg --files 默认：跳过 hidden + gitignored + parent 忽略规则；
        // require_git(false)：非 git 仓库目录同样应用 .gitignore（对齐 rg）。
        .standard_filters(true)
        .require_git(false)
        .build();

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root_path) else {
            continue;
        };
        // 统一 `/` 分隔（globset 按 `/` 匹配；Windows 下 Path 是 `\`）。
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&rel_str) {
            matches.push(rel_str);
            if matches.len() >= max_results {
                truncated = true;
                break;
            }
        }
    }

    matches.sort();
    let mut text = matches.join("\n");
    if truncated {
        text.push_str(&format!("\n... truncated at {max_results} matches"));
    }
    if text.is_empty() {
        text = "(no files match the pattern)".to_string();
    }
    ToolResult::ok_data(
        serde_json::json!({
            "matches": matches,
            "truncated": truncated,
            "count": matches.len(),
        }),
        text,
    )
}

handler!(handle_glob, exec_glob);

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "glob".to_string(),
            description: "List files matching a gitignore-style glob pattern (native \
                implementation, no shell). Pattern is relative to `path` (default: \
                workspace root). `*` matches within one path segment, `**` crosses \
                directories, `?` one char, `[a-z]` char class, `{a,b}` alternation \
                (same syntax as `rg -g` / VS Code file search). Hidden files and \
                gitignored paths are skipped (rg --files defaults). Returns relative \
                paths one per line, sorted, capped at max_results.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern relative to root, e.g. \"src/**/*.rs\", \"*.md\", \"crates/{qaqh-a,qaqh-b}/src/lib.rs\""
                    },
                    "path": {
                        "type": "string",
                        "description": "Search root (default: workspace root)"
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10000,
                        "description": "Max paths to return (default 500; overflow flagged truncated)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            handler: handle_glob,
            risk: ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(30),
        },
        crate::ToolPlacement::HostOnly,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/sub")).expect("mkdir src/sub");
        fs::create_dir_all(dir.path().join("crates/qaqh-a/src")).expect("mkdir crates");
        fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write a.rs");
        fs::write(dir.path().join("src/sub/b.rs"), "fn b() {}\n").expect("write b.rs");
        fs::write(dir.path().join("src/data.txt"), "x\n").expect("write txt");
        fs::write(dir.path().join("src/.hidden.rs"), "fn h() {}\n").expect("write hidden");
        fs::write(
            dir.path().join("crates/qaqh-a/src/lib.rs"),
            "pub fn x() {}\n",
        )
        .expect("write lib.rs");
        fs::write(dir.path().join("README.md"), "# hi\n").expect("write readme");
        dir
    }

    fn run(root: &Path, args: serde_json::Value) -> ToolResult {
        let mut args = args;
        if let Some(obj) = args.as_object_mut() {
            obj.insert(
                "path".to_string(),
                serde_json::json!(root.to_string_lossy().to_string()),
            );
        }
        exec_glob(&args)
    }

    fn lines(result: &ToolResult) -> Vec<String> {
        result.model_text().lines().map(String::from).collect()
    }

    #[test]
    fn glob_star_star_matches_recursively() {
        let dir = fixture();
        let result = run(dir.path(), serde_json::json!({ "pattern": "src/**/*.rs" }));
        assert!(result.is_success(), "{}", result.model_text());
        let list = lines(&result);
        assert!(list.contains(&"src/a.rs".to_string()));
        assert!(list.contains(&"src/sub/b.rs".to_string()));
        // `*` 不跨 `/`：单星不匹配子目录。
        let single = run(dir.path(), serde_json::json!({ "pattern": "src/*.rs" }));
        let single_list = lines(&single);
        assert!(single_list.contains(&"src/a.rs".to_string()));
        assert!(!single_list.contains(&"src/sub/b.rs".to_string()));
    }

    #[test]
    fn glob_skips_hidden_and_gitignored() {
        let dir = fixture();
        fs::write(dir.path().join(".gitignore"), "data.txt\n").expect("gitignore");
        let result = run(dir.path(), serde_json::json!({ "pattern": "**/*" }));
        assert!(result.is_success(), "{}", result.model_text());
        let list = lines(&result);
        assert!(list.contains(&"src/a.rs".to_string()));
        assert!(
            !list.contains(&"src/.hidden.rs".to_string()),
            "hidden must be skipped"
        );
        assert!(
            !list.contains(&"src/data.txt".to_string()),
            "gitignored must be skipped"
        );
    }

    #[test]
    fn glob_alternation_and_root_limiting() {
        let dir = fixture();
        let result = run(
            dir.path(),
            serde_json::json!({ "pattern": "crates/{qaqh-a,qaqh-b}/src/lib.rs" }),
        );
        let list = lines(&result);
        assert!(list.contains(&"crates/qaqh-a/src/lib.rs".to_string()));
        assert!(!list.contains(&"README.md".to_string()));
    }

    #[test]
    fn glob_invalid_pattern_and_empty_match() {
        let dir = fixture();
        let bad = run(
            dir.path(),
            serde_json::json!({ "pattern": "src/[unclosed" }),
        );
        assert!(!bad.is_success(), "invalid glob must error");
        let none = run(dir.path(), serde_json::json!({ "pattern": "*.toml" }));
        assert!(none.is_success());
        assert!(lines(&none).iter().any(|l| l.contains("no files match")));
    }

    #[test]
    fn glob_is_read_only_under_permission_engine() {
        // 权限引擎必须把 glob（handler 声明 Read）视为只读：Level 2 自动批准。
        use crate::permission::{PermissionDecision, PermissionLevel, needs_permission};
        let ws = std::env::temp_dir().join("qaqh-glob-perm");
        let decision = needs_permission(
            PermissionLevel::ReadFree,
            "glob",
            &serde_json::json!({ "pattern": "**/*.rs" }),
            &ws,
            &Default::default(),
            crate::permission::ToolCategory::Read,
        );
        assert!(matches!(decision, PermissionDecision::AutoApprove));
    }

    #[test]
    fn glob_respects_max_results() {
        let dir = fixture();
        let result = run(
            dir.path(),
            serde_json::json!({ "pattern": "**/*", "max_results": 2 }),
        );
        assert!(result.is_success(), "{}", result.model_text());
        // 文本截断标记 + 恰好 2 行路径（其余为截断提示）。
        assert!(result.model_text().contains("truncated"));
        let path_lines = result
            .model_text()
            .lines()
            .filter(|l| !l.starts_with("..."))
            .count();
        assert_eq!(path_lines, 2);
    }
}
