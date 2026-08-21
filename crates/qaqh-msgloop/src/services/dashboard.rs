use qaqh_proto::DocInfo;
use qaqh_proto::TaskInfo;

pub fn build_documents() -> Vec<DocInfo> {
    let files_read = qaqh_workspace::runtime::files_read();
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
    let files = qaqh_workspace::runtime::files_written();
    files
        .iter()
        .take(10)
        .map(|f| format!("edit: {}", f))
        .collect()
}

pub fn build_tasks() -> Vec<TaskInfo> {
    qaqh_workspace::todo::get_todo_infos()
}

pub fn build_current_todo_id() -> Option<String> {
    qaqh_workspace::todo::load_todo()
        .ok()
        .and_then(|store| store.current_id)
}

/// Builds the native replaceable dashboard record without exposing the legacy
/// `Agent2Ui::Dashboard` schema to new consumers.
pub fn build_snapshot(seed: String) -> qaqh_domain::DashboardSnapshot {
    qaqh_domain::DashboardSnapshot {
        seed,
        documents: build_documents()
            .into_iter()
            .map(|doc| qaqh_domain::DashboardDocument {
                tag: doc.tag,
                path: doc.path,
                turns_since_read: doc.turns_since_read,
                is_stale: doc.is_stale,
            })
            .collect(),
        recent_edits: build_recent_edits(),
        tasks: build_tasks()
            .into_iter()
            .map(|task| qaqh_domain::DashboardTask {
                id: task.id,
                subject: task.subject,
                description: task.description,
                status: task.status,
                evidence: task.evidence,
            })
            .collect(),
        current_todo_id: build_current_todo_id(),
    }
}
