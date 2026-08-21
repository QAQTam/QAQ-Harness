//! 可靠事件 journal（有界，内存实现）。
//!
//! PLAN 硬规则：
//! - journal 只保存 **reliable** 事件与稀疏 progress checkpoint，
//!   禁止保存每个 provider token；
//! - 相同 `event_id` 至少一次投递但只允许应用一次（幂等）；
//! - cursor 超出保留窗口时发送 `ringing.reset_required`，客户端经 HTTP
//!   读取权威 snapshot（本层返回 `CursorExpired` 信号）。

use std::collections::{HashMap, VecDeque};

use qaqh_domain::ConversationEvent;
use qaqh_ringing::{RingingEvent, RingingEventEnvelope};

/// cursor 超出保留窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorExpired {
    pub earliest_available_seq: u64,
}

const DEFAULT_JOURNAL_CAPACITY: usize = 8192;

/// 有界可靠 journal（每 seed+channel 一个实例）。
#[derive(Debug)]
pub struct ReliableJournal {
    entries: VecDeque<RingingEventEnvelope>,
    /// event_id 去重（有界：只保留窗口内）。
    seen_event_ids: HashMap<String, u64>,
    /// 稀疏 replaceable checkpoint：identity → 最新 stream_seq。
    checkpoints: HashMap<String, u64>,
    /// 已从窗口前端淘汰的最大 stream_seq（0 = 从未淘汰）。
    ///
    /// 全局 stream_seq 由多个 seed 共享，某 seed 的首事件可能远大于客户端
    /// cursor；只有"该 seed 确有被淘汰且 seq > cursor 的事件"才意味着
    /// cursor 过期（需要 `ringing.reset_required`）。
    evicted_through: u64,
    capacity: usize,
}

