//! todo::model — 数据模型（TodoItem/TodoStatus/TodoMode/TodoStore + TODO_LOCK）。

use serde::{Deserialize, Serialize};

use std::sync::Mutex;

// ═══════════════════════════════════════════════════════
// Data model
// ═══════════════════════════════════════════════════════

pub(crate) static TODO_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default = "default_status")]
    pub status: TodoStatus,
    /// Completion evidence (filled when status=completed).
    #[serde(default)]
    pub evidence: Option<String>,
}

fn default_status() -> TodoStatus {
    TodoStatus::Pending
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoMode {
    #[default]
    Manual,
    Goal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TodoStore {
    pub items: Vec<TodoItem>,
    #[serde(default)]
    pub mode: TodoMode,
    #[serde(default)]
    pub current_id: Option<String>,
    #[serde(default)]
    pub auto_turns: u32,
    #[serde(default = "default_max_auto")]
    pub max_auto_turns: u32,
    /// 高水位：下一个待分配的 T<n> 号。持久化以取代 max+1 推导——
    /// 旧文件无此字段（default 0）时首次分配自动迁移；未来引入
    /// 删除/归档后已用号也不会被复用（IDs stable 契约）。
    #[serde(default)]
    pub next_id: u32,
}

fn default_max_auto() -> u32 {
    24
}
