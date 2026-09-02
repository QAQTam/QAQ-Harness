//! Ringing daemon 运行时（Projection/队列 层）。
//!
//! - `sequencer`：stream_seq / channel_seq / session_seq / state_revision 生成
//! - `router`：每频道 ChannelRouter（reliable 队列 + replaceable slots + 回放）
//! - `outbox`：每频道分级发送队列（背压语义）
//! - `journal`：有界可靠 journal + replaceable checkpoint
//! - `projection`：领域 snapshot projection（禁止事件数组模拟状态）
//! - `hub`：三频道聚合入口 `RingingHub`

pub mod attachment;
pub mod content_store;
pub mod conversation_snapshot;
pub mod hub;
pub mod journal;
pub mod journal_store;
pub mod lease_store;
pub mod outbox;
pub mod pending_store;
pub mod projection;
pub mod router;
pub mod sequencer;
pub mod service_methods;
pub(crate) mod timeline_rebuild;

pub use attachment::hydrate_attachment_previews;
pub use content_store::CONTENT_STORE_THRESHOLD_BYTES;
pub use lease_store::RingingLeaseStore;
pub use pending_store::PendingCommandStore;

// PR-2-3 模块规则 1（R-4）：ringing/ 的对外消费面收敛为上方 re-export
// 白名单；`ringing/` 之外的模块禁止深路径引用（如 `ringing::content_store::*`），
// 需要新条目时先在此登记再使用。评审检查单：ringing/ 之外 `crate::ringing::`
// 命中必须全为白名单路径；`crates/qaqh-runtime/src/agent/` 内命中必须为 0。
