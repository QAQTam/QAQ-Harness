//! Ringing envelope：事件/命令信封、ack、batch。

use qaqh_domain::{Delivery, RingingChannel};
use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
use crate::event::RingingEvent;
use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION, is_safe_integer};

/// 事件信封（PLAN 固定字段）。
///
/// M4 瘦身：`schema`/`version`/`channel`/`server_epoch` 已从信封移除——
/// 版本由端点 URL 承担，epoch/channel 由 SSE 帧 id 承担，batch 级字段承担
/// 聚合上下文；`seed` 保留（单频道连接承载多 seed，必须逐事件路由）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingEventEnvelope {
    /// 可靠性等级：由领域事件定义显式声明，wire 不决定。
    pub delivery: Delivery,
    /// 会话标识。
    pub seed: String,
    /// 每 (server_epoch, channel) 全局递增，供单条 SSE 连接恢复。
    pub stream_seq: u64,
    /// 每 (seed, channel) 递增，供领域状态乱序检测。
    pub channel_seq: u64,
    /// 每 session/channel 因果序（保留 legacy session_seq 语义）。
    pub session_seq: u64,
    /// 事件唯一 id；同 id 至少一次投递但只允许应用一次（幂等）。
    pub event_id: String,
    /// 因果来源（如 command_id），用于关联命令与业务终态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    /// 关联链 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// 领域状态修订号；terminal 到达后旧 revision 的 replaceable 立即作废。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_revision: Option<u64>,
    /// 服务器发布时间（unix 毫秒）。诊断/遥测用：配合客户端本地到达时间
    /// 可测端到端延迟（provider → daemon → SSE → drain → 渲染），定位
    /// 流式"攒感"在链路的哪一段。可选——旧事件/回放不保证存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_ts: Option<u64>,
    pub event: RingingEvent,
}

impl RingingEventEnvelope {
    /// 构造信封并强制 channel 与事件一致。
    pub fn new(
        seed: impl Into<String>,
        stream_seq: u64,
        channel_seq: u64,
        session_seq: u64,
        event_id: impl Into<String>,
        event: RingingEvent,
    ) -> Self {
        let delivery = event.delivery();
        Self {
            delivery,
            seed: seed.into(),
            stream_seq,
            channel_seq,
            session_seq,
            event_id: event_id.into(),
            causation_id: None,
            correlation_id: None,
            state_revision: None,
            server_ts: None,
            event,
        }
    }

    pub fn with_causation(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }

    pub fn with_state_revision(mut self, revision: u64) -> Self {
        self.state_revision = Some(revision);
        self
    }

    pub fn with_server_ts(mut self, unix_ms: u64) -> Self {
        self.server_ts = Some(unix_ms);
        self
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.seed.is_empty()
            || self.event_id.is_empty()
            || !is_safe_integer(self.stream_seq)
            || !is_safe_integer(self.channel_seq)
            || !is_safe_integer(self.session_seq)
            || self.state_revision.is_some_and(|v| !is_safe_integer(v))
        {
            return Err("invalid_envelope");
        }
        Ok(())
    }
}

/// 命令信封（PLAN 固定字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingCommandEnvelope {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    /// 命令幂等 id：accepted 前断线可安全重试；accepted 后不得重复执行。
    pub command_id: String,
    /// 发起客户端实例 id（lease 绑定该身份）。
    pub client_instance_id: String,
    /// open 成功后由 daemon 签发的连接级身份。
    pub client_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    /// 乐观并发修订（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    pub command: RingingCommand,
}

impl RingingCommandEnvelope {
    pub fn new(
        command_id: impl Into<String>,
        client_instance_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        let channel = command.channel();
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel,
            command_id: command_id.into(),
            client_instance_id: client_instance_id.into(),
            client_session_id: String::new(),
            seed: None,
            expected_revision: None,
            command,
        }
    }

    pub fn with_seed(mut self, seed: impl Into<String>) -> Self {
        self.seed = Some(seed.into());
        self
    }

    pub fn with_client_session_id(mut self, client_session_id: impl Into<String>) -> Self {
        self.client_session_id = client_session_id.into();
        self
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA || self.version != RINGING_VERSION {
            return Err("unsupported_version");
        }
        if self.command.channel() != self.channel {
            return Err("invalid_envelope");
        }
        if self.command_id.is_empty() || self.client_instance_id.is_empty() {
            return Err("invalid_envelope");
        }
        if self.client_session_id.is_empty() {
            return Err("lease_required");
        }
        if self.seed.as_deref().is_some_and(str::is_empty) {
            return Err("invalid_envelope");
        }
        if self.seed.is_none()
            && !matches!(
                self.command,
                RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate { .. })
            )
        {
            return Err("missing_seed");
        }
        if self.expected_revision.is_some_and(|v| !is_safe_integer(v)) {
            return Err("invalid_envelope");
        }
        Ok(())
    }
}

