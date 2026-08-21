//! daemon ↔ agent worker 边界 frame（framed OS pipe，语义与 in-process channel 一致）。

use qaqh_domain::RingingChannel;
use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
use crate::event::RingingEvent;
use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 单 frame 长度上限（字节）。超出必须分帧/拒绝，防内存放大。
pub const WORKER_FRAME_MAX_BYTES: usize = 16 * 1024 * 1024;

/// worker 边界线格式标记（PLAN 阶段 1：新记录必须携带该判别字段）。
/// reader 必须先检查 `wire`，禁止 untagged 猜测。
pub const WIRE_RINGING_DOMAIN_V1: &str = "Ringing_domain_v1";
/// Native Ringing V1 timeline producer frame. Unlike the domain-event wire it has no
/// channel: transcript order is assigned by the daemon's one Timeline writer.
pub const WIRE_RINGING_TIMELINE_INTENT_V1: &str = "Ringing_timeline_intent_v1";

/// frame 方向（stdin 只承载 Command，stdout 只承载 Event；stderr 只承载脱敏日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDirection {
    Command,
    Event,
}

/// daemon → worker 命令 frame。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerCommandEnvelope {
    pub schema: String,
    pub version: u32,
    /// 线格式判别字段，固定 `WIRE_RINGING_DOMAIN_V1`。
    pub wire: String,
    pub direction: WorkerDirection,
    pub channel: RingingChannel,
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
        let channel = command.channel();
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            wire: WIRE_RINGING_DOMAIN_V1.to_string(),
            direction: WorkerDirection::Command,
            channel,
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

/// worker → daemon 事件 frame。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerEventEnvelope {
    pub schema: String,
    pub version: u32,
    /// 线格式判别字段，固定 `WIRE_RINGING_DOMAIN_V1`。
    pub wire: String,
    pub direction: WorkerDirection,
    pub channel: RingingChannel,
    pub seed: String,
    pub event_id: String,
    /// 因果来源 command_id（Ringing 命令执行期间产出的事件携带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub event: RingingEvent,
}

impl RingingWorkerEventEnvelope {
    pub fn new(seed: impl Into<String>, event_id: impl Into<String>, event: RingingEvent) -> Self {
        let channel = event.channel();
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            wire: WIRE_RINGING_DOMAIN_V1.to_string(),
            direction: WorkerDirection::Event,
            channel,
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

/// Worker → daemon Ringing V1 timeline intent. It is intentionally distinct
/// from the per-channel `RingingWorkerEventEnvelope`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingTimelineIntentEnvelope {
    pub schema: String,
    pub version: u32,
    pub wire: String,
    pub direction: WorkerDirection,
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
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            wire: WIRE_RINGING_TIMELINE_INTENT_V1.to_string(),
            direction: WorkerDirection::Event,
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
    fn worker_frames_round_trip() {
        let cmd = RingingCommand::Tool(ToolCommand::ToolInvoke {
            tool_call_id: "c".into(),
            name: "exec".into(),
            action: "run".into(),
            args: serde_json::json!({ "cmd": "echo hi" }),
        });
        let frame = RingingWorkerCommandEnvelope::new("s1", "cmd-1", cmd);
        let json = serde_json::to_string(&frame).expect("serialize");
        assert!(json.contains("\"direction\":\"command\""));
        let back: RingingWorkerCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.channel, RingingChannel::Tool);
        assert_eq!(back.command_id, "cmd-1");
    }
}
