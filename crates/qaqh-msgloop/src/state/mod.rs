//! state/ — passive domain state and data model.
//!
//! This layer holds the agent's mutable state (`AgentState`), session lifecycle
//! functions, and the skill-context manager. These are consumed by the Ring
//! architecture engines but are not engines themselves — they carry no event-loop
//! logic and make no scheduling decisions.
//!
//! ## Modules
//!
//! | Module            | Role                              |
//! |-------------------|-----------------------------------|
//! | `agent.rs`        | `AgentState`: central agent state |
//! | `lifecycle.rs`    | Session create / init / restore   |
//! | `skill_context.rs`| `SkillContextManager`             |

pub mod agent;
pub mod lifecycle;
pub mod skill_context;
pub(crate) mod token_calibration;
