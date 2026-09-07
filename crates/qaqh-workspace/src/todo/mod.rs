//! todo — 会话级任务清单（Session-scoped todo management）。
//!
//! 由单文件 `todo.rs` 拆分（Phase 2-3）：按既有 ═══ 分段切分，对外 API 不变。
//! 持久化 `sessions/{seed}/todo.json`；公共契约支持 create/ordered insert/
//! ID-only state changes/list。

pub mod actions;
pub mod dispatch;
pub mod model;
pub mod parse;
pub mod store;

pub use actions::{todo_list_for, todo_set_for};
pub use dispatch::register;
#[cfg(test)]
pub(crate) use dispatch::{handle_todo, reject_fields};
pub use model::{TodoItem, TodoMode, TodoStatus, TodoStore};
pub use store::{load_todo, load_todo_for, save_todo, todo_cancel_json, todo_status_json};

#[cfg(test)]
mod tests;
