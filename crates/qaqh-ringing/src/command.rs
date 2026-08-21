//! Ringing 频道命令（wire 视图）。

use qaqh_domain::{ControlCommand, ConversationCommand, ToolCommand};
use serde::{Deserialize, Serialize};

use qaqh_domain::RingingChannel;

/// Control 频道命令（wire 名，域类型为 `qaqh_domain::ControlCommand`）。
pub type RingingControlCommand = ControlCommand;
/// Conversation 频道命令。
pub type RingingConversationCommand = ConversationCommand;
/// Tool 频道命令。
pub type RingingToolCommand = ToolCommand;

/// 统一 Ringing 命令（envelope `command` 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum RingingCommand {
    Control(RingingControlCommand),
    Conversation(RingingConversationCommand),
    Tool(RingingToolCommand),
}

impl RingingCommand {
    pub fn channel(&self) -> RingingChannel {
        match self {
            RingingCommand::Control(_) => RingingChannel::Control,
            RingingCommand::Conversation(_) => RingingChannel::Conversation,
            RingingCommand::Tool(_) => RingingChannel::Tool,
        }
    }
}

impl From<qaqh_domain::DomainCommand> for RingingCommand {
    fn from(c: qaqh_domain::DomainCommand) -> Self {
        match c {
            qaqh_domain::DomainCommand::Control(inner) => RingingCommand::Control(inner),
            qaqh_domain::DomainCommand::Conversation(inner) => RingingCommand::Conversation(inner),
            qaqh_domain::DomainCommand::Tool(inner) => RingingCommand::Tool(inner),
        }
    }
}
