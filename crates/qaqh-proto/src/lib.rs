//! QAQ-Harness shared data models and daemon discovery.
//!
//! The legacy UI ↔ Agent frame protocol (`Ui2Agent` / `Agent2Ui`) and the
//! control WebSocket protocol (`ControlClientMessage` / `ControlServerMessage`)
//! have been fully removed. All transport now uses Ringing envelopes
//! (`qaqh-ringing` + `qaqh-domain`); this crate retains only the neutral
//! projection/data models and the on-disk `DaemonDiscovery` record.
//!
//! ## Contents
//!
//! - `agent_protocol` — session activity + turn projection data models
//! - `control` — `DaemonDiscovery` / `CONTROL_PROTOCOL_VERSION`

mod agent_protocol;
mod control;

// ── Re-exports ──────────────────────────────────────────────────────────

pub use agent_protocol::{
    CodeDeltaRecord, DocInfo, FileSnapshotInfo, RoundBlock, RoundData, SessionActivity,
    SessionActivityState, TaskInfo, TodoActivationItem, ToolCallDef, ToolResultDef, TurnData,
};
pub use control::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
