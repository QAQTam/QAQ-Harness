mod activity;
mod lease;
mod registry;
pub mod ringing;
mod service;
mod actor;
mod host_impl;
pub mod timeline;
mod timeline_store;

pub use activity::SessionActivityTracker;
pub use lease::{LeaseDecision, LeaseManager};
pub use registry::{AgentRegistry, cache_system_path, detect_os_info};
pub use ringing::hub::RingingHub;
pub use service::QaqhService;
pub use timeline::{
    TimelineAppender, TimelineError, TimelineLiveEntry, materialize_timeline_from_journal,
};
pub mod workspace_supervisor;
pub use service::WorkspaceRuntimeState;
pub use workspace_supervisor::{WorkspaceMode, WorkspaceSupervisor};
