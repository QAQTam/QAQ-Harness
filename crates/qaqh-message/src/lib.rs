//! qaqh-message: structured conversation state with state-machine lifecycle.
//!
//! `MessageStore` is the single source of truth for messages.
//! Every `push_*` returns `bool` — `true` when the push completed the current turn (last step all tools satisfied, none pending).

pub mod context_flow;
pub mod effect;
pub mod store;
pub mod wal;

pub use context_flow::{
    CompactBehavior, ContextFlow, ContextSource, FlowError, FlowRole, IngestReceipt,
    IngestTraceEntry, LifecyclePolicy, PendingIngest, Sink, Timing, UndoBehavior, Visibility,
    builtin,
};
pub use effect::{PendingTool, PersistOp};
pub use store::{MessageStore, Turn};
pub use wal::WalWriter;
