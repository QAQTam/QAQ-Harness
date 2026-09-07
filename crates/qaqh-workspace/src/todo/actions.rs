//! todo::actions — V2 操作（create/set/list + todo_set_for/todo_list_for 直访变体）。

use serde_json::Value;

use crate::{json_err_string, json_ok};

use super::model::{TODO_LOCK, TodoItem, TodoStatus};
use super::parse::{alloc_id, expand_todo_ids, insertion_index, parse_create_items, parse_todo_id};
use super::store::{
    count_status, normalize_current_id, read_store, read_store_for, status_name, todo_item_json,
    write_store, write_store_for,
};

// ═══════════════════════════════════════════════════════
// Todo V2 operations
// ═══════════════════════════════════════════════════════

pub(crate) fn exec_todo_create(args: &Value, positioned: bool) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    if !positioned && (args.get("after_id").is_some() || args.get("before_id").is_some()) {
        return Err(json_err_string(
            "INVALID_INPUT",
            "create does not accept before_id or after_id",
            "Use action=insert for positioned tasks.",
        ));
    }
    let insertion = if positioned {
        insertion_index(&store, args)?
    } else {
        store.items.len()
    };
    let pending = parse_create_items(args)?;

    // IDs are permanent monotonically assigned identities. Inserting a subtask
    // changes display order only; existing IDs are never renumbered.
    let mut created = Vec::with_capacity(pending.len());
    for todo in pending {
        created.push(TodoItem {
            id: alloc_id(&mut store),
            title: todo.title,
            description: todo.description,
            status: TodoStatus::Pending,
            evidence: None,
        });
    }
    store
        .items
        .splice(insertion..insertion, created.iter().cloned());
    normalize_current_id(&mut store);
    write_store(&store)?;

    Ok(json_ok(serde_json::json!({
        "created": created.iter().map(todo_item_json).collect::<Vec<_>>(),
        "count": created.len(),
        "message": format!(
            "Created {} todo(s): {}",
            created.len(),
            created.iter().map(|item| item.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    })))
}

pub(crate) fn parse_status(value: &str) -> Option<TodoStatus> {
    match value {
        // V2 public vocabulary.
        "idle" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "completed" | "complete" => Some(TodoStatus::Completed),
        "cancelled" | "canceled" => Some(TodoStatus::Cancelled),
        // V1 compatibility for persisted/model calls during rollout.
        "pending" => Some(TodoStatus::Pending),
        _ => None,
    }
}

/// 可选文本编辑字段：absent → None（不修改）；提供则 trim 后校验长度。
/// title 不允许空（1-100）；description 允许空（显式清空）。
pub(crate) fn parse_edit_field(
    value: Option<&Value>,
    label: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value.as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        if allow_empty {
            return Ok(Some(text));
        }
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label} must be 1-{max_chars} chars when provided"),
            "Omit the field to leave it unchanged.",
        ));
    }
    if text.chars().count() > max_chars {
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label} max {max_chars} chars"),
            "",
        ));
    }
    Ok(Some(text))
}

pub(crate) fn exec_todo_set(args: &Value) -> Result<String, String> {
    let seed = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    todo_set_for(&seed, args)
}

