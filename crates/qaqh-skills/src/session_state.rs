//! Skill session-state persistence types (PR-1-2 / B2).
//!
//! Moved verbatim from `qaqh_types::session`: these are skills-domain
//! concepts (persisted in session meta.json by the session manager).
//! `qaqh_types` re-exports them for downstream compatibility.

use serde::{Deserialize, Serialize};

/// Activation state of a single skill within a session.
///
/// Tracks whether a skill is currently loaded and available in the
/// agent's context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillSessionEntryState {
    /// Skill is loaded and active in the current session.
    Active,
    /// Skill was previously available but is now unavailable
    /// (e.g. file deleted, scope changed).
    Unavailable,
}

/// Runtime tracking for one skill in a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionEntry {
    /// Skill name matching SKILL.md metadata.
    pub name: String,
    /// Monotonic counter for determining activation order across sessions.
    pub activation_order: u64,
    /// Path or identifier of the skill source directory (project/user scope).
    pub source: String,
    /// Current activation state.
    pub state: SkillSessionEntryState,
}

/// Snapshot of skill activation state for a session, persisted in meta.json.
///
/// Version 2 adds `context_epoch` and `operation_revision` for tracking
/// skill activation/deactivation across context compaction cycles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionStateV2 {
    /// Schema version (always 2).
    pub version: u8,
    /// Epoch counter incremented on context compaction. Used to detect
    /// whether stale skill contexts need refresh.
    pub context_epoch: u64,
    /// Monotonic revision counter for operation ordering across restarts.
    pub operation_revision: u64,
    /// Active skill entries in activation order.
    pub entries: Vec<SkillSessionEntry>,
}

impl Default for SkillSessionStateV2 {
    fn default() -> Self {
        Self {
            version: 2,
            context_epoch: 0,
            operation_revision: 0,
            entries: Vec::new(),
        }
    }
}
