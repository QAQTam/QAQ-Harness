//! 每频道分级发送队列（outbox）。
//!
//! 消费模型（一条 SSE 连接 = 一个消费者）：
//! - `reliable`：FIFO，先到先发，不丢弃；
//! - `replaceable`：仅保留每个 identity 的最新值，慢消费者只丢旧增量；
//! - terminal（reliable）到达时优先于未消费的 replaceable 发送，
//!   并携带 `state_revision` 作废旧 revision 的 replaceable。

use std::collections::{HashMap, VecDeque};

use qaqh_domain::{Delivery, RingingChannel};
use qaqh_ringing::RingingEventEnvelope;

use super::router::ReplaceableKey;

const DEFAULT_RELIABLE_CAPACITY: usize = 4096;

/// 消费者视角的发送队列。
#[derive(Debug)]
pub struct ChannelOutbox {
    channel: RingingChannel,
    reliable: VecDeque<RingingEventEnvelope>,
    replaceable: HashMap<ReplaceableKey, RingingEventEnvelope>,
    /// replaceable 插入顺序（FIFO 淘汰"最旧 identity"）。
    replaceable_order: VecDeque<ReplaceableKey>,
    reliable_capacity: usize,
}

impl ChannelOutbox {
    pub fn new(channel: RingingChannel) -> Self {
        Self {
            channel,
            reliable: VecDeque::new(),
            replaceable: HashMap::new(),
            replaceable_order: VecDeque::new(),
            reliable_capacity: DEFAULT_RELIABLE_CAPACITY,
        }
    }

    pub fn with_limits(channel: RingingChannel, reliable_capacity: usize) -> Self {
        Self {
            channel,
            reliable: VecDeque::new(),
            replaceable: HashMap::new(),
            replaceable_order: VecDeque::new(),
            reliable_capacity: reliable_capacity.max(1),
        }
    }

    pub fn channel(&self) -> RingingChannel {
        self.channel
    }

    /// 入队（按 delivery 分级）。返回 `Ok(())` 或背压信号。
    pub fn enqueue(&mut self, envelope: RingingEventEnvelope) -> Result<(), OutboxFull> {
        match envelope.delivery {
            Delivery::Reliable => self.push_reliable(envelope),
            Delivery::Replaceable => {
                let key = super::router::replaceable_key_for(&envelope.event);
                if let Some(key) = key {
                    if !self.replaceable.contains_key(&key) {
                        self.replaceable_order.push_back(key.clone());
                    }
                    self.replaceable.insert(key, envelope);
                }
                Ok(())
            }
            Delivery::Ephemeral => Ok(()), // 诊断事件直接丢弃（live 提示由调用方透传）
        }
    }

    fn push_reliable(&mut self, envelope: RingingEventEnvelope) -> Result<(), OutboxFull> {
        if self.reliable.len() >= self.reliable_capacity {
            // 慢消费者：先腾退 replaceable 槽位；无可腾退 → 背压
            if self.replaceable_order.is_empty() {
                return Err(OutboxFull);
            }
            if let Some(victim) = self.replaceable_order.pop_front() {
                self.replaceable.remove(&victim);
            }
        }
        self.reliable.push_back(envelope);
        Ok(())
    }

    /// 取下一个待发事件（reliable 优先于 replaceable 的旧值）。
    /// 返回 `(envelope, 是否为 final 批次末尾)`。
    pub fn next(&mut self) -> Option<RingingEventEnvelope> {
        if let Some(env) = self.reliable.pop_front() {
            return Some(env);
        }
        // reliable 空时，按 FIFO 返回一个 replaceable 当前值
        let key = self.replaceable_order.pop_front()?;
        self.replaceable.remove(&key)
    }

    /// terminal 到达时调用：把该 identity 的 replaceable 最终值 flush 到队首
    /// （terminal 前必须 flush/覆盖同 identity 的 replaceable，PLAN 硬规则）。
    pub fn flush_replaceable(&mut self, key: &ReplaceableKey) -> Option<RingingEventEnvelope> {
        self.replaceable_order.retain(|k| k != key);
        self.replaceable.remove(key)
    }

    pub fn pending_reliable(&self) -> usize {
        self.reliable.len()
    }

    pub fn pending_replaceable(&self) -> usize {
        self.replaceable.len()
    }

    /// 丢弃所有已过时 revision 的 replaceable（terminal 携带新 revision 时调用）。
    pub fn drop_stale_replaceable(&mut self, up_to_seq: u64) -> usize {
        let before = self.replaceable.len();
        self.replaceable.retain(|_, env| env.stream_seq > up_to_seq);
        self.replaceable_order
            .retain(|k| self.replaceable.contains_key(k));
        before - self.replaceable.len()
    }
}

/// reliable 队列满且无可腾退 replaceable——调用方应停止投递并等待消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxFull;

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{DomainEvent, ToolEvent};

    fn progress(seq: u64, chunk: &str) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "s",
            seq,
            seq,
            seq,
            format!("e{seq}"),
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
            })
            .into(),
        )
    }

    fn started(seq: u64) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "s",
            seq,
            seq,
            seq,
            format!("e{seq}"),
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: format!("c{seq}"),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            })
            .into(),
        )
    }

    #[test]
    fn reliable_fifo_then_replaceable_latest() {
        let mut outbox = ChannelOutbox::new(RingingChannel::Tool);
        outbox.enqueue(started(1)).expect("ok");
        outbox.enqueue(progress(2, "a")).expect("ok");
        outbox.enqueue(progress(3, "ab")).expect("ok");
        outbox.enqueue(progress(4, "abc")).expect("ok");

        // 第一个是 reliable ToolStarted
        let first = outbox.next().expect("first");
        assert_eq!(first.stream_seq, 1);
        // 之后只有 replaceable 最新值
        let second = outbox.next().expect("second");
        assert_eq!(second.stream_seq, 4);
        assert!(outbox.next().is_none());
    }

    #[test]
    fn backpressure_only_when_replaceable_exhausted() {
        let mut outbox = ChannelOutbox::with_limits(RingingChannel::Tool, 2);
        outbox.enqueue(started(1)).expect("ok");
        outbox.enqueue(started(2)).expect("ok");
        // 无 replaceable 可腾退 → 背压
        assert_eq!(outbox.enqueue(started(3)), Err(OutboxFull));
        // 有 replaceable 时腾退后成功
        outbox.enqueue(progress(4, "x")).expect("ok");
        assert!(outbox.enqueue(started(5)).is_ok());
    }

    #[test]
    fn drop_stale_replaceable_after_terminal_revision() {
        let mut outbox = ChannelOutbox::new(RingingChannel::Tool);
        outbox.enqueue(progress(2, "old")).expect("ok");
        outbox.enqueue(progress(3, "new")).expect("ok");
        let dropped = outbox.drop_stale_replaceable(3);
        assert_eq!(dropped, 1);
        assert_eq!(outbox.pending_replaceable(), 0);
    }

    #[test]
    fn flush_replaceable_returns_final_tail() {
        let mut outbox = ChannelOutbox::new(RingingChannel::Tool);
        outbox.enqueue(progress(5, "final")).expect("ok");
        let flushed = outbox.flush_replaceable(&ReplaceableKey::tool_progress("c1"));
        assert!(flushed.is_some());
        assert_eq!(outbox.pending_replaceable(), 0);
    }
}
