//! ringing_v1/ — production Ringing V1 architecture loop (primary).
//!
//! This loop replaces the old monolithic `Loop` with a fixed set of engine
//! modules. There is deliberately no `Engine` trait: `loop_core` dispatches
//! Ringing commands directly to the engine module that owns the state machine
//! (D6: honest modularity, not a fake plugin boundary).
//!
//! ## Module map
//!
//! | Module              | Role                                   |
//! |---------------------|----------------------------------------|
//! | `types.rs`          | Shared types: Outcome, RingContext, CancelToken |
//! | `loop_core.rs`      | Loop dispatcher                        |
//! | `engine_turn.rs`    | TurnEngine: gate→tools cycle           |
//! | `engine_tool.rs`    | ToolEngine: admit→execute→result       |
//! | `engine_session.rs` | SessionEngine: create/resume/reload    |
//! | `engine_input.rs`   | InputEngine: user input → turn start   |
//! | `engine_compact.rs` | CompactEngine: context summarization   |
//! | `engine_misc.rs`    | MiscEngine: undo/dashboard/mode        |
//!
//! ## Ring interface
//!
//! The central abstraction is the `Outcome` enum. Each Engine returns
//! an Outcome, and the Loop dispatcher acts on it:
//!
//! - `ContinueTurn` → re-enter TurnEngine for another gate lap
//! - `YieldToUser` → pause, wait for PermissionResponse or UserInput
//! - `TurnComplete` → emit Done, return to Idle
//! - `TurnAborted` → emit Cancelled + Done, return to Idle
//! - `Handled` → return to Idle
//! - `Error` → emit error, return to Idle
//! - `Shutdown` → exit loop
//!
//! ## Extension
//!
//! To add a new command/feature:
//! 1. Add the command to `qaqh-domain` / `qaqh-ringing` if it crosses the wire.
//! 2. Add a new `engine_*.rs` module or extend the owning engine module.
//! 3. Route it explicitly in `loop_core::dispatch_ringing_one`.
//!
//! Injection must go through the InjectionBus (roadmap 刀7), not through a
//! new per-feature path.

pub mod engine_compact;
pub mod engine_input;
pub mod engine_misc;
pub mod engine_session;
pub mod engine_title;
pub mod engine_tool;
pub mod engine_turn;
pub mod injection;
pub mod loop_core;
pub mod paced_emitter;
pub(crate) mod turn_lap;
pub mod types;
pub mod wire;
