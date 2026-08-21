//! `confirm_apply`：dry-run 后的内存直提。
//!
//! 写工具（edit / apply_patch / write）以 `dry_run=true` 通过验证时
//! 返回 `pending_id`（参数已暂存在 `pending` 注册表）。模型向用户确认后调
//! 本工具：**从注册表取出参数重放执行路径**——模型不需要重新输出 patch /
//! hunks / content（消除二次输出）。
//!
//! - `action=apply`：重放 → 落盘（各工具的 expected_hash 校验拦截 dry-run
//!   之后发生的外部改动；内容匹配工具天然防漂移）。
//! - `action=discard`：丢弃 pending，不落盘。
//! - pending 一次性（apply 或 discard 都消费）；过期（30 分钟）或不存在 →
//!   `PENDING_NOT_FOUND_OR_EXPIRED`。

use crate::{ToolHandler, ToolResult, ToolRisk};

fn exec_confirm_apply(args: &serde_json::Value) -> ToolResult {
    let pending_id = match args
        .get("pending_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(id) => id,
        None => {
            return crate::ToolResult::error(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "MISSING_PENDING_ID",
                "message": "confirm_apply requires 'pending_id' (returned by a dry_run of edit / apply_patch / write)",
            })
            .to_string());
        }
    };
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("apply");

    let Some(p) = crate::pending::take(pending_id) else {
        return crate::ToolResult::error(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "PENDING_NOT_FOUND_OR_EXPIRED",
            "message": format!("pending {pending_id} not found or expired (30 min window; one-shot)"),
            "hint": "Re-run the write tool with dry_run=true to get a fresh pending_id.",
        })
        .to_string());
    };

    match action {
        "apply" => match p.tool_name.as_str() {
            "edit" => crate::file_edit_v2::exec_edit(&p.args),
            "write" => crate::file_mutate::exec_write_file(&p.args),
            "apply_patch" => crate::apply_patch::exec_apply_patch(&p.args),
            other => crate::ToolResult::error(
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "UNKNOWN_PENDING_TOOL",
                    "message": format!("pending {pending_id} holds unknown tool '{other}'"),
                })
                .to_string(),
            ),
        },
        "discard" => crate::ToolResult::ok(format!(
            "[OK] confirm_apply — pending {pending_id} discarded, no changes written\n"
        )),
        other => crate::ToolResult::error(
            serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "INVALID_ACTION",
                "message": format!("invalid action {other:?} — use \"apply\" or \"discard\""),
            })
            .to_string(),
        ),
    }
}

