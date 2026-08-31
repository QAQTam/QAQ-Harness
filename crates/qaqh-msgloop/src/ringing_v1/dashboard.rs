//! Native replaceable dashboard snapshot assembly (PR-1-4 / B4).
//!
//! Domain mapping only; the workspace-state projection lives in
//! `qaqh_workspace::dashboard`. All four engine consumers call
//! [`build_snapshot`] directly.

/// Builds the native replaceable dashboard record without exposing the legacy
/// `Agent2Ui::Dashboard` schema to new consumers.
pub fn build_snapshot(seed: String) -> qaqh_domain::DashboardSnapshot {
    qaqh_domain::DashboardSnapshot {
        seed,
        documents: qaqh_workspace::dashboard::build_documents()
            .into_iter()
            .map(|doc| qaqh_domain::DashboardDocument {
                tag: doc.tag,
                path: doc.path,
                turns_since_read: doc.turns_since_read,
                is_stale: doc.is_stale,
            })
            .collect(),
        recent_edits: qaqh_workspace::dashboard::build_recent_edits(),
        tasks: qaqh_workspace::dashboard::build_tasks()
            .into_iter()
            .map(|task| qaqh_domain::DashboardTask {
                id: task.id,
                subject: task.subject,
                description: task.description,
                status: task.status,
                evidence: task.evidence,
            })
            .collect(),
        current_todo_id: qaqh_workspace::dashboard::build_current_todo_id(),
    }
}
