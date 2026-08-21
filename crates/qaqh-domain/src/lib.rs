//! # qaqh-domain — Ringing 领域层
//!
//! 四层架构（Domain / Projection / Wire / Transport）的最底层业务模型：
//!
//! ```text
//! Legacy input ───────→ DomainCommand ───────→ Agent core
//! Ringing command ───→ DomainCommand ───────→ Agent core
//!
//! Agent core ─────────→ DomainEvent ─┬───────→ Ringing event（wire 层序列化）
//!                                    └───────→ LegacyProjector → Agent2Ui
//! ```
//!
//! ## 架构硬规则
//!
//! - 本 crate **不得**依赖 `qaqh-proto`（`Agent2Ui` / `Ui2Agent`）或 `qaqh-ringing`（wire）。
//!   依赖方向固定为：`domain ← wire ← transport`。
//! - 领域事件自行声明可靠性等级（`Delivery`），wire 层不得重新解释。
//! - 领域类型不携带任何传输语义（无 SSE/WebSocket/HTTP/pipe 概念）。

pub mod channel;
pub mod command;
pub mod delivery;
pub mod event;
pub mod timeline;

pub use channel::RingingChannel;
pub use command::{
    AskAnswer, ControlCommand, ConversationCommand, ConversationMode, DomainCommand, ImageBlock,
    ToolCommand,
};
pub use delivery::Delivery;
pub use event::{
    ActivityState, AgentLifecycleState, AskMode, AskQuestion, AskResolution, CompactStatus,
    ContentRef, ControlEvent, ConversationEvent, DashboardDocument, DashboardSnapshot,
    DashboardTask, DomainError, DomainEvent, ErrorScope, NoticeLevel, PermissionCategory,
    PermissionRisk, ProviderToolState, RoundDeltaKind, SessionState, SkillInfo, SkillRuntimeInfo,
    SkillsStatus, TodoItem, ToolEvent, ToolResult,
};
pub use timeline::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelineIntent, TimelineRound, TimelineSnapshot, TimelineTool,
    TimelineToolPermission, TimelineToolState, TimelineTurn, TimelineTurnState,
};