fn handle_confirm_apply(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_confirm_apply(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "confirm_apply".to_string(),
            description: concat!(
                "Commit or discard a pending dry-run. ",
                "edit / apply_patch / write called with dry_run=true return a pending_id when the dry-run passes. ",
                "After asking the user for confirmation, call confirm_apply with that pending_id and action=\"apply\" to commit ",
                "— the engine replays the stored parameters, so you do NOT re-send the hunks/patch/content. ",
                "action=\"discard\" drops the pending without writing. ",
                "One-shot: each pending_id works once; expires after 30 minutes. ",
                "If the file changed since the dry-run, the commit is rejected (re-run the dry-run)."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pending_id": {"type": "string", "description": "pending_id returned by a dry_run of edit / apply_patch / write"},
                    "action": {"type": "string", "enum": ["apply", "discard"], "default": "apply", "description": "apply = commit the pending change; discard = drop it"}
                },
                "required": ["pending_id"],
                "additionalProperties": false
            }),
            handler: handle_confirm_apply,
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

    /// 写 CURRENT_WORKSPACE 的测试必须串行（全局静态，并行测试会互相踩踏）。
    static WS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_confirm(id: &str, action: &str) -> serde_json::Value {
        let result = exec_confirm_apply(&serde_json::json!({
            "pending_id": id,
            "action": action,
        }));
        let data = result.data.clone();
        if data.as_object().is_none_or(|o| o.is_empty()) {
            // 无结构化 data → 尝试解析错误 JSON；失败则按状态兜底。
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

    fn dry_run_v2(
        path: &str,
        old: &str,
        new: &str,
    ) -> (tempfile::TempDir, String, serde_json::Value) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), old).unwrap();
        // 用绝对路径：resolve_workspace_path 对绝对路径直接返回，
        // 避免并行测试踩踏全局 CURRENT_WORKSPACE。
        let ws = dir.path().to_string_lossy().to_string();
        let result = crate::file_edit_v2::exec_edit(&serde_json::json!({
            "path": format!("{}/f.txt", ws.replace('\\', "/")),
            "dry_run": true,
            "hunks": [{"kind": "replace", "old": old, "new": new}],
        }));
        let data = result.data.clone();
        (dir, ws, data)
    }

    #[test]
    fn v2_dry_run_then_confirm_applies_without_resending_hunks() {
        let (dir, _ws, data) = dry_run_v2("f.txt", "a\nb\nc\n", "A\nb\nc\n");
        assert_eq!(data["status"], "ok", "dry run failed: {data}");
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();

        // 文件未写
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );

        // 确认 → 落盘（重放参数，模型无需重发 hunks）
        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["status"], "ok", "confirm failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "A\nb\nc\n"
        );

        // 一次性：再确认 → 过期/不存在
        let out2 = run_confirm(&pending_id, "apply");
        assert_eq!(out2["code"], "PENDING_NOT_FOUND_OR_EXPIRED");
    }

    #[test]
    fn discard_drops_pending_without_writing() {
        let (dir, _ws, data) = dry_run_v2("f.txt", "x\ny\n", "X\ny\n");
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();

        let out = run_confirm(&pending_id, "discard");
        assert_eq!(out["status"], "ok", "discard failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x\ny\n"
        );
        // 已消费
        assert_eq!(
            run_confirm(&pending_id, "apply")["code"],
            "PENDING_NOT_FOUND_OR_EXPIRED"
        );
    }

    #[test]
    fn confirm_rejected_when_file_changed_after_dry_run() {
        let (dir, _ws, data) = dry_run_v2("f.txt", "a\nb\n", "A\nb\n");
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();

        // dry-run 后文件被外部改动 → 重放时 hash gate 拒绝
        std::fs::write(dir.path().join("f.txt"), "a\nCHANGED\n").unwrap();
        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["code"], "HASH_MISMATCH", "got: {out}");
        // 文件保持外部改动，未被覆盖
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\nCHANGED\n"
        );
    }

    #[test]
    fn write_dry_run_then_confirm() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("w.txt"), "old\n").unwrap();
        let ws = dir.path().to_string_lossy().to_string();
        let wpath = format!("{}/w.txt", ws.replace('\\', "/"));

        let dry = crate::file_mutate::exec_write_file(&serde_json::json!({
            "path": wpath,
            "content": "new content\n",
            "dry_run": true,
        }));
        let data = dry.data.clone();
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("w.txt")).unwrap(),
            "old\n"
        );

        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["status"], "ok", "confirm write failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("w.txt")).unwrap(),
            "new content\n"
        );
    }

    #[test]
    fn apply_patch_dry_run_then_confirm() {
        let _guard = WS_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "line1\nline2\n").unwrap();
        let ws = dir.path().to_string_lossy().to_string();
        // 引擎的 workspace-bounded 检查要求绝对路径落在 cwd 内：
        // cwd = CURRENT_WORKSPACE（并行测试可能被踩踏）→ 用 cwd 无关的
        // 方式不可行，因此这里直接设置全局并立即 confirm（串行窗口内安全）。
        let apath = format!("{}/a.txt", ws.replace('\\', "/"));
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws);
        let patch = format!(
            "*** Begin Patch\n*** Update File: {apath}\n@@\n-line2\n+LINE2\n*** End Patch\n"
        );
        let dry = crate::apply_patch::exec_apply_patch(&serde_json::json!({
            "patch": patch,
            "dry_run": true,
        }));
        let data = dry.data.clone();
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );

        // 并行测试可能覆盖 CURRENT_WORKSPACE（Linux 调度差异放大竞争）：
        // confirm 重放前重新钉住本测试的 workspace。
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws);
        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["status"], "ok", "confirm patch failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nLINE2\n"
        );
    }

    #[test]
    fn missing_pending_id_is_error() {
        let out = run_confirm("", "apply");
        assert_eq!(out["code"], "MISSING_PENDING_ID");
    }
}
