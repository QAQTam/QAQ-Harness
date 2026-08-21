//! SSE 恢复指令：cursor 超出可靠 journal 保留窗口时的 `ringing.reset_required`。
//!
//! 该指令不是领域事件，不进入 snapshot/journal；客户端收到后必须经 HTTP
//! 读取对应频道的权威 snapshot，并以 snapshot 的 `baseline_stream_seq` 继续。

use serde::{Deserialize, Serialize};

use qaqh_domain::RingingChannel;

/// `event: ringing.reset_required` 的 data payload。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingResetRequired {
    pub channel: RingingChannel,
    /// 需要重新拉取 snapshot 的会话。
    pub seed: String,
    /// 服务端该 seed+channel 仍可回放的最早 stream_seq。
    pub earliest_available_seq: u64,
}

impl RingingResetRequired {
    pub fn new(
        channel: RingingChannel,
        seed: impl Into<String>,
        earliest_available_seq: u64,
    ) -> Self {
        Self {
            channel,
            seed: seed.into(),
            earliest_available_seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_required_round_trip() {
        let reset = RingingResetRequired::new(RingingChannel::Tool, "s1", 42);
        let json = serde_json::to_string(&reset).expect("serialize");
        assert!(json.contains("\"channel\":\"tool\""));
        assert!(json.contains("\"seed\":\"s1\""));
        assert!(json.contains("\"earliest_available_seq\":42"));
        let back: RingingResetRequired = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, reset);
    }
}
