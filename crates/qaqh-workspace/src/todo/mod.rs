//! todo — 会话级任务清单（Session-scoped todo management）。
//!
//! 由单文件 `todo.rs` 拆分（Phase 2-3）：按既有 ═══ 分段切分，对外 API 不变。
//! 持久化 `sessions/{seed}/todo.json`；公共契约v3 形态（owner 拍板混合制）：
//! todo_write（追加+空清空）/ todo_update（单条状态）/ todo_list（只读）。

pub mod actions;
pub mod model;
pub mod parse;
pub mod split;
pub mod store;

pub use actions::{todo_list_for, todo_set_for};
pub use model::{TodoItem, TodoMode, TodoStatus, TodoStore};
pub use split::register;
#[cfg(test)]
pub(crate) use split::reject_fields;
pub use store::{load_todo, load_todo_for, save_todo, todo_cancel_json, todo_status_json};

#[cfg(test)]
mod tests;