impl ReliableJournal {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_JOURNAL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            seen_event_ids: HashMap::new(),
            checkpoints: HashMap::new(),
            evicted_through: 0,
            capacity: capacity.max(1),
        }
    }

    /// 追加可靠事件。返回是否首次出现（幂等语义：重复 event_id 拒绝）。
    pub fn append(&mut self, envelope: &RingingEventEnvelope) -> AppendOutcome {
        if self.seen_event_ids.contains_key(&envelope.event_id) {
            return AppendOutcome::Duplicate;
        }
        while self.entries.len() >= self.capacity {
            let evicted = self
                .entries
                .pop_front()
                .expect("non-empty while len >= capacity");
            self.evicted_through = self.evicted_through.max(evicted.stream_seq);
            self.seen_event_ids.remove(&evicted.event_id);
        }
        self.seen_event_ids
            .insert(envelope.event_id.clone(), envelope.stream_seq);
        self.entries.push_back(envelope.clone());
        AppendOutcome::Appended
    }

    /// 记录 replaceable checkpoint（稀疏：terminal 前或周期性调用）。
    pub fn checkpoint_replaceable(&mut self, identity: &str, stream_seq: u64) {
        self.checkpoints.insert(identity.to_string(), stream_seq);
    }

    /// 按 round 压缩流式增量：`RoundCompleted` 携带该 round 的权威全量
    /// （thinking/answer），此前 journal 里的 `RoundDelta` 不再需要回放。
    /// 只移除条目，保留 `seen_event_ids` 幂等窗口（防止重复事件被重新接受）。
    pub fn compact_round_deltas(&mut self, turn_id: &str, round_num: u32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|envelope| {
            !matches!(
                &envelope.event,
                RingingEvent::Conversation(ConversationEvent::RoundDelta {
                    turn_id: t,
                    round_num: r,
                    ..
                }) if t == turn_id && *r == round_num
            )
        });
        before - self.entries.len()
    }

    /// 从 cursor 回放。cursor 早于保留窗口 → `CursorExpired`。
    pub fn replay_since(
        &self,
        after_stream_seq: u64,
    ) -> Result<Vec<RingingEventEnvelope>, CursorExpired> {
        let earliest = self.entries.front().map(|e| e.stream_seq).unwrap_or(0);
        if self.evicted_through > after_stream_seq {
            return Err(CursorExpired {
                earliest_available_seq: earliest,
            });
        }
        Ok(self
            .entries
            .iter()
            .filter(|e| e.stream_seq > after_stream_seq)
            .cloned()
            .collect())
    }

    pub fn checkpoints(&self) -> &HashMap<String, u64> {
        &self.checkpoints
    }

    /// 存活事件（compact 后为已折叠序列，stream_seq 升序）。磁盘重写用。
    pub fn entries(&self) -> impl Iterator<Item = &RingingEventEnvelope> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    Duplicate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, DomainEvent};

    fn env(seq: u64, event_id: &str) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "s",
            seq,
            seq,
            seq,
            event_id,
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None })
                .into(),
        )
    }

    #[test]
    fn duplicate_event_id_rejected() {
        let mut journal = ReliableJournal::new();
        assert_eq!(journal.append(&env(1, "e1")), AppendOutcome::Appended);
        assert_eq!(journal.append(&env(2, "e1")), AppendOutcome::Duplicate);
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn replay_within_window_works() {
        let mut journal = ReliableJournal::new();
        for seq in 1..=5 {
            journal.append(&env(seq, &format!("e{seq}")));
        }
        let tail = journal.replay_since(2).expect("within window");
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].stream_seq, 3);
    }

    #[test]
    fn cursor_before_window_triggers_expired() {
        let mut journal = ReliableJournal::with_capacity(4);
        for seq in 1..=4 {
            journal.append(&env(seq, &format!("e{seq}")));
        }
        // 再追加 2 个，窗口变成 3..=6
        journal.append(&env(5, "e5"));
        journal.append(&env(6, "e6"));
        let err = journal.replay_since(1).expect_err("expired");
        assert_eq!(err.earliest_available_seq, 3);
    }

    #[test]
    fn late_seed_without_eviction_replays_fully() {
        // 全局 stream_seq 下，某 seed 的首事件可能是 2/3/…；cursor=0 时
        // 该 seed 没有淘汰过任何事件，必须完整回放而不是误报 reset。
        let mut journal = ReliableJournal::new();
        journal.append(&env(2, "e2"));
        journal.append(&env(5, "e5"));
        let tail = journal.replay_since(0).expect("no eviction, full replay");
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].stream_seq, 2);
        assert_eq!(tail[1].stream_seq, 5);
    }

    #[test]
    fn checkpoint_tracks_replaceable_sparse() {
        let mut journal = ReliableJournal::new();
        journal.checkpoint_replaceable("tool:c1", 10);
        journal.checkpoint_replaceable("tool:c1", 20);
        assert_eq!(journal.checkpoints().get("tool:c1"), Some(&20));
    }

    #[test]
    fn round_deltas_are_compacted_on_round_completed() {
        let mut journal = ReliableJournal::new();
        for seq in 1..=3 {
            journal.append(&env(seq, &format!("e{seq}")));
        }
        // 追加一个 RoundDelta（env() 默认是 ConversationCancelled）
        let delta = RingingEventEnvelope::new(
            "s",
            5,
            5,
            5,
            "e5",
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: qaqh_domain::RoundDeltaKind::Thinking,
                delta: "思考".into(),
            })
            .into(),
        );
        journal.append(&delta);
        journal.append(&env(5, "e6"));
        assert_eq!(journal.len(), 5);

        let removed = journal.compact_round_deltas("t1", 0);
        assert_eq!(removed, 1);
        assert_eq!(journal.len(), 4);
        // 幂等窗口保留：同一 event_id 仍被拒绝
        assert_eq!(journal.append(&delta), AppendOutcome::Duplicate);
        // 其他 round / turn 不受影响
        let removed_other = journal.compact_round_deltas("t1", 1);
        assert_eq!(removed_other, 0);
    }
}
