use qaqh_types::Message;

/// Returned by MessageStore::push_* methods.
/// Tells external actors (runner) what to do next.
#[derive(Debug, Clone)]
pub enum Effect {
    /// No side effect.
    None,
    /// Call the gate with this context.
    CallGate { messages: Vec<Message> },
    /// Turn finished — save snapshot, return to idle.
    TurnComplete,
}

/// A tool invocation extracted from the assistant message.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Host-side persistence instruction (PR-1-6 / A1).
///
/// MessageStore never touches the session manager singleton: every disk write is enqueued
/// as a [`PersistOp`] (see `MessageStore::take_persist_ops`), and the host
/// (loop) drains the queue after each command dispatch and replays the ops
/// against an injected the session manager singleton. Single-threaded replay keeps the
/// on-disk byte order identical to the old synchronous writes (Z5 red line).
///
/// The op→the session manager singleton mapping lives on the consumer side (msgloop /
/// runtime): this crate must not re-introduce a `qaqh-session` dependency
/// just to execute persistence.
#[derive(Debug, Clone)]
pub enum PersistOp {
    /// Append new messages to messages.jsonl and refresh meta/index
    /// (was `the session manager's save_append`).
    Append {
        seed: String,
        messages: Vec<Message>,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        turn_count: usize,
    },
    /// Refresh meta/index without new messages (was `update_meta`).
    UpdateMeta {
        seed: String,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        turn_count: usize,
    },
    /// Refresh the live-context projection of the compact checkpoint
    /// (was `update_compact_context`).
    UpdateCompactContext { seed: String, messages: Vec<Message> },
    /// Full rewrite of the compact checkpoint (was `save_compact_context`).
    SaveCompactContext { seed: String, messages: Vec<Message> },
    /// Full rewrite of messages.jsonl — undo / compact aftermath
    /// (was `save_full`).
    SaveFull {
        seed: String,
        messages: Vec<Message>,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        turn_count: usize,
    },
}
