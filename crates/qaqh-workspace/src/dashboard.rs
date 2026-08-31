//! Dashboard data assembly from workspace runtime state (PR-1-4 / B4).
//!
//! Moved from the loop crate: these four builders project workspace-owned
//! state (files read/written, todos) into `qaqh_proto` records. Domain-event
//! mapping (`build_snapshot`) stays with the loop's engines.

use qaqh_proto::DocInfo;
use qaqh_proto::TaskInfo;

pub fn build_documents() -> Vec<DocInfo> {
    let files_read = crate::runtime::files_read();
    let mut docs: Vec<DocInfo> = files_read
        .iter()
        .map(|path| {
            let tag = String::from("doc");
            DocInfo {
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

pub fn build_tasks() -> Vec<TaskInfo> {
    crate::todo::get_todo_infos()
}

pub fn build_current_todo_id() -> Option<String> {
    crate::todo::load_todo()
        .ok()
        .and_then(|store| store.current_id)
}
