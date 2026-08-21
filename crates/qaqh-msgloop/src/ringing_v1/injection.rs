//! Unified injection bus (knife-7 stage 1).
//!
//! Every non-user message injection (subagent reports today; system/MCP in
//! the future) flows through `Loop::inject()` and is queued here. The bus
//! owns the busy-turn queue and command-id idempotency; `ContextFlow`
//! remains the message persistence boundary.

use std::collections::{HashSet, VecDeque};

use qaqh_types::{ContentBlock, Message};

/// Stable source id used by the existing ContextFlow registration.
pub const SUBAGENT_SOURCE: &str = "subagent";

/// Injection priority. `Deferred` marks injections queued while a compact is
/// running; they are dispatched as turns once the compact finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionPriority {
    Normal,
    Deferred,
}

/// Injection semantics. Only `NextTurn` exists this round; `Interrupt` /
/// `Inline` are reserved for future sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionSemantics {
    NextTurn,
}

/// A non-user message injection waiting to be handed to ContextFlow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub session_id: String,
    pub command_id: String,
    /// Source id (`SUBAGENT_SOURCE` today; future "system"/"mcp").
    pub source: &'static str,
    /// Role used for the persisted message shape (`Message::ROLE_USER`).
    pub role: &'static str,
    pub text: String,
    pub priority: InjectionPriority,
    pub semantics: InjectionSemantics,
}

impl Injection {
    /// Convenience constructor matching the pre-unification subagent shape:
    /// source=SUBAGENT_SOURCE, role=user, priority=Normal, semantics=NextTurn.
    pub fn new(
        session_id: impl Into<String>,
        command_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            command_id: command_id.into(),
            source: SUBAGENT_SOURCE,
            role: Message::ROLE_USER,
            text: text.into(),
            priority: InjectionPriority::Normal,
            semantics: InjectionSemantics::NextTurn,
        }
    }

    /// Preserve the established storage shape: `user` + `name=subagent`
    /// (the `name` follows `self.source`, so a new source only needs to
    /// declare itself — the message shape for subagent stays identical).
    pub fn message(&self) -> Message {
        Message {
            msg_id: None,
            role: self.role.into(),
            name: Some(self.source.into()),
            content: vec![ContentBlock::text(&self.text)],
        }
    }
}

/// Result of attempting to enqueue an injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Queued,
    DuplicateCommandId,
    StaleSession,
}

/// FIFO queue and session-local idempotency set for injections.
#[derive(Debug, Default)]
pub struct InjectionBus {
    active_session: Option<String>,
    pending: VecDeque<Injection>,
    seen_command_ids: HashSet<String>,
}

impl InjectionBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch the active session and discard all queued/seen injections from
    /// the previous session. Re-selecting the same session is not a switch.
    pub fn switch_session(&mut self, session_id: &str) {
        if session_id.is_empty() {
            self.clear();
            self.active_session = None;
            return;
        }
        if self.active_session.as_deref() != Some(session_id) {
            self.clear();
            self.active_session = Some(session_id.to_string());
        }
    }

    /// Clear queued records and their idempotency history without changing
    /// the currently selected session.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.seen_command_ids.clear();
    }

    pub fn enqueue(&mut self, injection: Injection) -> EnqueueResult {
        if injection.session_id.is_empty() {
            return EnqueueResult::StaleSession;
        }
        if self.active_session.is_none() {
            self.active_session = Some(injection.session_id.clone());
        }
        if self.active_session.as_deref() != Some(injection.session_id.as_str()) {
            return EnqueueResult::StaleSession;
        }
        if !self.seen_command_ids.insert(injection.command_id.clone()) {
            return EnqueueResult::DuplicateCommandId;
        }
        self.pending.push_back(injection);
        EnqueueResult::Queued
    }

    /// Take pending records in submission order. Seen command ids stay marked
    /// so a replay after the boundary cannot submit a second message.
    pub fn drain(&mut self) -> Vec<Injection> {
        self.pending.drain(..).collect()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection(session_id: &str, command_id: &str, text: &str) -> Injection {
        Injection::new(session_id, command_id, text)
    }

    #[test]
    fn preserves_fifo_and_existing_message_shape() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "first")),
            EnqueueResult::Queued
        );
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-2", "second")),
            EnqueueResult::Queued
        );

        let drained = bus.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "first");
        assert_eq!(drained[1].text, "second");
        let message = drained[0].message();
        assert_eq!(message.role, "user");
        assert_eq!(message.name.as_deref(), Some("subagent"));
    }

    #[test]
    fn rejects_duplicate_command_id_before_and_after_drain() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "first")),
            EnqueueResult::Queued
        );
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "duplicate")),
            EnqueueResult::DuplicateCommandId
        );
        assert_eq!(bus.drain().len(), 1);
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "replay")),
            EnqueueResult::DuplicateCommandId
        );
    }

    #[test]
    fn session_switch_clears_pending_and_idempotency_state() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "old")),
            EnqueueResult::Queued
        );

        bus.switch_session("session-b");
        assert_eq!(bus.pending_len(), 0);
        assert_eq!(bus.drain(), Vec::new());
        assert_eq!(
            bus.enqueue(injection("session-b", "cmd-1", "new")),
            EnqueueResult::Queued
        );
        assert_eq!(bus.drain()[0].session_id, "session-b");
    }

    #[test]
    fn mixes_sources_fifo_and_dedupes_command_ids_across_sources() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        let sys = Injection {
            source: "system",
            ..injection("session-a", "cmd-1", "system report")
        };
        let sub = injection("session-a", "cmd-1", "subagent replay");
        let mcp = Injection {
            source: "mcp",
            ..injection("session-a", "cmd-2", "mcp report")
        };

        assert_eq!(bus.enqueue(sys), EnqueueResult::Queued);
        assert_eq!(
            bus.enqueue(sub),
            EnqueueResult::DuplicateCommandId,
            "command_id dedupe must be source-independent"
        );
        assert_eq!(bus.enqueue(mcp), EnqueueResult::Queued);

        let drained = bus.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "system report");
        assert_eq!(drained[1].text, "mcp report");
        // message() 的 name 跟随 source；subagent 的既有形状不变。
        assert_eq!(drained[0].message().name.as_deref(), Some("system"));
        assert_eq!(drained[1].message().name.as_deref(), Some("mcp"));
        assert_eq!(drained[0].priority, InjectionPriority::Normal);
        assert_eq!(drained[0].semantics, InjectionSemantics::NextTurn);
    }
}
