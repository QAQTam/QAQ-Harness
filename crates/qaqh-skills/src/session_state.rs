//! Compatibility re-exports for the skill session-state persistence types.
//!
//! The canonical definitions live in `qaqh_types::session` — the dependency
//! direction was corrected (skills → types, the foundation crate must not look
//! up at a domain crate). This module keeps the `qaqh_skills::session_state::*`
//! import path working for any existing consumers.

pub use qaqh_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
