//! Dashboard data assembly from workspace runtime state (PR-1-4 / B4).
//!
//! Moved from the loop crate: these builders project workspace-owned
//! state (files read/written, todos) directly into `qaqh-domain` records
//! (PR-3-3 单源化：proto 侧同名投影层随之删除）。
//!
//! R-4 约束：本 crate 对 `qaqh_domain` 的使用仅限 Dashboard* 类型，
//! 且禁止 import domain 事件类型（评审 grep：`rg -n "qaqh_domain"
//! crates/qaqh-workspace/src | rg -v dashboard` → 0）。

use qaqh_domain::{DashboardDocument, DashboardTask};

use crate::todo::{TodoStatus, load_todo, load_todo_for};

pub fn build_documents() -> Vec<DashboardDocument> {
    let files_read = crate::runtime::files_read();
    let mut docs: Vec<DashboardDocument> = files_read
        .iter()
        .map(|path| {
            let tag = String::from("doc");
            DashboardDocument {
                tag,
                path: path.clone(),
                turns_since_read: 1,
                is_stale: false,
            }
        })
        .collect();
    docs.truncate(20);
    docs
}

pub fn build_recent_edits() -> Vec<String> {
    let files = crate::runtime::files_written();
    files
        .iter()
        .take(10)
        .map(|f| format!("edit: {}", f))
        .collect()
}

/// TodoItem → DashboardTask 投影（状态字符串与 legacy 投影逐值一致）。
fn dashboard_task(item: &crate::todo::TodoItem) -> DashboardTask {
    DashboardTask {
        id: item.id.clone(),
        subject: item.title.clone(),
        description: item.description.clone(),
        status: match item.status {
            TodoStatus::Pending => "idle".into(),
            TodoStatus::InProgress => "in_progress".into(),
            TodoStatus::Completed => "completed".into(),
            TodoStatus::Cancelled => "cancelled".into(),
        },
        evidence: item.evidence.clone(),
    }
}

pub fn build_tasks() -> Vec<DashboardTask> {
    load_todo()
        .map(|store| store.items.iter().map(dashboard_task).collect())
        .unwrap_or_default()
}

pub fn build_tasks_for(seed: &str) -> Vec<DashboardTask> {
    load_todo_for(seed)
        .map(|store| store.items.iter().map(dashboard_task).collect())
        .unwrap_or_default()
}

pub fn build_current_todo_id() -> Option<String> {
    crate::todo::load_todo()
        .ok()
        .and_then(|store| store.current_id)
}

pub fn build_current_todo_id_for(seed: &str) -> Option<String> {
    crate::todo::load_todo_for(seed)
        .ok()
        .and_then(|store| store.current_id)
}
