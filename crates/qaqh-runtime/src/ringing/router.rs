//! 每频道 ChannelRouter：可靠队列 + replaceable slots。
//!
//! PLAN 硬规则：
//! - `reliable` 事件进入有界队列，必须按 cursor 回放，不能静默丢弃；
//! - `replaceable` 事件按 identity 合并/覆盖（慢消费者只能覆盖 progress，
//!   不能阻塞或丢弃 terminal）；
//! - terminal 发送前必须 flush/覆盖同 identity 的 replaceable 事件。

use std::collections::{HashMap, VecDeque};

use qaqh_domain::{Delivery, RingingChannel};
use qaqh_ringing::{RingingEvent, RingingEventEnvelope};

/// replaceable 合并键（identity）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReplaceableKey {
    /// 按工具进度合并（tool_call_id）。
    ToolProgress(String),
    /// 按回合/轮次/块种类合并（turn_id, round_num, kind 语义由调用方编码进 key）。
    RoundDelta(String),
    /// 按回合/轮次/块种类合并的完整值 checkpoint（与 RoundDelta 同 identity
    /// 空间，但语义为覆盖；`RoundCompleted` 时两者一并作废）。
    BlockCheckpoint(String),
    /// 按 usage 身份合并（turn_id:round_num）。
    Usage(String),
    /// 按 provider 工具状态合并（call_id）。
    ProviderTool(String),
    /// 按 compact 进度合并（compact_id）。
    CompactProgress(String),
    /// 按仪表盘合并（seed）。
    Dashboard(String),
    /// 其他（显式 key）。
    Other(String),
}

impl ReplaceableKey {
    pub fn tool_progress(tool_call_id: &str) -> Self {
        ReplaceableKey::ToolProgress(tool_call_id.to_string())
    }

    pub fn round_delta(turn_id: &str, round_num: u32, kind: &str) -> Self {
        ReplaceableKey::RoundDelta(format!("{turn_id}:{round_num}:{kind}"))
    }

    pub fn block_checkpoint(turn_id: &str, round_num: u32, kind: &str) -> Self {
        ReplaceableKey::BlockCheckpoint(format!("{turn_id}:{round_num}:{kind}"))
    }

    pub fn usage(turn_id: &str, round_num: u32) -> Self {
        ReplaceableKey::Usage(format!("{turn_id}:{round_num}"))
    }

    pub fn provider_tool(call_id: &str) -> Self {
        ReplaceableKey::ProviderTool(call_id.to_string())
    }

    pub fn compact_progress(compact_id: &str) -> Self {
        ReplaceableKey::CompactProgress(compact_id.to_string())
    }

    pub fn dashboard(seed: &str) -> Self {
        ReplaceableKey::Dashboard(seed.to_string())
    }
}

/// 领域事件 → replaceable 合并键（事件自身决定，wire 不解释）。
pub fn replaceable_key_for(event: &RingingEvent) -> Option<ReplaceableKey> {
    use qaqh_domain::{ConversationEvent as CE, ToolEvent as TE};
    match event {
        RingingEvent::Tool(TE::ToolProgress { tool_call_id, .. }) => {
            Some(ReplaceableKey::tool_progress(tool_call_id))
        }
        RingingEvent::Tool(TE::ToolCallPrepared { tool_call_id, .. }) => {
            Some(ReplaceableKey::tool_progress(tool_call_id))
        }
        RingingEvent::Conversation(CE::RoundDelta {
            turn_id,
            round_num,
            kind,
            ..
        }) => Some(ReplaceableKey::round_delta(
            turn_id,
            *round_num,
            match kind {
                qaqh_domain::RoundDeltaKind::Thinking => "thinking",
                qaqh_domain::RoundDeltaKind::ToolCalling => "tool_calling",
                qaqh_domain::RoundDeltaKind::Answering => "answering",
            },
        )),
        RingingEvent::Conversation(CE::BlockCheckpoint {
            turn_id,
            round_num,
            kind,
            ..
        }) => Some(ReplaceableKey::block_checkpoint(
            turn_id,
            *round_num,
            match kind {
                qaqh_domain::RoundDeltaKind::Thinking => "thinking",
                qaqh_domain::RoundDeltaKind::ToolCalling => "tool_calling",
                qaqh_domain::RoundDeltaKind::Answering => "answering",
            },
        )),
        RingingEvent::Conversation(CE::UsageUpdated {
            turn_id, round_num, ..
        }) => Some(ReplaceableKey::usage(turn_id, *round_num)),
        RingingEvent::Conversation(CE::ProviderToolStatus { call_id, .. }) => {
            Some(ReplaceableKey::provider_tool(call_id))
        }
        RingingEvent::Conversation(CE::CompactProgress { compact_id, .. }) => {
            Some(ReplaceableKey::compact_progress(compact_id))
        }
        RingingEvent::Control(qaqh_domain::ControlEvent::DashboardUpdated { .. }) => {
            Some(ReplaceableKey::Dashboard("global".into()))
        }
        _ => None,
    }
}

