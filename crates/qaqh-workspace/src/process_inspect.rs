//! Process inspection tools — check, wait, kill, write for tracked processes.
//!
//! Registered under the `process` name with an explicit action.
//! These let the LLM inspect long-running exec/subagent processes that
//! hit their timeout, instead of blindly retrying or killing.

use crate::{ToolCallCtx, ToolPlacement, ToolResult, ToolRisk, process_registry::ProcessRegistry};

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(crate::ToolHandler {
        key: "process".into(),
        description: "Inspect and control tracked background processes with one action-based interface: check, wait, write, or kill.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["check", "wait", "write", "kill"],
                    "description": "check: query status, output tail and metadata of a tracked process; wait: block until the process exits (or timeout_secs elapses) and return its current state; write: send text to the process stdin; kill: terminate the process tree."
                },
                "id": {
                    "type": "integer",
                    "description": "Process id returned by exec when a command was backgrounded (status \\\"backgrounded\\\" + process_id)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 3600,
                    "description": "Max seconds to wait for action=wait. Default 120. Ignored by check/write/kill."
                },
                "text": {
                    "type": "string",
                    "description": "Text to write to the process stdin (action=write only; newline is NOT appended automatically)."
                }
            },
            "required": ["action", "id"],
            "additionalProperties": false
        }),
        handler: handle_process,
        risk: ToolRisk::Administrative,
        category: crate::permission::ToolCategory::Exec,
        default_timeout: std::time::Duration::from_secs(180),
    }, ToolPlacement::Workspace);
}

fn handle_process(ctx: ToolCallCtx) -> ToolResult {
    match ctx.args.get("action").and_then(|value| value.as_str()) {
        Some("check") => handle_check(ctx),
        Some("wait") => handle_wait(ctx),
        Some("write") => handle_write(ctx),
        Some("kill") => handle_kill(ctx),
        _ => ToolResult::error("process.action must be check, wait, write, or kill"),
    }
}

/// Format a process info payload as a flat structured response.
///
/// `ProcessRegistry::get_info` already returns a structured JSON object
/// (id/name/status/exit_code/output…). Serializing it into a `content` string
/// would double-escape it (JSON inside a JSON string), so we keep the
/// fields in the model payload and add a short human-readable summary
/// that the context-fold logic can later replace with a hint.
fn process_info_ok(id: u32, info: serde_json::Value) -> String {
    let mut v = info;
    if let serde_json::Value::Object(ref mut map) = v {
        map.insert("timeis".to_string(), serde_json::json!(crate::now_utc8()));
        if !map.contains_key("content") {
            let status = map.get("status").and_then(|s| s.as_str()).unwrap_or("");
            map.insert(
                "content".to_string(),
                serde_json::json!(format!("process {id}: {status}")),
            );
        }
    }
    v.to_string()
}

fn process_error(code: &str, message: impl Into<String>, hint: &str) -> ToolResult {
    ToolResult::error(crate::json_err(code, message, hint))
}

fn process_ok(payload: String) -> ToolResult {
    ToolResult::ok(payload)
}

fn process_id(ctx: &ToolCallCtx, operation: &str) -> Result<u32, ToolResult> {
    match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => Ok(v as u32),
        _ => Err(process_error(
            "MISSING_ID",
            format!("{operation}: id required"),
            "Provide the process ID returned by exec.",
        )),
    }
}

fn handle_check(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.check") {
        Ok(id) => id,
        Err(result) => return result,
    };

    // 刷新终态：子进程已退出则立即反映（孙进程持管道时 EOF 不达，
    // 原实现状态停在 running，模型会误以为任务未结束）。
    let _ = ProcessRegistry::try_wait(id);

    match ProcessRegistry::get_info(id) {
        Some(info) => process_ok(process_info_ok(id, info)),
        None => process_error(
            "NOT_FOUND",
            format!("process.check: process {id} not found"),
            "Process may have already exited and been cleaned up.",
        ),
    }
}

fn handle_wait(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.wait") {
        Ok(id) => id,
        Err(result) => return result,
    };
    let timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    match ProcessRegistry::wait_for(id, timeout_secs) {
        Some(info) => process_ok(process_info_ok(id, info)),
        None => process_error(
            "NOT_FOUND",
            format!("process.wait: process {id} not found"),
            "Check that the process ID is correct.",
        ),
    }
}

fn handle_kill(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.kill") {
        Ok(id) => id,
        Err(result) => return result,
    };

    if ProcessRegistry::kill(id) {
        process_ok(crate::json_ok(
            serde_json::json!({"content": format!("Process {id} killed.")}),
        ))
    } else {
        process_error(
            "NOT_FOUND",
            format!("process.kill: process {id} not found or already exited"),
            "Check the process ID.",
        )
    }
}

fn handle_write(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.write") {
        Ok(id) => id,
        Err(result) => return result,
    };
    let text = match ctx.args.get("text").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return process_error(
                "MISSING_TEXT",
                "process.write: text required",
                "Provide the text to write to stdin.",
            );
        }
    };

    match ProcessRegistry::write_to(id, text) {
        Ok(n) => process_ok(crate::json_ok(
            serde_json::json!({"content": format!("Wrote {n} bytes to process {id}.")}),
        )),
        Err(e) => process_error(
            "WRITE_FAILED",
            format!("process write: {e}"),
            "Check that the process is still running.",
        ),
    }
}
