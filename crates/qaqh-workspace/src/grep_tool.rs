//! `grep`：内容搜索工具——直接使用 ripgrep 的核心库
//! （`grep-searcher` + `grep-regex` + `ignore`，与 rg 二进制同一实现），
//! **零外部二进制依赖**：用户机器上没有 rg 也能用。
//!
//! 与 `glob`（文件名列举）分工：grep 搜**文件内容**，输出 `path:line:content`
//! （上下文行用 rg 的 `path-line-content` 格式）。与 `exec` 分工：grep 是
//! 模型友好的封装——workspace 边界内、结果截断、结构化输出。
//!
//! - 正则语法 = rg（Rust regex 引擎；`(?i)`、`\b`、`|` 等均可用）。
//! - 默认大小写不敏感（对齐 Claude Code Grep）；`case_sensitive=true` 关闭。
//! - 默认跳过 hidden/gitignored/binary 文件（rg 原生行为）。
//! - `max_results` 截断 + `truncated` 标记，防上下文爆炸。

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_RESULTS_CAP: usize = 2_000;

struct MatchLine {
    path: String,
    line: u64,
    content: String,
    is_context: bool,
}

/// Sink：按 searcher 的回调顺序收集匹配行与上下文行（顺序天然正确）。
/// 达到 max_results 条匹配后返回 `Ok(false)` 停止当前文件。
struct CollectSink {
    path: String,
    /// 绝对路径（与 read 的账本 key 同形态，record_grep 清链用）。
    abs_path: String,
    matches: Vec<MatchLine>,
    max_matches: usize,
    truncated: bool,
}

impl Sink for CollectSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.matches.iter().filter(|m| !m.is_context).count() >= self.max_matches {
            self.truncated = true;
            return Ok(false);
        }
        // grep 输出的是实时行号：清空该文件的偏移链，防止后续 read 用
        // grep 行号时被账本修正误伤（read→edit→grep→read 链）。
        crate::file_state::record_grep(&self.abs_path);
        let content = mat
            .lines()
            .next()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default();
        let content = content.trim_end_matches(['\n', '\r']).to_string();
        self.matches.push(MatchLine {
            path: self.path.clone(),
            line: mat.line_number().unwrap_or(0),
            content,
            is_context: false,
        });
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        // 上下文行跟随在匹配行之后输出（rg 的 -A/-B 语义）。
        if self.truncated {
            return Ok(false);
        }
        let content = String::from_utf8_lossy(context.bytes())
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.matches.push(MatchLine {
            path: self.path.clone(),
            line: context.line_number().unwrap_or(0),
            content,
            is_context: true,
        });
        Ok(true)
    }
}

