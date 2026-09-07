//! todo::dispatch — 工具分发与注册（handle_todo + register）。

use serde_json::Value;

use crate::{ToolCallCtx, ToolResult, json_err_string};

use super::actions::{exec_todo_create, exec_todo_list, exec_todo_set};

// ═══════════════════════════════════════════════════════
// Dispatcher and registration
// ═══════════════════════════════════════════════════════

fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => ToolResult::error(content),
    }
}

pub(crate) fn reject_fields(args: &Value, fields: &[&str], action: &str) -> Result<(), String> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| args.get(*field).is_some())
        .collect();
    if present.is_empty() {
        Ok(())
    } else {
        Err(json_err_string(
            "INVALID_INPUT",
            format!("action={action} does not accept: {}", present.join(", ")),
            "Follow the action-specific Todo V2 schema.",
        ))
    }
}

pub(crate) fn handle_todo(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx
        .args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match action {
        "create" => reject_fields(
            &ctx.args,
            &[
                "id",
                "status",
                "evidence",
                "after_id",
                "before_id",
                "ids",
                "updates",
            ],
            action,
        )
        .and_then(|_| exec_todo_create(&ctx.args, false)),
        "insert" => reject_fields(
            &ctx.args,
            &["id", "status", "evidence", "ids", "updates"],
            action,
        )
        .and_then(|_| exec_todo_create(&ctx.args, true)),
        "set" => reject_fields(
            &ctx.args,
            &["title", "description", "items", "after_id", "before_id"],
            action,
        )
        .and_then(|_| exec_todo_set(&ctx.args)),
        "list" => reject_fields(
            &ctx.args,
            &[
                "title",
                "description",
                "items",
                "id",
                "evidence",
                "after_id",
                "before_id",
                "ids",
                "updates",
            ],
            action,
        )
        .and_then(|_| exec_todo_list(&ctx.args)),
        _ => Err(json_err_string(
            "INVALID_INPUT",
            "todo.action must be create, insert, set, or list",
            "",
        )),
    };
    tool_result(result)
}

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "todo".to_string(),
            description: "Task list (session-scoped, T1..). IDs auto-assigned & stable, never renumbered; blocked in plan mode. create/insert(after_id,before_id) add tasks (bulk ≤20). set: batch same status via ids:[\"T1\",\"T4-T6\"]+status; per-item {id,status?,evidence?,title?,description?} via updates (edit title/description mid-task there). list(status?) inspects. Provide evidence when marking completed. (Deprecated: prefer the split tools todo_create/todo_insert/todo_set/todo_list.)",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "insert", "set", "list"],
                        "description": "create/insert/set/list 之一"
                    },
                    "title": {
                        "type": "string",
                        "description": "Task title (1-100)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Context/acceptance (<=200)"
                    },
                    "items": {
                        "type": "array",
                        "maxItems": 20,
                        "description": "Bulk create/insert", 
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string", "description": "任务标题"},
                                "description": {"type": "string", "description": "可选上下文"}
                            },
                            "required": ["title"],
                            "additionalProperties": false
                        }
                    },
                    "id": {
                        "type": ["string", "integer"],
                        "description": "Target ID (T1); Omit for action=create"
                    },
                    "ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Batch same-status IDs/range (T1,T1-T3)"
                    },
                    "updates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": ["string", "integer"], "description": "目标任务 ID（如 T1）"},
                                "status": {
                                    "type": "string",
                                    "enum": ["idle", "in_progress", "completed", "cancelled"],
                                    "description": "目标状态；仅编辑 title/description 时可省略"
                                },
                                "evidence": {"type": "string", "description": "完成摘要（可选，非空字符串）"},
                                "title": {"type": "string", "description": "新标题（1-100 字，可选；中途修改任务描述）"},
                                "description": {"type": "string", "description": "新描述/验收（≤200 字，可选；传空串清空）"}
                            },
                            "required": ["id"],
                            "additionalProperties": false
                        },
                        "description": "Per-item edit: {id, status?, evidence?, title?, description?} — status 可批量同状态用 ids 表达式"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["idle", "in_progress", "completed", "cancelled"],
                        "description": "Target status"
                    },
                    "evidence": {
                        "type": "string",
                        "description": "Evidence (optional)"
                    },
                    "after_id": {
                        "type": ["string", "integer"],
                        "description": "Insert after ID"
                    },
                    "before_id": {
                        "type": ["string", "integer"],
                        "description": "Insert before ID"
                    }
                },
                "required": ["action"],
                "additionalProperties": false,
                "oneOf": [
                    {"title": "Create", "properties": {"action": {"const": "create"}}, "required": ["action"]},
                    {"title": "Insert", "properties": {"action": {"const": "insert"}}, "required": ["action"]},
                    {"title": "Set", "properties": {"action": {"const": "set"}}, "required": ["action"]},
                    {"title": "List", "properties": {"action": {"const": "list"}}, "required": ["action"]}
                ]
            }),
            handler: handle_todo,
            risk: ToolRisk::Write,
            category: crate::permission::ToolCategory::Write,
            default_timeout: Duration::from_secs(15),
        },
        crate::ToolPlacement::Workspace,
    );
}

// ═══════════════════════════════════════════════════════
// Tests
