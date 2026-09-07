//! todo::store — 持久化与查询（todo.json 读写 + status/cancel JSON）。

use crate::{json_err_string, json_ok};

use super::model::{TODO_LOCK, TodoItem, TodoMode, TodoStatus, TodoStore};

// ═══════════════════════════════════════════════════════
// Persistence
// ═══════════════════════════════════════════════════════

pub(crate) fn todo_path() -> Option<std::path::PathBuf> {
    let session = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    if session.is_empty() {
        None
    } else {
        Some(
            qaqh_types::platform::sessions_dir()
                .join(&session)
                .join("todo.json"),
        )
    }
}

/// Session-aware read: direct path `sessions/{seed}/todo.json` (no thread-local).
pub(crate) fn todo_path_for(seed: &str) -> std::path::PathBuf {
    qaqh_types::platform::sessions_dir()
        .join(seed)
        .join("todo.json")
}

pub(crate) fn read_store_for(seed: &str) -> Result<TodoStore, String> {
    if seed.is_empty() {
        return Err("no active session".into());
    }
    let path = todo_path_for(seed);
    if !path.exists() {
        return Ok(TodoStore {
            items: Vec::new(),
            mode: TodoMode::Manual,
            current_id: None,
            auto_turns: 0,
            max_auto_turns: 24,
            next_id: 0,
        });
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    let store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    Ok(store)
}

/// Session-aware variant: load TodoStore for an explicit seed (no RUNTIME_CTX).
pub fn load_todo_for(seed: &str) -> Result<TodoStore, String> {
    let _guard = TODO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_store_for(seed)
}

/// Public API: load the TodoStore from disk (used by GoalEngine).
pub fn load_todo() -> Result<TodoStore, String> {
    // W2：公共入口串行化，GoalEngine 与工具路径互斥。
    let _guard = TODO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_store()
}

/// Public API: save the TodoStore to disk atomically (used by GoalEngine).
pub fn save_todo(store: &TodoStore) -> Result<(), String> {
    // W2：公共入口串行化（write_store 自身不加锁，见其注释）。
    let _guard = TODO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    write_store(store)
}

/// Session-scoped todo status for the frontend Todo panel.
pub fn todo_status_json(seed: &str) -> Result<String, String> {
    if seed.is_empty() {
        return Ok("null".into());
    }
    // Serialise with writer to avoid reading a half-renamed tmp.
    let _guard = TODO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = qaqh_types::platform::sessions_dir()
        .join(seed)
        .join("todo.json");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("null".into()),
        Err(e) => return Err(format!("read todo.json: {e}")),
    };
    let store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    let current = store
        .current_id
        .as_ref()
        .and_then(|id| {
            store
                .items
                .iter()
                .find(|item| &item.id == id && item.status == TodoStatus::InProgress)
        })
        .or_else(|| {
            store
                .items
                .iter()
                .find(|item| item.status == TodoStatus::InProgress)
        });
    let pending = count_status(&store, TodoStatus::Pending);
    let in_progress = count_status(&store, TodoStatus::InProgress);
    let completed = count_status(&store, TodoStatus::Completed);
    let cancelled = count_status(&store, TodoStatus::Cancelled);
    let items_summary: Vec<serde_json::Value> = store.items.iter().map(todo_item_json).collect();
    serde_json::to_string(&serde_json::json!({
        "mode": "manual",
        "current_id": current.map(|item| item.id.clone()),
        "current_title": current.map(|i| i.title.clone()),
        "idle": pending,
        "pending": pending,
        "in_progress": in_progress,
        "completed": completed,
        "cancelled": cancelled,
        "total": store.items.len(),
        "items": items_summary,
    }))
    .map_err(|e| format!("todo: {e}"))
}

/// Direct cancel by session seed — no runtime context needed.
pub fn todo_cancel_json(seed: &str, id: &str) -> Result<String, String> {
    if seed.is_empty() {
        return Err(json_err_string("INVALID_INPUT", "no active session", ""));
    }
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let path = qaqh_types::platform::sessions_dir()
        .join(seed)
        .join("todo.json");
    if !path.exists() {
        return Err(json_err_string(
            "NOT_FOUND",
            "no todo list for this session",
            "",
        ));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    let mut store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    let idx = store
        .items
        .iter()
        .position(|item| item.id == id)
        .ok_or_else(|| {
            json_err_string(
                "NOT_FOUND",
                format!("todo {id} not found"),
                "Use todo(action=\"list\") to see all IDs.",
            )
        })?;

    store.items[idx].status = TodoStatus::Cancelled;
    normalize_current_id(&mut store);

    let item_json = todo_item_json(&store.items[idx]);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(&store).map_err(|e| format!("serialize todo: {e}"))?;
    std::fs::write(&tmp, data).map_err(|e| format!("write todo.tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename todo: {e}"))?;

    Ok(json_ok(serde_json::json!({
        "item": item_json,
        "message": format!("Todo {id} cancelled.")
    })))
}

pub(crate) fn read_store() -> Result<TodoStore, String> {
    let path = todo_path().ok_or("no active session")?;
    if !path.exists() {
        return Ok(TodoStore {
            items: Vec::new(),
            mode: TodoMode::Manual,
            current_id: None,
            auto_turns: 0,
            max_auto_turns: 24,
            next_id: 0,
        });
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    let store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    Ok(store)
}

pub(crate) fn status_name(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "idle",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn count_status(store: &TodoStore, status: TodoStatus) -> usize {
    store
        .items
        .iter()
        .filter(|item| item.status == status)
        .count()
}

pub(crate) fn todo_item_json(item: &TodoItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "title": item.title,
        "description": item.description,
        "status": status_name(&item.status),
        "evidence": item.evidence,
    })
}

/// Atomic write: temporary file → rename.
pub(crate) fn write_store_for(seed: &str, store: &TodoStore) -> Result<(), String> {
    // 注意：本函数不加 TODO_LOCK——工具路径在持锁状态下调用它；
    // 锁由公共入口 load_todo/save_todo 负责（W2），内部调用方须已持锁。
    let path = todo_path_for(seed);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create todo directory: {e}"))?;
    }
    // 唯一 tmp 名：并发写者不再互踩同一 json.tmp（W2）。
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let data = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize todo: {e}"))?;
    std::fs::write(&tmp, data).map_err(|e| format!("write todo.tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename todo: {e}"))
}

pub(crate) fn write_store(store: &TodoStore) -> Result<(), String> {
    let seed = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    write_store_for(&seed, store)
}

pub(crate) fn normalize_current_id(store: &mut TodoStore) {
    if let Some(active) = store
        .items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
    {
        store.current_id = Some(active.id.clone());
    } else {
        store.current_id = None;
    }
}
