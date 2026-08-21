//! 序号生成器。
//!
//! PLAN 序号规则：
//! - `stream_seq` 在 `server_epoch + channel` 内全局递增（一条 SSE 连接恢复用）。
//! - `channel_seq` 在 `seed + channel` 内递增（领域状态乱序检测）。
//! - `session_seq` 保留因果序（每 seed+channel）。
//! - `state_revision` 每 seed+channel 递增（terminal 到达后旧 revision 作废）。

use std::collections::HashMap;
use std::sync::Mutex;

use qaqh_domain::RingingChannel;

#[derive(Debug, Default)]
struct PerSeed {
    channel_seq: u64,
    session_seq: u64,
    state_revision: u64,
}

/// 线程安全序号生成器（daemon 内多线程消费）。
#[derive(Debug, Default)]
pub struct Sequencer {
    stream_seq: Mutex<HashMap<RingingChannel, u64>>,
    per_seed: Mutex<HashMap<(RingingChannel, String), PerSeed>>,
}

impl Sequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从持久化 journal 装载后恢复序号（取历史最大值，`next` 继续递增）。
    pub fn seed(
        &self,
        channel: RingingChannel,
        seed: &str,
        stream_seq: u64,
        channel_seq: u64,
        session_seq: u64,
    ) {
        let mut streams = self.stream_seq.lock().unwrap_or_else(|e| e.into_inner());
        let entry = streams.entry(channel).or_default();
        *entry = (*entry).max(stream_seq);
        drop(streams);

        let mut per = self.per_seed.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry((channel, seed.to_string())).or_default();
        entry.channel_seq = entry.channel_seq.max(channel_seq);
        entry.session_seq = entry.session_seq.max(session_seq);
    }

    /// 分配一组序号（stream/channel/session 各自独立递增）。
    pub fn next(&self, channel: RingingChannel, seed: &str) -> (u64, u64, u64) {
        let mut streams = self.stream_seq.lock().unwrap_or_else(|e| e.into_inner());
        let s = streams.entry(channel).or_default();
        *s = s.saturating_add(1);
        let stream_seq = *s;
        drop(streams);

        let mut per = self.per_seed.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry((channel, seed.to_string())).or_default();
        entry.channel_seq = entry.channel_seq.saturating_add(1);
        entry.session_seq = entry.session_seq.saturating_add(1);
        (stream_seq, entry.channel_seq, entry.session_seq)
    }

    /// 领域状态修订号递增（terminal / revision 变更事件时调用）。
    pub fn bump_revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        let mut per = self.per_seed.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry((channel, seed.to_string())).or_default();
        entry.state_revision = entry.state_revision.saturating_add(1);
        entry.state_revision
    }

    pub fn current_revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        let per = self.per_seed.lock().unwrap_or_else(|e| e.into_inner());
        per.get(&(channel, seed.to_string()))
            .map(|e| e.state_revision)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_are_per_channel_and_per_seed() {
        let seq = Sequencer::new();
        let (s1, c1, ss1) = seq.next(RingingChannel::Tool, "a");
        let (s2, c2, ss2) = seq.next(RingingChannel::Tool, "a");
        let (s3, c3, _) = seq.next(RingingChannel::Tool, "b");
        let (s4, c4, _) = seq.next(RingingChannel::Control, "a");
        assert_eq!((s1, c1, ss1), (1, 1, 1));
        assert_eq!((s2, c2, ss2), (2, 2, 2));
        assert_eq!((s3, c3), (3, 1)); // seed b 独立 channel_seq
        assert_eq!((s4, c4), (1, 1)); // Control 频道独立 stream_seq
    }

    #[test]
    fn revision_is_per_seed_channel() {
        let seq = Sequencer::new();
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "s"), 1);
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "s"), 2);
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "t"), 1);
        assert_eq!(seq.current_revision(RingingChannel::Conversation, "s"), 2);
        assert_eq!(seq.current_revision(RingingChannel::Tool, "s"), 0);
    }
}