/// Reliable terminal events invalidate the replaceable identities that fed
/// them. This prevents a reconnect/restart from replaying stale progress after
/// the authoritative terminal state.
pub fn terminal_replaceable_keys(event: &RingingEvent) -> Vec<ReplaceableKey> {
    use qaqh_domain::{ConversationEvent as CE, ToolEvent as TE};
    match event {
        RingingEvent::Tool(TE::ToolFinished { tool_call_id, .. }) => {
            vec![ReplaceableKey::tool_progress(tool_call_id)]
        }
        RingingEvent::Conversation(CE::RoundCompleted {
            turn_id, round_num, ..
        }) => {
            let mut keys: Vec<ReplaceableKey> = ["thinking", "tool_calling", "answering"]
                .into_iter()
                .flat_map(|kind| {
                    [
                        ReplaceableKey::round_delta(turn_id, *round_num, kind),
                        ReplaceableKey::block_checkpoint(turn_id, *round_num, kind),
                    ]
                })
                .collect();
            keys.dedup();
            keys
        }
        RingingEvent::Conversation(CE::CompactFinished { compact_id, .. }) => {
            vec![ReplaceableKey::compact_progress(compact_id)]
        }
        _ => Vec::new(),
    }
}

/// 路由器对单个事件的入队结果。
#[derive(Debug)]
pub enum RouteOutcome {
    /// 已入队（reliable 或 replaceable 覆盖）。
    Routed { envelope: RingingEventEnvelope },
    /// reliable 队列已满且无 replaceable 可腾退——调用方应背压。
    Backpressure,
}

const DEFAULT_RELIABLE_CAPACITY: usize = 4096;
const DEFAULT_REPLACEABLE_SLOTS: usize = 512;

/// 每 (channel, seed) 的可靠队列与 replaceable slots。
#[derive(Debug)]
pub struct ChannelRouter {
    channel: RingingChannel,
    reliable: VecDeque<RingingEventEnvelope>,
    replaceable: HashMap<ReplaceableKey, RingingEventEnvelope>,
    /// replaceable 插入顺序（FIFO 淘汰"最旧 identity"）。
    replaceable_order: VecDeque<ReplaceableKey>,
    reliable_capacity: usize,
    replaceable_slots: usize,
}

impl ChannelRouter {
    pub fn new(channel: RingingChannel) -> Self {
        Self {
            channel,
            reliable: VecDeque::new(),
            replaceable: HashMap::new(),
            replaceable_order: VecDeque::new(),
            reliable_capacity: DEFAULT_RELIABLE_CAPACITY,
            replaceable_slots: DEFAULT_REPLACEABLE_SLOTS,
        }
    }

    pub fn with_limits(
        channel: RingingChannel,
        reliable_capacity: usize,
        replaceable_slots: usize,
    ) -> Self {
        Self {
            channel,
            reliable: VecDeque::new(),
            replaceable: HashMap::new(),
            replaceable_order: VecDeque::new(),
            reliable_capacity: reliable_capacity.max(1),
            replaceable_slots: replaceable_slots.max(1),
        }
    }

    pub fn channel(&self) -> RingingChannel {
        self.channel
    }

    /// 入队一个已构造的信封（delivery 由事件定义声明）。
    pub fn route(&mut self, envelope: RingingEventEnvelope) -> RouteOutcome {
        match envelope.delivery {
            Delivery::Reliable => self.push_reliable(envelope),
            Delivery::Replaceable => RouteOutcome::Routed {
                envelope: self.push_replaceable(envelope),
            },
            Delivery::Ephemeral => {
                // ephemeral 不入队；透传给调用方（outbox 直接转发给当前消费者）
                RouteOutcome::Routed { envelope }
            }
        }
    }

    fn push_reliable(&mut self, envelope: RingingEventEnvelope) -> RouteOutcome {
        if self.reliable.len() >= self.reliable_capacity {
            // 尝试腾退 replaceable 槽位
            if self.replaceable_order.is_empty() {
                return RouteOutcome::Backpressure;
            }
            self.evict_oldest_replaceable();
        }
        self.reliable.push_back(envelope.clone());
        RouteOutcome::Routed { envelope }
    }

