//! 独立补丁工具 `apply_patch`：**Codex 格式补丁**（`*** Begin Patch`）。
//!
//! 自研内容匹配引擎（`apply_patch_engine`，移植自 OpenAI codex-rs，
//! Apache-2.0）：**无行号**、四级匹配（精确→去尾空白→trim→Unicode 归一化）、
//! `@@` 上下文锚定、`*** End of File` 文件尾锚定。模型手写友好。
//!
//! 事务语义：**按序应用**——任一 hunk 失败即停，已写入的文件保留（与上游
//! Codex 一致）；失败前先 `dry_run=true` 可预检全部 hunk。路径相对 workspace
//! 根解析，`..` 逃逸/外部绝对路径被拒绝。
//!
//! 与 `edit` 分工：edit 是结构化精确定位（内容锚定、严格拒绝歧义），
//! 适合模型逐步编辑；apply_patch 适合批量补丁合入（模型输出 patch → 校验 → 合入），
//! 失败时整个 patch 需修正重发。

use crate::{ToolHandler, ToolResult, ToolRisk};

fn workspace_root() -> String {
    let ws = crate::current_workspace();
    if ws.is_empty() { ".".to_string() } else { ws }
}

/// 执行 apply_patch：patch（必填，`*** Begin Patch` Codex 格式）+ dry_run。
pub(super) fn exec_apply_patch(args: &serde_json::Value) -> ToolResult {
    let patch = match args
        .get("patch")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(p) => p,
        None => {
            return crate::ToolResult::error(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "PARSE_ERROR",
                "message": "apply_patch: missing 'patch'",
                "hint": "Provide a Codex-format patch: '*** Begin Patch' ... '*** End Patch' (see the tool description for the format).",
            }).to_string());
        }
    };
    let dry_run = args
        .get("dry_run")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let ws = workspace_root();
    let mut result = exec_engine_patch(&ws, patch, dry_run);
    // dry-run 通过 → 暂存参数供 confirm_apply 内存直提（模型无需重发 patch）。
    if dry_run && result.status == crate::ToolStatus::Ok {
        let mut pending_args = args.clone();
        if let Some(obj) = pending_args.as_object_mut() {
            obj.remove("dry_run");
        }
        let pending_id = crate::pending::store("apply_patch", &pending_args);
        result.data["pending_id"] = serde_json::json!(pending_id);
        result.data["dry_run"] = serde_json::json!(true);
        result.summary.push_str(&format!(
            "pending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}\n"
        ));
    }
    result
}

/// 内容匹配引擎：无行号、四级匹配、按序应用（任一 hunk 失败即停，已写入
/// 的文件保留——与上游 Codex 一致；失败前先 `dry_run=true` 预检全部 hunk）。
fn exec_engine_patch(ws: &str, patch: &str, dry_run: bool) -> ToolResult {
    use crate::apply_patch_engine::{EngineError, apply_patch_engine, dry_run_patch_engine};

    let outcome = if dry_run {
        dry_run_patch_engine(patch, std::path::Path::new(ws))
    } else {
        apply_patch_engine(patch, std::path::Path::new(ws), Default::default())
    };

    match outcome {
        Ok(outcome) => {
            let mut ins = 0usize;
            let mut del = 0usize;
            for d in &outcome.deltas {
                match (&d.old, &d.new) {
                    (None, Some(new)) => ins += new.lines().count(),
                    (Some(old), None) => del += old.lines().count(),
                    (Some(old), Some(new)) => {
                        let (a, r, _) = crate::file_shared::diff_stats_between(old, new);
                        ins += a as usize;
                        del += r as usize;
                    }
                    (None, None) => {}
                }
                // 账本同步：引擎直接写盘，touched 文件的最新内容登记进
                // file_state，否则后续 edit 盲定位防漂移会误报。
                if !dry_run {
                    if let Some(new) = &d.new {
                        crate::file_state::record_write(&d.path, new);
                    }
                    let op = if d.old.is_none() {
                        "add"
                    } else if d.new.is_none() {
                        "delete"
                    } else {
                        "update"
                    };
                    crate::journal::record_change(
                        &crate::journal::active_session(),
                        "",
                        "apply_patch",
                        &d.path,
                        op,
                        d.old.as_deref(),
                        d.new.as_deref(),
                        "ok",
                    );
                }
            }
            let n = outcome.affected.added.len()
                + outcome.affected.modified.len()
                + outcome.affected.deleted.len();
            let text = if dry_run {
                format!(
                    "[DRY RUN] apply_patch — patch parses: {n} file(s), +{ins} -{del}; engine pre-checked every hunk against current file contents (a real apply may still differ)\n"
                )
            } else {
                format!("[OK] apply_patch — applied: {n} file(s), +{ins} -{del}\n")
            };
            let mut data = serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "format": "codex",
                "dry_run": dry_run,
                "files": n,
                "insertions": ins,
                "deletions": del,
                "added": outcome.affected.added,
                "modified": outcome.affected.modified,
                "deleted": outcome.affected.deleted,
            });
            if dry_run {
                data["touched"] = serde_json::Value::Array(
                    outcome
                        .deltas
                        .iter()
                        .map(|d| serde_json::Value::String(d.path.clone()))
                        .collect(),
                );
            }
            crate::ToolResult::ok_data(data, text)
        }
        Err(e) => {
            let (code, hint) = match &e {
                EngineError::Parse(_) => (
                    "PARSE_ERROR",
                    "The patch does not follow the Codex apply-patch format: start with '*** Begin Patch', end with '*** End Patch'; hunk lines start with '+' (add), '-' (remove), ' ' (context); '@@' starts a chunk (optionally with a context line).",
                ),
                EngineError::Compute(_) => (
                    "NO_MATCH",
                    "The engine could not find the expected lines in the target file (4-tier matching: exact → trailing-whitespace → trimmed → Unicode-normalised). Check the 'old' lines against the file; use '@@ <context>' to anchor the chunk, or '*** End of File' for end-of-file hunks. Re-send the FULL corrected patch — no partial application happened.",
                ),
                EngineError::EmptyPatch => (
                    "EMPTY_PATCH",
                    "The patch parsed to zero hunks; at least one '*** Add File: / *** Delete File: / *** Update File:' section is required.",
                ),
                EngineError::Io { .. } => (
                    "IO_ERROR",
                    "A filesystem operation failed (read/write/remove).",
                ),
                EngineError::PathOutsideWorkspace { .. } => (
                    "PATH_OUTSIDE_WORKSPACE",
                    "Every patch path must resolve inside the workspace root; '..' escapes and absolute paths outside the workspace are rejected.",
                ),
            };
            crate::ToolResult::error(
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": code,
                    "message": e.to_string(),
                    "hint": hint,
                })
                .to_string(),
            )
        }
    }
}

