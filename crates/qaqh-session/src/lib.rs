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