    fn push_replaceable(&mut self, envelope: RingingEventEnvelope) -> RingingEventEnvelope {
        let key = match replaceable_key_for(&envelope.event) {
            Some(key) => key,
            // 无合并键的 replaceable（异常/未来类型）：不入队，原样透传
            None => return envelope,
        };
        let is_new = !self.replaceable.contains_key(&key);
        if is_new {
            if self.replaceable.len() >= self.replaceable_slots {
                // 满且新 identity：淘汰最旧
                self.evict_oldest_replaceable();
            }
            self.replaceable_order.push_back(key.clone());
        }
        self.replaceable.insert(key, envelope.clone());
        envelope
    }

    /// 淘汰最旧 identity 的 replaceable（FIFO）。
    fn evict_oldest_replaceable(&mut self) {
        if let Some(victim) = self.replaceable_order.pop_front() {
            self.replaceable.remove(&victim);
        }
    }

    /// 从 cursor 之后回放可靠事件（含已存在的 replaceable 当前值）。
    pub fn replay_since(&self, after_stream_seq: u64) -> Vec<RingingEventEnvelope> {
        let mut out: Vec<RingingEventEnvelope> = self
            .reliable
            .iter()
            .filter(|e| e.stream_seq > after_stream_seq)
            .cloned()
            .collect();
        // replaceable 最新值追加（慢消费者恢复时拿到当前快照增量）
        for env in self.replaceable.values() {
            out.push(env.clone());
        }
        out
    }

    /// terminal 到达前 flush：返回该 identity 的 replaceable 最终值（覆盖式）。
    pub fn flush_replaceable(&mut self, key: &ReplaceableKey) -> Option<RingingEventEnvelope> {
        self.replaceable_order.retain(|k| k != key);
        self.replaceable.remove(key)
    }

    pub fn last_stream_seq(&self) -> u64 {
        self.reliable.back().map(|e| e.stream_seq).unwrap_or(0)
    }

    pub fn reliable_len(&self) -> usize {
        self.reliable.len()
    }

    pub fn replaceable_len(&self) -> usize {
        self.replaceable.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, DomainEvent, ToolEvent};

    fn env_for(seed: &str, seq: u64, event: DomainEvent) -> RingingEventEnvelope {
        RingingEventEnvelope::new(seed, seq, seq, seq, format!("e{seq}"), event.into())
    }