/// 命令确认：accepted 仅代表校验通过并进入正确 actor/worker，
/// 业务完成必须通过 `causation_id = command_id` 的可靠事件返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingCommandAckStatus {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingCommandAck {
    pub command_id: String,
    pub status: RingingCommandAckStatus,
    /// 稳定错误码（rejected 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 限流/退避提示（rejected 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// 可持久化的命令执行状态。ACK 丢失时客户端用原 command_id 查询它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingCommandState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingCommandStatus {
    pub command_id: String,
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// 事件批次（main→renderer 必须整 batch 传递，禁止展开为逐事件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingEventBatch {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    pub seed: String,
    pub server_epoch: String,
    pub from_stream_seq: u64,
    pub to_stream_seq: u64,
    pub envelopes: Vec<RingingEventEnvelope>,
}

impl RingingEventBatch {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA || self.version != RINGING_VERSION {
            return Err("unsupported_version");
        }
        if self.from_stream_seq > self.to_stream_seq || self.envelopes.is_empty() {
            return Err("invalid_envelope");
        }
        if !is_safe_integer(self.from_stream_seq) || !is_safe_integer(self.to_stream_seq) {
            return Err("invalid_envelope");
        }
        let expected_to = self
            .from_stream_seq
            .checked_add(self.envelopes.len() as u64 - 1)
            .ok_or("invalid_envelope")?;
        if self.to_stream_seq != expected_to {
            return Err("invalid_envelope");
        }
        for (index, envelope) in self.envelopes.iter().enumerate() {
            envelope.validate()?;
            if envelope.seed != self.seed
                || envelope.stream_seq
                    != self
                        .from_stream_seq
                        .checked_add(index as u64)
                        .ok_or("invalid_envelope")?
            {
                return Err("invalid_envelope");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, ToolEvent};

    #[test]
    fn event_envelope_round_trip() {
        let event = RingingEvent::Tool(qaqh_domain::ToolEvent::ToolProgress {
            tool_call_id: "c1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            stream: "stdout".into(),
            seq_start: 0,
            seq_end: 2,
            chunk: "hi".into(),
            dropped_bytes: 0,
            truncated: false,
        });
        let env = RingingEventEnvelope::new("seed-1", 5, 3, 2, "evt-1", event)
            .with_causation("cmd-9")
            .with_state_revision(7);

        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("\"delivery\":\"replaceable\""));
        assert!(json.contains("\"causation_id\":\"cmd-9\""));
        assert!(json.contains("\"state_revision\":7"));

        let back: RingingEventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.event.channel(), RingingChannel::Tool);
        assert_eq!(back.delivery, Delivery::Replaceable);
        assert!(matches!(
            back.event,
            RingingEvent::Tool(ToolEvent::ToolProgress { .. })
        ));
    }

    #[test]
    fn command_envelope_round_trip() {
        use qaqh_domain::{ConversationCommand, ConversationMode};
        let cmd = RingingCommand::Conversation(ConversationCommand::ConversationSetMode {
            mode: ConversationMode::Plan,
        });
        let env = RingingCommandEnvelope::new("cmd-1", "client-a", cmd).with_seed("s1");
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("\"command_id\":\"cmd-1\""));
        let back: RingingCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.channel, RingingChannel::Conversation);
    }

    #[test]
    fn ack_round_trip() {
        let ack = RingingCommandAck {
            command_id: "cmd-1".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: None,
            retry_after_ms: None,
        };
        let json = serde_json::to_string(&ack).expect("serialize");
        assert!(json.contains("\"status\":\"accepted\""));
        let back: RingingCommandAck = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.status, RingingCommandAckStatus::Accepted);
    }

    #[test]
    fn batch_round_trip() {
        let batch = RingingEventBatch {
            channel: RingingChannel::Conversation,
            seed: "s1".into(),
            schema: RINGING_SCHEMA.into(),
            version: RINGING_VERSION,
            from_stream_seq: 1,
            to_stream_seq: 1,
            server_epoch: "e1".into(),
            envelopes: vec![RingingEventEnvelope::new(
                "s1",
                1,
                1,
                1,
                "event-1",
                RingingEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: None,
                }),
            )],
        };
        let json = serde_json::to_string(&batch).expect("serialize");
        let back: RingingEventBatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.envelopes.len(), 1);
        assert_eq!(back.to_stream_seq, 1);
        back.validate().expect("valid batch");
    }
}