fn handle_apply_patch(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_apply_patch(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "apply_patch".to_string(),
        description: concat!(
            "Apply a Codex-format patch (the '*** Begin Patch' format) to the workspace repo. ",
            "Structure: '*** Begin Patch' … '*** End Patch' with '*** Add File: <path>' / '*** Delete File: <path>' / '*** Update File: <path>' sections; hunk lines start with '+' (add), '-' (remove), ' ' (context); '@@ <context>' starts a chunk (context anchors the match); '*** Move to: <path>' renames; '*** End of File' anchors a chunk at EOF. ",
            "NO line numbers: matching is content-based with 4 tiers (exact → trailing-whitespace-insensitive → trimmed → Unicode-normalised typographic punctuation). ",
            "Hunks apply in order; when one fails, files already written stay, so re-send a corrected FULL patch for a clean result (or dry_run=true first to pre-check every hunk). ",
            "Example:\n```\n*** Begin Patch\n*** Update File: src/a.rs\n@@ fn main\n-let old = 1;\n+let old = 2;\n*** End Patch\n```\n",
            "For structured single-file edits use edit."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "Codex-format patch text: '*** Begin Patch' ... '*** End Patch'; '+'/'-'/' ' prefixed hunk lines; '@@ <context>' chunk anchors; '*** Add/Delete/Update File:' sections. Multiple files allowed in one patch."},
                "dry_run": {"type": "boolean", "description": "Pre-check parsing + every hunk against current file contents; do not apply", "default": false}
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        handler: handle_apply_patch,
        risk: ToolRisk::Write,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(60),
    },
    crate::ToolPlacement::Workspace,
);
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use std::path::Path;

    /// 建一个带初始 commit 的临时 git 仓库，返回 (tempdir, workspace path)。
    fn repo_with_commit(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temp repo");
        let repo = Repository::init(dir.path()).expect("init repo");
        let mut index = repo.index().expect("index");
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).expect("write fixture");
            index.add_path(Path::new(name)).expect("stage fixture");
        }
        // 必须 write() 落盘：libgit2 的 commit 不会像 git CLI 那样自动刷新 index
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature =
            git2::Signature::now("QAQ-Harness Test", "qaqh-test@local").expect("signature");
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("initial commit");
        let ws = dir.path().to_str().unwrap().to_string();
        (dir, ws)
    }

    fn run_in(ws: &str, patch: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut args = serde_json::json!({ "patch": patch });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                args[k] = v.clone();
            }
        }
        // 测试直接注入 workspace（避免依赖全局 CURRENT_WORKSPACE）
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string());
        let result = exec_apply_patch(&args);
        let data = result.data.clone();
        if data.as_object().is_none_or(|o| o.is_empty()) {
            let raw = result.model_text();
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) if v.is_object() => v,
                _ => serde_json::json!({ "status": "error", "raw": raw }),
            }
        } else {
            data
        }
    }

    #[test]
    fn codex_format_routes_to_engine_and_writes() {
        // `*** Begin Patch` 格式走自研内容匹配引擎：无行号、空白容错。
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\nline3\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-line2
+LINE2
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["format"], "codex");
        assert_eq!(out["files"], 1);
        assert_eq!(out["insertions"], 1);
        assert_eq!(out["deletions"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "line1\nLINE2\nline3\n"
        );
    }

    #[test]
    fn codex_format_dry_run_prechecks_without_writing() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-line2
++LINE2
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({ "dry_run": true }));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["files"], 1);
        assert!(out["touched"][0].as_str().unwrap().ends_with("a.txt"));
        // 文件未被修改
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );
    }

    #[test]
    fn codex_format_no_match_rejects_with_zero_changes() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-never-exists
++NOPE
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "error", "got: {out}");
        assert_eq!(out["code"], "NO_MATCH");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );
    }
}
