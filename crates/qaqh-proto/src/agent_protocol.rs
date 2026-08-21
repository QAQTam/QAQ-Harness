//! Shared data models formerly carried by the legacy UI ↔ Agent frame
//! protocol (Ui2Agent / Agent2Ui, removed).
//!
//! These types remain as neutral display/projection models consumed by the
//! runtime (session activity) and the agent loop (turn projections). They are
//! not transport frames: all IPC now uses Ringing envelopes
//! (`qaqh-ringing` / `qaqh-domain`).

use serde::{Deserialize, Serialize};

/// Authoritative runtime state for one desktop session.
///
/// Emitted via the Ringing `SessionActivityChanged` domain event whenever the
/// agent session transitions between lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityState {
    /// Agent is initializing (loading config, creating session, etc.).
    Starting,
    /// No turn in progress; waiting for user input.
    Idle,
    /// A turn is actively running (gate → tools loop).
    Working,
    /// Turn suspended — waiting for user response (permission, ask, plan review).
    WaitingUser,
    /// Agent subprocess has disconnected.
    Disconnected,
}

/// Snapshot/event payload emitted for every session state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    /// Session identifier (8 hex chars).
    pub seed: String,
    /// Current lifecycle state.
    pub state: SessionActivityState,
    /// Active turn ID, if a turn is in progress or suspended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Monotonic event sequence number for this session.
    pub seq: u64,
    /// Unix timestamp of this state change.
    pub updated_at: u64,
}

/// An item pending review for todo_activate. Carried in PlanSubmitted.review_type="todo_activation".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoActivationItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}

/// Tool call definition used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDef {
    pub id: String,
    pub name: String,
    /// Human-readable args summary (e.g. "foo.rs", "search pattern")
    pub args_display: String,
    /// Raw JSON arguments string
    pub args_json: String,
}

/// Tool execution result used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultDef {
    pub tool_call_id: String,
    pub output: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileSnapshotInfo>,
}

/// File metadata snapshot for rich rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotInfo {
    pub path: String,
    pub lines: u32,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Document tracking entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocInfo {
    pub tag: String,
    pub path: String,
    pub turns_since_read: u32,
    pub is_stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// One round of a turn (one API call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundData {
    pub round_num: u32,
    #[serde(default)]
    pub is_final: bool,
    pub thinking: Option<String>,
    pub answer: Option<String>,
    pub tool_calls: Vec<ToolCallDef>,
    pub tool_results: Vec<ToolResultDef>,
    /// Ordered blocks preserving the LLM's output sequence (reasoning ↔ text ↔ tool).
    #[serde(default)]
    pub blocks: Vec<RoundBlock>,
}

/// One full turn (user message + all rounds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnData {
    pub turn_id: String,
    pub user_text: String,
    pub rounds: Vec<RoundData>,
}

/// One block in a round, preserving the LLM's output order.
///
/// Blocks are streamed to the frontend in order so it can reconstruct
/// the exact sequence of reasoning → text → tool calls from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoundBlock {
    /// Model reasoning/thinking block (collapsible in UI).
    Reasoning { content: String },
    /// Plain text answer block.
    Text { content: String },
    /// A tool call the model wants to invoke.
    Tool { card: ToolCallDef },
    /// A server-side web search performed by the model's built-in tool
    /// (Responses API). Shown as a record line; the search itself ran on the
    /// provider, so there is no local tool card or result round-trip.
    WebSearch { action: String },
}

/// A single code delta record for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDeltaRecord {
    pub timestamp: u64,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}
