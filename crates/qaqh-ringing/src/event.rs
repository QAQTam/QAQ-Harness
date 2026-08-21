//! Ringing 频道事件（wire 视图）。
//!
//! wire 事件枚举即领域事件（`qaqh-domain`）的频道化别名——同一份类型，
//! 不复制结构，保证"serializer 只接受 DomainEvent"的单一事实源。

use qaqh_domain::{ControlEvent, ConversationEvent, ToolEvent};
use serde::{Deserialize, Serialize};

use qaqh_domain::{Delivery, RingingChannel};

/// Control 频道事件（wire 名，域类型为 `qaqh_domain::ControlEvent`）。
pub type RingingControlEvent = ControlEvent;
/// Conversation 频道事件。
pub type RingingConversationEvent = ConversationEvent;
/// Tool 频道事件。
pub type RingingToolEvent = ToolEvent;

/// 统一 Ringing 事件（envelope `event` 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum RingingEvent {
    Control(RingingControlEvent),
    Conversation(RingingConversationEvent),
    Tool(RingingToolEvent),
}

impl RingingEvent {
    pub fn channel(&self) -> RingingChannel {
        match self {
            RingingEvent::Control(_) => RingingChannel::Control,
            RingingEvent::Conversation(_) => RingingChannel::Conversation,
            RingingEvent::Tool(_) => RingingChannel::Tool,
        }
    }

    /// 可靠性由领域事件定义显式声明（Wire 不决定可靠性）。
    pub fn delivery(&self) -> Delivery {
        match self {
            RingingEvent::Control(e) => e.delivery(),
            RingingEvent::Conversation(e) => e.delivery(),
            RingingEvent::Tool(e) => e.delivery(),
        }
    }
}

impl From<qaqh_domain::DomainEvent> for RingingEvent {
    fn from(e: qaqh_domain::DomainEvent) -> Self {
        match e {
            qaqh_domain::DomainEvent::Control(inner) => RingingEvent::Control(inner),
            qaqh_domain::DomainEvent::Conversation(inner) => RingingEvent::Conversation(inner),
            qaqh_domain::DomainEvent::Tool(inner) => RingingEvent::Tool(inner),
        }
    }
}

impl From<RingingEvent> for qaqh_domain::DomainEvent {
    fn from(e: RingingEvent) -> Self {
        match e {
            RingingEvent::Control(inner) => qaqh_domain::DomainEvent::Control(inner),
            RingingEvent::Conversation(inner) => qaqh_domain::DomainEvent::Conversation(inner),
            RingingEvent::Tool(inner) => qaqh_domain::DomainEvent::Tool(inner),
        }
    }
}
