//! qaqh-runtime::agent — agent loop, engines and session state (merged from the former message-loop crate, PR-2-1).
//!
//! The primary production Loop is [`loop_core::Loop`] (Ringing V1 architecture).
//! It reads Ringing worker command envelopes (`RingingWorkerCommandEnvelope`) via an mpsc channel fed by a background I/O
//! thread, and writes Ringing worker event envelopes via a channel consumed by a background
//! writer thread. It drives the full user-input → gate → tools → response
//! pipeline through a fixed set of engine modules dispatched by [`loop_core`].
//!
//! ## Architecture
//!
//! ```text
//! Loop（in-process actor 线程，单会话）
//!  ├─ I/O: cmd_rx, event_tx（typed channel，daemon registry 供 fed）
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
//! | Entry     | `spawn.rs`  | 构造唯一入口（PR-2-3）                  |
//! | Loop      | `loop_core.rs` | Ringing V1 固定引擎模块显式分派      |
//! | Engines   | `engine_*.rs`（平铺） | session/input/compact/misc/title/tool/turn |
//! | State     | `state/`    | AgentState, sessions, skills            |
//! | Services  | `dashboard.rs` | Conflict detection, dashboard        |
//! | Utilities | `util/`     | Calendar, token logging, display fmt    |
//!
//! 引擎模块为固定集合，无独立 `Engine` trait；命令经 `dispatch_ringing_one`
//! 直接路由到各引擎方法。
//!
//! ## Module rules（PR-2-3 / R-4 评审检查单）
//!
//! 1. `agent/` 内禁止引用 runtime 自身的 ringing 模块（`use crate::ringing…`）——
//!    ringing 是 daemon 侧投影/队列层，agent 仅经本模块的 channel 类型
//!    （`types::WorkerCommand` / `types::WriterEvent`）与之交互。检查单：
//!    在 `crates/qaqh-runtime/src/agent/` 内 grep `crate::ringing` → 0 命中。
//! 2. 对外构造唯一入口为 `spawn_agent`（`actor.rs` / `registry.rs` 不得自行
//!    装配 `AgentState`）。


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
pub(crate) mod spawn;
pub(crate) mod turn_lap;
pub mod types;
pub mod wire;
pub mod state;
pub mod util;

pub(crate) use spawn::{spawn_agent, ActorKind, SubagentSpawnSpec};