/// Seed 参数化的 todo.set 直访变体（HTTP service 面 / CLI 用；不依赖
/// runtime 线程局部上下文）。锁与工具路径一致：本函数内获取，持久化
/// 直调 write_store_for（不得再走 save_todo 二次加锁）。
pub fn todo_set_for(seed: &str, args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store_for(seed)?;

    /// 一次变更（支持单条 / ids 批量 / updates 并行三种来源）。
    /// status 为 None 表示纯编辑（title/description/evidence）。
    struct PendingSet {
        id: String,
        status: Option<TodoStatus>,
        evidence: Option<String>,
        title: Option<String>,
        description: Option<String>,
    }

    let pending: Vec<PendingSet> = if let Some(updates) =
        args.get("updates").and_then(Value::as_array)
    {
        if updates.is_empty() {
            return Err(json_err_string(
                "INVALID_INPUT",
                "updates must not be empty",
                "Provide at least one {id, status} entry.",
            ));
        }
        let mut list = Vec::new();
        for update in updates {
            let id = parse_todo_id(update.get("id")).ok_or_else(|| {
                json_err_string(
                    "INVALID_INPUT",
                    "updates[].id missing or invalid",
                    "Provide the assigned ID, e.g. T1.",
                )
            })?;
            let requested = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // 纯编辑（title/description）时 status 可省略；提供则必须合法。
            let status = if requested.is_empty() {
                None
            } else {
                Some(parse_status(requested).ok_or_else(|| {
                    json_err_string(
                        "INVALID_INPUT",
                        format!("unknown status: {requested}"),
                        "Use idle, in_progress, completed, or cancelled.",
                    )
                })?)
            };
            let evidence = update
                .get("evidence")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let title = parse_edit_field(update.get("title"), "updates[].title", 100, false)?;
            let description = parse_edit_field(
                update.get("description"),
                "updates[].description",
                200,
                true,
            )?;
            if status.is_none() && title.is_none() && description.is_none() && evidence.is_none() {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("updates[] entry for {id} changes nothing"),
                    "Provide status, evidence, title, or description.",
                ));
            }
            list.push(PendingSet {
                id,
                status,
                evidence,
                title,
                description,
            });
        }
        list
    } else if let Some(ids) = args.get("ids") {
        let requested = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = parse_status(requested).ok_or_else(|| {
            json_err_string(
                "INVALID_INPUT",
                format!("unknown status: {requested}"),
                "Use idle, in_progress, completed, or cancelled.",
            )
        })?;
        let mut list = Vec::new();
        for expr in ids.as_array().ok_or_else(|| {
            json_err_string(
                "INVALID_INPUT",
                "ids must be an array of strings",
                "Use ids: [\"T1\", \"T1-T3\"]",
            )
        })? {
            let expr = expr.as_str().ok_or_else(|| {
                json_err_string(
                    "INVALID_INPUT",
                    "ids entries must be strings",
                    "Use ids: [\"T1\", \"T1-T3\"]",
                )
            })?;
            for id in expand_todo_ids(expr)? {
                list.push(PendingSet {
                    id,
                    status: Some(status.clone()),
                    evidence: None,
                    title: None,
                    description: None,
                });
            }
        }
        list
    } else {
        let id = parse_todo_id(args.get("id")).ok_or_else(|| {
            json_err_string(
                "INVALID_INPUT",
                "missing or invalid id",
                "Provide the assigned ID, e.g. T1.",
            )
        })?;
        let requested = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = parse_status(requested).ok_or_else(|| {
            json_err_string(
                "INVALID_INPUT",
                format!("unknown status: {requested}"),
                "Use idle, in_progress, completed, or cancelled.",
            )
        })?;
        let evidence = args
            .get("evidence")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if args.get("evidence").is_some() && evidence.is_none() {
            return Err(json_err_string(
                "INVALID_INPUT",
                "evidence must be a non-empty string when provided",
                "Omit evidence unless there is a concrete result summary.",
            ));
        }
        vec![PendingSet {
            id,
            status: Some(status),
            evidence,
            title: None,
            description: None,
        }]
    };

    // 宽松应用：未知 ID 记入 not_found，不中断其余更新。
    let mut updated: Vec<serde_json::Value> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    let mut last_updated_idx: Option<usize> = None;
    for pending in &pending {
        if let Some(idx) = store.items.iter().position(|item| item.id == pending.id) {
            if let Some(status) = &pending.status {
                store.items[idx].status = status.clone();
            }
            if pending.evidence.is_some() {
                store.items[idx].evidence = pending.evidence.clone();
            }
            if let Some(title) = &pending.title {
                store.items[idx].title = title.clone();
            }
            if let Some(description) = &pending.description {
                store.items[idx].description = description.clone();
            }
            // V1 clients may still send schema-filled empty strings. Status changes
            // are deliberately ID-only and must never erase task metadata.
            updated.push(serde_json::json!({
                "id": pending.id,
                "status": status_name(&store.items[idx].status),
            }));
            last_updated_idx = Some(idx);
        } else {
            not_found.push(pending.id.clone());
        }
    }

    if updated.is_empty() {
        return Err(json_err_string(
            "NOT_FOUND",
            format!("no matching todos: {}", not_found.join(", ")),
            "Use todo(action=\"list\") to inspect IDs.",
        ));
    }

    normalize_current_id(&mut store);
    write_store_for(seed, &store)?;

    if pending.len() == 1 && updated.len() == 1 {
        // 单条路径保持兼容返回（V1 客户端/前端依赖 item + message）。
        let item_json = todo_item_json(&store.items[last_updated_idx.expect("single update")]);
        let id = &pending[0].id;
        let message = match &pending[0].status {
            Some(status) => format!("Todo {id} is now {}.", status_name(status)),
            None => format!("Todo {id} updated."),
        };
        Ok(json_ok(serde_json::json!({
            "item": item_json,
            "message": message
        })))
    } else {
        Ok(json_ok(serde_json::json!({
            "updated": updated,
            "not_found": not_found,
            "message": format!("Updated {} todo(s).", updated.len()),
        })))
    }
}

pub(crate) fn exec_todo_list(args: &Value) -> Result<String, String> {
    let seed = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    todo_list_for(&seed, args)
}

/// Seed 参数化的 todo.list 直访变体（HTTP service 面 / CLI 用）。
pub fn todo_list_for(seed: &str, args: &Value) -> Result<String, String> {
    let store = read_store_for(seed)?;
    let filter = args
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| {
            parse_status(value).ok_or_else(|| {
                json_err_string(
                    "INVALID_INPUT",
                    format!("unknown status: {value}"),
                    "Use idle, in_progress, completed, or cancelled.",
                )
            })
        })
        .transpose()?;
    let items: Vec<&TodoItem> = store
        .items
        .iter()
        .filter(|item| filter.as_ref().is_none_or(|status| item.status == *status))
        .collect();

    Ok(json_ok(serde_json::json!({
        "items": items.into_iter().map(todo_item_json).collect::<Vec<_>>(),
        "current_id": store.current_id,
        "counts": {
            "idle": count_status(&store, TodoStatus::Pending),
            "in_progress": count_status(&store, TodoStatus::InProgress),
            "completed": count_status(&store, TodoStatus::Completed),
            "cancelled": count_status(&store, TodoStatus::Cancelled),
            "total": store.items.len(),
        }
    })))
}
