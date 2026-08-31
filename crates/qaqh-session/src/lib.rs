//! qaqh-session — unified session manager singleton.
//!
//! Follows the same pattern as qaqh-workspace::ToolManager.

pub mod manager;
mod migrate;
pub mod session_meta;
pub mod store;
pub mod workspace;
pub use manager::{CompactContext, SessionManager};
pub use session_meta::SessionMeta;
pub use workspace::{WorkspaceMeta, WorkspaceStore};

/// Free-function seed generator (PR-1-5 / B6): loop crates consume the
/// helpers without naming the [`SessionManager`] type. Delegates to the
/// manager's associated functions — no behaviour change.
pub fn generate_seed() -> String {
    SessionManager::generate_seed()
}

/// Free-function epoch helper, same rationale as [`generate_seed`].
pub fn now_epoch() -> u64 {
    SessionManager::now_epoch()
}
