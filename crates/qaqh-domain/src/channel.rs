use serde::{Deserialize, Serialize};

/// Ringing 协议族的频道。
///
/// 每个频道拥有独立的可靠事件流、snapshot、cursor 与消费预算
/// （Control / Conversation / Tool 物理隔离）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingChannel {
    /// 会话/agent 生命周期、interaction、skills、系统通知与失败终态。
    Control,
    /// 消息回合、provider 流式输出、compact 与 usage。
    Conversation,
    /// 工具执行生命周期、进度、权限与审计。
    Tool,
}

impl RingingChannel {
    /// SSE 事件流的 URL 片段（`/ringing/v1/events/{channel}`）。
    pub fn as_str(self) -> &'static str {
        match self {
            RingingChannel::Control => "control",
            RingingChannel::Conversation => "conversation",
            RingingChannel::Tool => "tool",
        }
    }
}

impl std::fmt::Display for RingingChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
