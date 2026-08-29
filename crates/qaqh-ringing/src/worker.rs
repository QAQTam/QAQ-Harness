//! agent 边界消息类型（进程内 channel 按值传递；测试经 JSON-LP 行往返）。
//!
//! 曾经的 framed OS pipe 线格式（schema/version/`wire` 判别、direction、
//! 16MB 帧帽）已随 `Loop::new_ipc` 的退役删除：生产路径为
//! `Loop::from_channels` 的 typed 传递，此处仅保留地址与因果元数据。
//! 频道不再冗余存储——由载荷 `command`/`event` 自身派生。

use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
use crate::event::RingingEvent;

/// daemon → agent 命令消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerCommandEnvelope {
    pub seed: String,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub command: RingingCommand,
}

impl RingingWorkerCommandEnvelope {
    pub fn new(
        seed: impl Into<String>,
        command_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        Self {
            seed: seed.into(),
            command_id: command_id.into(),
            expected_revision: None,
            command,
        }
    }

    pub fn with_expected_revision(mut self, revision: Option<u64>) -> Self {
        self.expected_revision = revision;
        self
    }
}

/// agent → daemon 事件消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerEventEnvelope {
    pub seed: String,
    pub event_id: String,
    /// 因果来源 command_id（Ringing 命令执行期间产出的事件携带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub event: RingingEvent,
}

impl RingingWorkerEventEnvelope {
    pub fn new(seed: impl Into<String>, event_id: impl Into<String>, event: RingingEvent) -> Self {
        Self {
            seed: seed.into(),
            event_id: event_id.into(),
            causation_id: None,
            event,
        }
    }

    pub fn with_causation(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }
}

/// agent → daemon timeline intent（独立于频道事件：顺序由 daemon 唯一
/// Timeline writer 赋予）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingTimelineIntentEnvelope {
    pub seed: String,
    pub intent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub intent: qaqh_domain::TimelineIntent,
}

impl RingingTimelineIntentEnvelope {
    pub fn new(
        seed: impl Into<String>,
        intent_id: impl Into<String>,
        intent: qaqh_domain::TimelineIntent,
    ) -> Self {
        Self {
            seed: seed.into(),
            intent_id: intent_id.into(),
            causation_id: None,
            intent,
        }
    }

    pub fn with_causation(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::ToolCommand;

    #[test]
    fn agent_messages_round_trip() {
        let cmd = RingingCommand::Tool(ToolCommand::ToolInvoke {
            tool_call_id: "c".into(),
            name: "exec".into(),
            action: "run".into(),
            args: serde_json::json!({ "cmd": "echo hi" }),
        });
        let frame = RingingWorkerCommandEnvelope::new("s1", "cmd-1", cmd);
        let json = serde_json::to_string(&frame).expect("serialize");
        let back: RingingWorkerCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.command.channel(), qaqh_domain::RingingChannel::Tool);
        assert_eq!(back.command_id, "cmd-1");
    }
}