/// Lexically resolve `.`/`..` components without touching the filesystem.
fn lexically_normalize(p: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise prefix comparison (case-insensitive on Windows), with `..`
/// lexically resolved first — prevents `ws/../../..` from slipping past a
/// plain `Path::starts_with` (which compares component prefixes only).
fn path_within_workspace(abs: &std::path::Path, ws: &std::path::Path) -> bool {
    let a_norm = lexically_normalize(abs);
    let a: Vec<_> = a_norm.components().collect();
    let w_norm = lexically_normalize(ws);
    let w: Vec<_> = w_norm.components().collect();
    if a.len() < w.len() {
        return false;
    }
    a[..w.len()].iter().zip(w.iter()).all(|(x, y)| {
        #[cfg(windows)]
        {
            x.as_os_str().to_string_lossy().to_lowercase()
                == y.as_os_str().to_string_lossy().to_lowercase()
        }
        #[cfg(not(windows))]
        {
            x == y
        }
    })
}

fn exec_grep(args: &serde_json::Value) -> ToolResult {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if pattern.is_empty() {
        return ToolResult::error(
            "grep: 'pattern' is required (regex, e.g. \"fn main\" or \"TODO|FIXME\")",
        );
    }
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP);

    // ── 匹配器（rg 同款：RegexMatcherBuilder）──────────────────────
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(
            !args
                .get("case_sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => {
            return ToolResult::error(format!("grep: invalid regex {pattern:?}: {e}"));
        }
    };

    let context_before = args
        .get("context_before")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let context_after = args
        .get("context_after")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    // ── glob 过滤（rg -g 语义：positive 至少一个命中；`!` 前缀排除）──
    let globs: Vec<String> = args
        .get("glob")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let (neg_globs, pos_globs): (Vec<&str>, Vec<&str>) = globs
        .iter()
        .map(|g| g.as_str())
        .partition(|g| g.starts_with('!'));
    let pos_matchers: Vec<globset::GlobMatcher> = pos_globs
        .iter()
        .filter_map(|g| {
            globset::GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .ok()
        })
        .map(|g| g.compile_matcher())
        .collect();
    let neg_matchers: Vec<globset::GlobMatcher> = neg_globs
        .iter()
        .filter_map(|g| {
            globset::GlobBuilder::new(g.strip_prefix('!').unwrap_or(g))
                .literal_separator(true)
                .build()
                .ok()
        })
        .map(|g| g.compile_matcher())
        .collect();
    let glob_filter = |rel: &str| -> bool {
        // rg -g 是 gitignore 语义：不含 `/` 的模式（如 "*.rs"）匹配任意层级
        // 的 basename；含 `/` 的模式匹配相对路径。
        let basename = rel.rsplit('/').next().unwrap_or(rel);
        let pos_hit = pos_matchers.is_empty()
            || pos_matchers
                .iter()
                .any(|m| m.is_match(rel) || m.is_match(basename));
        if !pos_hit {
            return false;
        }
        !neg_matchers
            .iter()
            .any(|m| m.is_match(rel) || m.is_match(basename))
    };

    // ── workspace 边界 + 搜索根 ───────────────────────────────────
    let ws = crate::current_workspace();
    let ws = if ws.is_empty() { ".".to_string() } else { ws };
    let ws_path = std::path::Path::new(&ws);
    let strip_verbatim = |p: &std::path::Path| -> std::path::PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        std::path::PathBuf::from(s)
    };
    // canonicalize 在 Windows 返回 verbatim 前缀路径；剥掉后供边界检查与
    // strip_prefix 共用（walker 产出的路径不带该前缀）。
    let ws_abs = ws_path
        .canonicalize()
        .unwrap_or_else(|_| ws_path.to_path_buf());
    let ws_abs = strip_verbatim(&ws_abs);

    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    let raw_paths: Vec<&str> = match args.get("paths").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .collect(),
        None => Vec::new(),
    };
    if raw_paths.is_empty() {
        roots.push(ws_abs.clone());
    } else {
        for p in &raw_paths {
            let resolved = crate::resolve_workspace_path(p);
            let resolved = std::path::Path::new(&resolved);
            let abs = if resolved.is_absolute() {
                resolved.to_path_buf()
            } else {
                ws_path.join(resolved)
            };
            // 相对路径提升为绝对（不碰文件系统），并词法解析 `..`——
            // 否则 `ws/../../..` 的 starts_with(ws) 组件前缀检查会误放行。
            let abs = std::path::absolute(&abs).unwrap_or(abs);
            if !path_within_workspace(&abs, &ws_abs) {
                return ToolResult::error(format!(
                    "grep: path {p:?} resolves outside the workspace — search is workspace-bounded"
                ));
            }
            roots.push(abs);
        }
    }

    // ── 遍历 + 搜索（rg 同款：ignore 遍历 + Searcher 逐文件）────────
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .after_context(context_after)
        .before_context(context_before)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let mut all_matches: Vec<MatchLine> = Vec::new();
    let mut truncated = false;

    'roots: for root in &roots {
        let walker = ignore::WalkBuilder::new(root)
            .standard_filters(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(&ws_abs)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if !glob_filter(&rel) {
                continue;
            }
            // max_results 是**全局**预算：先看已收集数，再给当前文件剩余配额。
            let collected = all_matches.iter().filter(|m| !m.is_context).count();
            if collected >= max_results {
                truncated = true;
                break 'roots;
            }
            let mut sink = CollectSink {
                path: rel.clone(),
                abs_path: path.to_string_lossy().into_owned(),
                matches: Vec::new(),
                max_matches: max_results - collected,
                truncated: false,
            };
            // 忽略单个文件的 IO 错误（rg 行为：不可读文件跳过）。
            let _ = searcher.search_path(&matcher, path, &mut sink);
            if sink.truncated {
                truncated = true;
            }
            all_matches.extend(sink.matches);
            if truncated {
                break 'roots;
            }
        }
    }

    // ── 输出 ──────────────────────────────────────────────────────
    let match_count = all_matches.iter().filter(|m| !m.is_context).count();
    let mut text = String::new();
    for m in &all_matches {
        if m.is_context {
            text.push_str(&format!("{}-{}-{}\n", m.path, m.line, m.content));
        } else {
            text.push_str(&format!("{}:{}:{}\n", m.path, m.line, m.content));
        }
    }
    if truncated {
        text.push_str(&format!(
            "... truncated at {max_results} matches (narrow the pattern, add glob filters, or set a higher max_results)\n"
        ));
    }
    if text.is_empty() {
        text = "(no matches)\n".to_string();
    }

    ToolResult::ok_data(
        serde_json::json!({
            "status": "ok",
            "matches": all_matches
                .iter()
                .filter(|m| !m.is_context)
                .map(|m| serde_json::json!({"path": m.path, "line": m.line, "content": m.content}))
                .collect::<Vec<_>>(),
            "truncated": truncated,
            "count": match_count,
        }),
        text,
    )
}

