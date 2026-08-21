//! Ringing daemon 运行时（Projection/队列 层）。
//!
//! - `sequencer`：stream_seq / channel_seq / session_seq / state_revision 生成
//! - `router`：每频道 ChannelRouter（reliable 队列 + replaceable slots + 回放）
//! - `outbox`：每频道分级发送队列（背压语义）
//! - `journal`：有界可靠 journal + replaceable checkpoint
//! - `projection`：领域 snapshot projection（禁止事件数组模拟状态）
//! - `hub`：三频道聚合入口 `RingingHub`

pub mod content_store;
pub mod conversation_snapshot;
pub mod hub;
pub mod journal;
pub mod journal_store;
pub mod outbox;
pub mod projection;
pub mod query;
pub mod router;
pub mod sequencer;
pub(crate) mod timeline_rebuild;
pub mod tool_progress;
