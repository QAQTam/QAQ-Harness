//! qaqh-runtime::agent — agent loop, engines and session state (merged from the former message-loop crate, PR-2-1).
//!
//! The primary production Loop is [`ringing_v1::loop_core::Loop`] (Ringing V1 architecture).
//! It reads Ringing worker command envelopes (`RingingWorkerCommandEnvelope`) via an mpsc channel fed by a background I/O
//! thread, and writes Ringing worker event envelopes via a channel consumed by a background
//! writer thread. It drives the full user-input → gate → tools → response
//! pipeline through a fixed set of engine modules dispatched by [`ringing_v1::loop_core`].
//!
//! ## Architecture
//!
//! ```text
//! Loop（worker 进程，单会话）
//!  ├─ I/O: cmd_rx, event_tx（stdin/stdout JSON-LP 双线程）
//!  ├─ Signal: cancel, phase, pending, writer_dead
//!  ├─ Session: SessionBundle { agent, stats, turn, tool }
//!  ├─ Stateless engines: session, input, compact, goal, misc
//!  ├─ flow: ContextFlow
//!  └─ injection_bus + paced_emitter
//! ```
//!
//! ## Module layout
//!
//! | Layer     | Path        | Role                                    |
//! |-----------|-------------|-----------------------------------------|
//! | Ringing V1 loop | `ringing_v1/`     | Fixed engine modules dispatched explicitly |
//! | State     | `state/`    | AgentState, sessions, skills            |
//! | Services  | `services/` | Conflict detection, dashboard         |
//! | Utilities | `util/`     | Calendar, token logging, display fmt    |
//!
//! Ringing V1 引擎模块：`ringing_v1/engine_*.rs`（固定模块集合，无独立
//! `Engine` trait；命令经 `dispatch_ringing_one` 直接路由到各引擎方法）。


pub(crate) mod dashboard;
pub mod input_guard;
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
pub mod prompt;
pub(crate) mod turn_lap;
pub mod types;
pub mod wire;
pub mod state;
pub mod util;
