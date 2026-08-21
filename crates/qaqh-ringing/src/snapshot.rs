//! 频道快照（wire 视图）。

use qaqh_domain::RingingChannel;
use serde::{Deserialize, Serialize};

use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 频道领域快照。**必须表达领域状态，禁止用事件数组模拟状态**
/// （PLAN 硬规则）。`state` 为对应频道的领域快照 payload
/// （Conversation/Tool/Control snapshot projection 在 transport 层注入强类型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingChannelSnapshot {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    pub seed: String,
    /// 快照覆盖到的 stream_seq 基线（其后的可靠事件需从 cursor 回放）。
    pub baseline_stream_seq: u64,
    pub state_revision: u64,
    pub snapshot_version: u32,
    pub state: serde_json::Value,
}

impl RingingChannelSnapshot {
    pub fn new(
        channel: RingingChannel,
        seed: impl Into<String>,
        baseline_stream_seq: u64,
        state_revision: u64,
        state: serde_json::Value,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel,
            seed: seed.into(),
            baseline_stream_seq,
            state_revision,
            snapshot_version: 1,
            state,
        }
    }
}

/// 原子恢复一个 session 所需的完整三频道快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingSessionBootstrap {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub control: RingingChannelSnapshot,
    pub conversation: RingingChannelSnapshot,
    pub tool: RingingChannelSnapshot,
}

impl RingingSessionBootstrap {
    pub fn new(
        server_epoch: impl Into<String>,
        seed: impl Into<String>,
        control: RingingChannelSnapshot,
        conversation: RingingChannelSnapshot,
        tool: RingingChannelSnapshot,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            server_epoch: server_epoch.into(),
            seed: seed.into(),
            control,
            conversation,
            tool,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let snap = RingingChannelSnapshot::new(
            RingingChannel::Tool,
            "s1",
            42,
            3,
            serde_json::json!({ "running": [], "pending_permission": null }),
        );
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"schema\":\"qaqh.Ringing\""));
        assert!(json.contains("\"baseline_stream_seq\":42"));
        let back: RingingChannelSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.state_revision, 3);
        assert_eq!(back.snapshot_version, 1);
    }
}