handler!(handle_grep, exec_grep);

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "grep".to_string(),
            description: "Search file CONTENTS using the ripgrep engine (in-process, no external \
                binary). Returns 'path:line:content' matches (relative paths, context lines as \
                'path-line-content'), workspace-bounded, gitignored/hidden/binary files skipped \
                by default. Pattern is a regex (rg syntax: 'fn main', 'TODO|FIXME', \
                '\\bfn\\s+\\w+'). Case-insensitive by default (set case_sensitive=true for exact \
                case). Use 'glob' to restrict to file patterns (e.g. [\"*.rs\", \"!**/tests/**\"]). \
                Results are capped at max_results (default 200) with a truncated marker — narrow \
                the pattern rather than raising it. For file NAME listing use glob; for arbitrary \
                commands use exec.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern (rg syntax)"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Directories/files to search (relative to workspace root; default: whole workspace)"},
                    "glob": {"type": "array", "items": {"type": "string"}, "description": "File filters, e.g. [\"*.rs\", \"!**/tests/**\"] (rg -g syntax)"},
                    "case_sensitive": {"type": "boolean", "default": false, "description": "Match case exactly (default: case-insensitive)"},
                    "context_before": {"type": "integer", "minimum": 0, "description": "Lines of context before each match"},
                    "context_after": {"type": "integer", "minimum": 0, "description": "Lines of context after each match"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Max matches to return (default 200; overflow flagged truncated)"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            handler: handle_grep,
            risk: ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(60),
        },
        crate::ToolPlacement::Workspace,
    );
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    /// 写 CURRENT_WORKSPACE 的测试必须串行（全局静态，并行测试会互相踩踏）。
    static WS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        let ws = dir.path().to_string_lossy().to_string();
        (dir, ws)
    }

    fn run(ws: &str, args: serde_json::Value) -> serde_json::Value {
        // WS_LOCK 串行 grep_tool 内部；TEST_RUNTIME_SERIAL 与改写
        // CURRENT_WORKSPACE 的其他模块测试（backend/workspace/authorization 等）
        // 互斥——缺了它全量并行跑时会搜到真实仓库。
        let _guard = WS_LOCK.lock().unwrap();
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string());
        let result = exec_grep(&args);
        let data = result.data.clone();
        if data.as_object().is_none_or(|o| o.is_empty()) {
            let raw = result.model_text();
            let mut v = serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_default();
            if v.get("code").is_none() {
                v["status"] =
                    serde_json::json!(if matches!(result.status, crate::ToolStatus::Ok) {
                        "ok"
                    } else {
                        "error"
                    });
                v["raw"] = serde_json::json!(raw);
            }
            v
        } else {
            data
        }
    }

    #[test]
    fn basic_search_returns_path_line_content() {
        let (_dir, ws) = setup(&[
            ("src/a.rs", "fn alpha() {}\nlet x = 1;\n"),
            ("src/b.rs", "fn beta() {}\n"),
            ("README.md", "no code here\n"),
        ]);
        let out = run(&ws, serde_json::json!({ "pattern": "fn \\w+" }));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["count"], 2);
        let m = &out["matches"][0];
        assert_eq!(m["line"], 1);
        assert!(m["content"].as_str().unwrap().contains("fn "));
    }

    #[test]
    fn case_insensitive_by_default_sensitive_when_requested() {
        let (_dir, ws) = setup(&[("f.txt", "Hello\nworld\n")]);
        let out = run(&ws, serde_json::json!({ "pattern": "hello" }));
        assert_eq!(out["count"], 1, "got: {out}");
        let out2 = run(
            &ws,
            serde_json::json!({ "pattern": "hello", "case_sensitive": true }),
        );
        assert_eq!(out2["count"], 0, "got: {out2}");
    }

    #[test]
    fn glob_filters_files() {
        let (_dir, ws) = setup(&[("src/a.rs", "target\n"), ("src/a.md", "target\n")]);
        let out = run(
            &ws,
            serde_json::json!({ "pattern": "target", "glob": ["*.rs"] }),
        );
        assert_eq!(out["count"], 1, "got: {out}");
        assert!(
            out["matches"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("a.rs")
        );
    }

    #[test]
    fn max_results_truncates_with_marker() {
        let files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("f{i:02}.txt"), format!("hit line {i}\n")))
            .collect();
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        let (_dir, ws) = setup(&file_refs);
        let out = run(
            &ws,
            serde_json::json!({ "pattern": "hit", "max_results": 5 }),
        );
        assert_eq!(out["count"], 5);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn no_match_is_ok_with_zero_count() {
        let (_dir, ws) = setup(&[("f.txt", "abc\n")]);
        let out = run(&ws, serde_json::json!({ "pattern": "zzz" }));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["count"], 0);
    }

    #[test]
    fn invalid_regex_reports_error() {
        let (_dir, ws) = setup(&[("f.txt", "abc\n")]);
        let out = run(&ws, serde_json::json!({ "pattern": "(" }));
        assert_eq!(out["status"], "error", "got: {out}");
        assert!(
            out["raw"]
                .as_str()
                .is_some_and(|s| s.contains("invalid regex")),
            "got: {out}"
        );
    }

    #[test]
    fn context_lines_are_included() {
        let (_dir, ws) = setup(&[("f.txt", "a\nTARGET\nc\n")]);
        let out = run(
            &ws,
            serde_json::json!({ "pattern": "TARGET", "context_before": 1, "context_after": 1 }),
        );
        assert_eq!(out["count"], 1);
        assert_eq!(out["matches"][0]["line"], 2);
    }

    #[test]
    fn path_outside_workspace_rejected() {
        let (_dir, ws) = setup(&[("f.txt", "abc\n")]);
        let out = run(
            &ws,
            serde_json::json!({ "pattern": "abc", "paths": ["../../.."] }),
        );
        assert_eq!(out["status"], "error", "got: {out}");
        assert!(
            out["raw"].as_str().is_some_and(|s| s.contains("workspace")),
            "got: {out}"
        );
    }

    #[test]
    fn gitignored_files_are_skipped() {
        let (_dir, ws) = setup(&[
            ("keep.txt", "needle\n"),
            (".gitignore", "skip.txt\n"),
            ("skip.txt", "needle\n"),
        ]);
        let out = run(&ws, serde_json::json!({ "pattern": "needle" }));
        assert_eq!(out["count"], 1, "got: {out}");
        assert!(
            out["matches"][0]["path"]
                .as_str()
                .unwrap()
                .contains("keep.txt")
        );
    }
}
