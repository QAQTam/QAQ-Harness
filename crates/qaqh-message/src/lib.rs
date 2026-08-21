//! qaqh-message: structured conversation state with state-machine lifecycle.
//!
//! `MessageStore` is the single source of truth for messages.
//! Every `push_*` returns an [`Effect`] telling the caller what to do next.

pub mod context_flow;
pub mod effect;
pub mod store;

pub use context_flow::{
    CompactBehavior, ContextFlow, ContextSource, FlowError, FlowRole, IngestReceipt,
    IngestTraceEntry, LifecyclePolicy, PendingIngest, Sink, Timing, UndoBehavior, Visibility,
    builtin,
};
pub use effect::{Effect, PendingTool, ToolExecReport, ToolExecRequest, ToolExecutorFn};
pub use store::{MessageStore, Turn};
