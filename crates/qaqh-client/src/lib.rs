//! QAQ-Harness Ringing V1 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, three SSE event channels and the per-session timeline
//! stream, plus commands, queries, bootstrap and graceful stop.
//!
//! The public API uses the canonical `qaqh-domain` and `qaqh-ringing`
//! contracts. HTTP/SSE JSON is decoded at this boundary and never becomes a
//! renderer-facing compatibility protocol.

pub mod client;
pub mod discovery;
pub mod endpoint;
pub mod error;
pub mod remote_path;
pub mod session;
pub mod sse;
mod sse_decoder;
pub mod timeline;
pub mod types;

pub use client::{
    Client, ClientHandlers, ClientOptions, RemoteEndpoint, StopStatus, runtime_handle,
};
pub use discovery::{DaemonDiscovery, ensure_daemon_running, read_discovery};
pub use endpoint::{ActionRequest, QueryRequest};
pub use error::{ClientError, Result};
pub use remote_path::{display_host, display_path, remote_path_from_display};
pub use session::{RingingSession, SessionState};
pub use timeline::TimelineStream;
pub use types::ResetRequired;
pub use types::{
    AskAnswer, Channel, ChannelStatus, CommandOptions, ContentRef, ControlCommand, ControlEvent,
    ConversationCommand, ConversationEvent, ConversationMode, DomainActivityState,
    DomainAskQuestion, DomainDashboardSnapshot, DomainError, DomainSessionState, ErrorScope,
    EventBatch, PermissionCategory, PermissionRisk, ProviderToolState, RingingCommand,
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
    RingingEvent, RingingEventEnvelope, RoundDeltaKind, SkillInfo, SkillRuntimeInfo, TimelineBlock,
    TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent, TimelinePage,
    TimelineRound, TimelineSnapshot, TimelineStatus, TimelineTool, TimelineToolState, TimelineTurn,
    TimelineTurnState, TodoItem, ToolCommand, ToolEvent,
};