    #[test]
    fn replaceable_progress_is_covered_by_identity() {
        let mut router = ChannelRouter::new(RingingChannel::Tool);
        let ev = |seq: u64, chunk: &str| {
            env_for(
                "s",
                seq,
                DomainEvent::Tool(ToolEvent::ToolProgress {
                    tool_call_id: "c1".into(),
                    turn_id: "t1".into(),
                    round_num: 0,
                    stream: "stdout".into(),
                    seq_start: 0,
                    seq_end: seq,
                    chunk: chunk.into(),
                    dropped_bytes: 0,
                    truncated: false,
                }),
            )
        };
        router.push_replaceable(ev(1, "a"));
        router.push_replaceable(ev(2, "ab"));
        router.push_replaceable(ev(3, "abc"));
        assert_eq!(router.replaceable_len(), 1, "same identity covers");
        let tail = router.replay_since(0);
        assert_eq!(tail.len(), 1);
        let RingingEventEnvelope { event, .. } = &tail[0];
        match event {
            RingingEvent::Tool(ToolEvent::ToolProgress { chunk, .. }) => {
                assert_eq!(chunk, "abc")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reliable_replay_preserves_order_and_cursor() {
        let mut router = ChannelRouter::new(RingingChannel::Conversation);
        let ev = |seq: u64| {
            env_for(
                "s",
                seq,
                DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: None,
                }),
            )
        };
        router.push_reliable(ev(1));
        router.push_reliable(ev(2));
        router.push_reliable(ev(3));
        let after1 = router.replay_since(1);
        assert_eq!(after1.len(), 2);
        assert_eq!(after1[0].stream_seq, 2);
        assert_eq!(after1[1].stream_seq, 3);
    }

    #[test]
    fn reliable_backpressure_when_full_and_no_replaceable() {
        let mut router = ChannelRouter::with_limits(RingingChannel::Tool, 16, 16);
        let ev = |seq: u64| {
            env_for(
                "s",
                seq,
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("c{seq}"),
                    turn_id: "t".into(),
                    round_num: 0,
                    name: "exec".into(),
                }),
            )
        };
        for seq in 1..=16 {
            assert!(matches!(
                router.push_reliable(ev(seq)),
                RouteOutcome::Routed { .. }
            ));
        }
        // 满 + 无 replaceable → 背压
        assert!(matches!(
            router.push_reliable(ev(17)),
            RouteOutcome::Backpressure
        ));
    }

    #[test]
    fn flush_replaceable_removes_slot() {
        let mut router = ChannelRouter::new(RingingChannel::Tool);
        let ev = |seq: u64, chunk: &str| {
            env_for(
                "s",
                seq,
                DomainEvent::Tool(ToolEvent::ToolProgress {
                    tool_call_id: "c1".into(),
                    turn_id: "t".into(),
                    round_num: 0,
                    stream: "stdout".into(),
                    seq_start: 0,
                    seq_end: seq,
                    chunk: chunk.into(),
                    dropped_bytes: 0,
                    truncated: false,
                }),
            )
        };
        router.push_replaceable(ev(1, "tail"));
        let flushed = router.flush_replaceable(&ReplaceableKey::tool_progress("c1"));
        assert!(flushed.is_some());
        assert_eq!(router.replaceable_len(), 0);
        // terminal 前 flush 后回放不再包含旧 progress
        assert!(router.replay_since(0).is_empty());
    }

    #[test]
    fn replaceable_slots_evict_oldest_identity() {
        let mut router = ChannelRouter::with_limits(RingingChannel::Tool, 16, 4);
        let ev = |seq: u64| {
            env_for(
                "s",
                seq,
                DomainEvent::Tool(ToolEvent::ToolProgress {
                    tool_call_id: format!("c{seq}"),
                    turn_id: "t".into(),
                    round_num: 0,
                    stream: "stdout".into(),
                    seq_start: 0,
                    seq_end: seq,
                    chunk: "x".into(),
                    dropped_bytes: 0,
                    truncated: false,
                }),
            )
        };
        for seq in 1..=5 {
            router.push_replaceable(ev(seq));
        }
        assert_eq!(router.replaceable_len(), 4);
        // 最早 identity c1 被逐出
        assert!(
            router
                .flush_replaceable(&ReplaceableKey::tool_progress("c1"))
                .is_none()
        );
    }

    #[test]
    fn round_delta_key_includes_kind() {
        let key1 = ReplaceableKey::round_delta("t1", 0, "answering");
        let key2 = ReplaceableKey::round_delta("t1", 0, "thinking");
        let key3 = ReplaceableKey::round_delta("t1", 0, "answering");
        assert_ne!(key1, key2);
        assert_eq!(key1, key3);
    }

    #[test]
    fn block_checkpoint_is_covered_by_identity_and_replays_latest() {
        let mut router = ChannelRouter::new(RingingChannel::Conversation);
        let checkpoint = |seq: u64, text: &str| {
            env_for(
                "s",
                seq,
                DomainEvent::Conversation(ConversationEvent::BlockCheckpoint {
                    turn_id: "t1".into(),
                    round_num: 0,
                    kind: qaqh_domain::RoundDeltaKind::Answering,
                    text: text.into(),
                    char_count: text.len() as u32,
                }),
            )
        };
        router.route(checkpoint(1, "partial"));
        router.route(checkpoint(2, "partial-later"));
        router.route(checkpoint(3, "complete-value"));
        assert_eq!(router.replaceable_len(), 1, "same identity covers");
        let replay = router.replay_since(0);
        assert_eq!(
            replay.len(),
            1,
            "slow consumer gets the latest complete value"
        );
        match &replay[0].event {
            RingingEvent::Conversation(ConversationEvent::BlockCheckpoint { text, .. }) => {
                assert_eq!(text, "complete-value");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn round_completed_invalidates_delta_and_checkpoint_keys() {
        let event = RingingEvent::Conversation(ConversationEvent::RoundCompleted {
            turn_id: "t1".into(),
            round_num: 2,
            thinking: Some("t".into()),
            answer: Some("a".into()),
            output_ref: None,
            is_final: true,
        });
        let keys = terminal_replaceable_keys(&event);
        assert_eq!(keys.len(), 6);
        assert!(keys.contains(&ReplaceableKey::round_delta("t1", 2, "answering")));
        assert!(keys.contains(&ReplaceableKey::block_checkpoint("t1", 2, "thinking")));
        assert!(keys.contains(&ReplaceableKey::block_checkpoint("t1", 2, "answering")));
    }
}
