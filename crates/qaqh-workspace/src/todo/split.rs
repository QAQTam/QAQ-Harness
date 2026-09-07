//! todo::split — W1 聚合工具拆分（PR-DT-1，见 docs/dynamic-tools-design.md §5.1）。
//!
//! 单一职责四分：`todo_create` / `todo_insert` / `todo_set` / `todo_list`。
//! 每个薄壳 handler 与聚合 dispatch 对应 action 的字段校验**同表**
//! （`reject_fields` 复用），写路径继续复用 `exec_*`（单一事实源不变）。
//!
//! 迁移策略（软迁移）：旧聚合 `todo` 保留一版（description 标 deprecated）；
//! 稳定后删除聚合，本文件成为唯一注册点。历史安全：旧会话的
//! `tool_call(name="todo")` 只存在于历史/审计，无重放执行路径。

use std::time::Duration;

use serde_json::Value;

use crate::permission::ToolCategory;
use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};

use super::actions::{exec_todo_create, exec_todo_list, exec_todo_set};
use super::dispatch::reject_fields;

// ═══════════════════════════════════════════════════════
// Handlers（字段校验表与聚合 dispatch.rs 各 action 完全一致）
// ═══════════════════════════════════════════════════════

fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => ToolResult::error(content),
    }
}

pub fn handle_create(ctx: ToolCallCtx) -> ToolResult {
    let result = reject_fields(
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
        "todo_create",
    )
    .and_then(|_| exec_todo_create(&ctx.args, false));
    tool_result(result)
}

pub fn handle_insert(ctx: ToolCallCtx) -> ToolResult {
    let result = reject_fields(
        &ctx.args,
        &["id", "status", "evidence", "ids", "updates"],
        "todo_insert",
    )
    .and_then(|_| exec_todo_create(&ctx.args, true));
    tool_result(result)
}

pub fn handle_set(ctx: ToolCallCtx) -> ToolResult {
    let result = reject_fields(
        &ctx.args,
        &["title", "description", "items", "after_id", "before_id"],
        "todo_set",
    )
    .and_then(|_| exec_todo_set(&ctx.args));
    tool_result(result)
}

pub fn handle_list(ctx: ToolCallCtx) -> ToolResult {
    let result = reject_fields(
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
        "todo_list",
    )
    .and_then(|_| exec_todo_list(&ctx.args));
    tool_result(result)
}

// ═══════════════════════════════════════════════════════
// Schemas（单一职责：无 oneOf、无参数归属说明文字）
// ═══════════════════════════════════════════════════════

/// 批量条目 schema（create/insert 共用；json! 宏按值内插）。
fn bulk_items_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "maxItems": 20,
        "description": "Bulk form (overrides title/description).",
        "items": {
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Task title"},
                "description": {"type": "string", "description": "Optional context"}
            },
            "required": ["title"],
            "additionalProperties": false
        }
    })
}

fn single_task_properties() -> Value {
    serde_json::json!({
        "title": {"type": "string", "description": "Task title (1-100). Single-task form: omit items."},
        "description": {"type": "string", "description": "Context/acceptance (<=200)"}
    })
}

fn todo_create_schema() -> Value {
    let single = single_task_properties();
    let items = bulk_items_schema();
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": single["title"],
            "description": single["description"],
            "items": items
        },
        "additionalProperties": false
    })
}

fn todo_insert_schema() -> Value {
    let single = single_task_properties();
    let items = bulk_items_schema();
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": single["title"],
            "description": single["description"],
            "items": items,
            "after_id": {"type": ["string", "integer"], "description": "Insert after this ID (e.g. T1). Provide exactly one of after_id/before_id."},
            "before_id": {"type": ["string", "integer"], "description": "Insert before this ID (e.g. T1). Provide exactly one of after_id/before_id."}
        },
        "additionalProperties": false
    })
}

fn todo_set_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {"type": ["string", "integer"], "description": "Single target (e.g. T1)."},
            "ids": {"type": "array", "items": {"type": "string"}, "description": "Batch same-status IDs/range (T1,T1-T3)."},
            "status": {"type": "string", "enum": ["idle", "in_progress", "completed", "cancelled"], "description": "Target status (required with id/ids)."},
            "evidence": {"type": "string", "description": "Completion summary (optional, single-id form)."},
            "updates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": ["string", "integer"], "description": "Target ID (e.g. T1)"},
                        "status": {
                            "type": "string",
                            "enum": ["idle", "in_progress", "completed", "cancelled"],
                            "description": "Omit for title/description-only edit"
                        },
                        "evidence": {"type": "string", "description": "Completion summary"},
                        "title": {"type": "string", "description": "New title (1-100)"},
                        "description": {"type": "string", "description": "New description (<=200; empty clears)"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                },
                "description": "Per-item edits."
            }
        },
        "additionalProperties": false
    })
}

fn todo_list_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["idle", "in_progress", "completed", "cancelled"], "description": "Optional filter."}
        },
        "additionalProperties": false
    })
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

/// 拆分工具注册项（key/description/schema/handler/risk/category）。
type SplitTool = (
    &'static str,
    &'static str,
    Value,
    fn(ToolCallCtx) -> ToolResult,
    ToolRisk,
    ToolCategory,
);

pub fn register(mgr: &mut crate::ToolManager) {
    let tools: [SplitTool; 4] = [
        (
            "todo_create",
            "Create session tasks (T1.. auto-assigned, stable; plan-mode blocked). Single via title/description, bulk <=20 via items.",
            todo_create_schema(),
            handle_create,
            ToolRisk::Write,
            ToolCategory::Write,
        ),
        (
            "todo_insert",
            "Insert session tasks at a position: exactly one of after_id/before_id (existing ID); bulk via items, single via title.",
            todo_insert_schema(),
            handle_insert,
            ToolRisk::Write,
            ToolCategory::Write,
        ),
        (
            "todo_set",
            "Update tasks: ids[]+status (batch, ranges T1-T3) | id+status+evidence | updates[{id,status?,evidence?,title?,description?}] for title/description edits.",
            todo_set_schema(),
            handle_set,
            ToolRisk::Write,
            ToolCategory::Write,
        ),
        (
            "todo_list",
            "List session tasks; optional status filter. Read-only (allowed in plan mode).",
            todo_list_schema(),
            handle_list,
            ToolRisk::ReadOnly,
            ToolCategory::Read,
        ),
    ];
    for (key, description, input_schema, handler, risk, category) in tools {
        mgr.register_with_placement(
            ToolHandler {
                key: key.to_string(),
                description,
                input_schema,
                handler,
                risk,
                category,
                default_timeout: Duration::from_secs(15),
            },
            crate::ToolPlacement::Workspace,
        );
    }
}
