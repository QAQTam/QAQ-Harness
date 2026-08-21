//! ToolProgress 流控器（PLAN Tool 频道流控规则）。
//!
//! - 同一工具/stream 在 16ms 内合并。
//! - 单 batch 不超过 256 KiB。
//! - 每工具最多保留 256 KiB progress tail。
//! - terminal 发送前先 flush 该工具保留的 progress（覆盖式）。
//! - `dropped_bytes` / `truncated` 字段如实维护。

use std::collections::HashMap;
use std::time::Instant;

/// 合并窗口。
pub const COALESCE_WINDOW_MS: u64 = 16;
/// 单 batch 上限。
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
/// 每工具 progress tail 上限。
pub const MAX_TAIL_BYTES: usize = 256 * 1024;

/// 单个工具/stream 的进行中状态。
#[derive(Debug)]
struct StreamState {
    /// 窗口起始时间。
    window_start: Instant,
    /// 窗口内累积 chunk。
    pending: String,
    /// 窗口起始 seq（首个未发 chunk 的序号起点）。
    seq_start: u64,
    /// 本工具累计丢弃字节（合并/截断导致）。
    dropped_bytes: u64,
    /// tail 是否已截断（>256KiB）。
    truncated: bool,
    /// 已 flush 的累计长度（用于 seq_end 计算）。
    flushed_len: u64,
}

/// 合并后的产出（一次性事件数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedProgress {
    pub tool_call_id: String,
    pub stream: String,
    pub seq_start: u64,
    pub seq_end: u64,
    pub chunk: String,
    pub dropped_bytes: u64,
    pub truncated: bool,
}

/// 按 (tool_call_id, stream) 聚合的流控器。
#[derive(Debug, Default)]
pub struct ToolProgressCoalescer {
    streams: HashMap<(String, String), StreamState>,
    /// 最近一次 tick 时间（测试注入用）。
    now: Option<Instant>,
}

impl ToolProgressCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_now(now: Instant) -> Self {
        Self {
            streams: HashMap::new(),
            now: Some(now),
        }
    }

    fn clock(&self) -> Instant {
        self.now.unwrap_or_else(Instant::now)
    }

    /// 注入一块输出。返回是否立即产出（窗口已过 / batch 超限）。
    pub fn push(
        &mut self,
        tool_call_id: &str,
        stream: &str,
        seq: u64,
        chunk: &str,
    ) -> Option<CoalescedProgress> {
        let key = (tool_call_id.to_string(), stream.to_string());
        let now = self.clock();
        let entry = self
            .streams
            .entry(key.clone())
            .or_insert_with(|| StreamState {
                window_start: now,
                pending: String::new(),
                seq_start: seq,
                dropped_bytes: 0,
                truncated: false,
                flushed_len: 0,
            });

        // 窗口过期：先产出累积
        if !entry.pending.is_empty()
            && now.duration_since(entry.window_start).as_millis() as u64 >= COALESCE_WINDOW_MS
        {
            let out = Self::flush_locked(&key, entry, now);
            // The chunk that caused the window rollover belongs to the next
            // window. Keep it pending; returning the previous window must not
            // silently drop the current write.
            entry.seq_start = seq;
            entry.pending.push_str(chunk);
            return Some(out);
        }

        // 追加（含超限截断记账）
        entry.pending.push_str(chunk);
        if entry.pending.len() >= MAX_BATCH_BYTES {
            let out = Self::flush_locked(&key, entry, now);
            return Some(out);
        }
        None
    }

    /// 主动 flush（terminal 前调用）。
    pub fn flush(&mut self, tool_call_id: &str, stream: &str) -> Option<CoalescedProgress> {
        let key = (tool_call_id.to_string(), stream.to_string());
        let now = self.clock();
        let entry = self.streams.get_mut(&key)?;
        if entry.pending.is_empty() {
            return None;
        }
        Self::flush_locked(&key, entry, now).into()
    }

    /// 覆盖式替换（terminal 到达时丢弃未消费 progress）。
    pub fn discard(&mut self, tool_call_id: &str, stream: &str) {
        self.streams
            .remove(&(tool_call_id.to_string(), stream.to_string()));
    }

    fn flush_locked(
        key: &(String, String),
        entry: &mut StreamState,
        now: Instant,
    ) -> CoalescedProgress {
        let seq_end = entry.seq_start + entry.pending.len() as u64;
        let out = CoalescedProgress {
            tool_call_id: key.0.clone(),
            stream: key.1.clone(),
            seq_start: entry.seq_start,
            seq_end,
            chunk: std::mem::take(&mut entry.pending),
            dropped_bytes: entry.dropped_bytes,
            truncated: entry.truncated,
        };
        // 截断 tail 到 256 KiB（超出部分计入 dropped_bytes）
        let out = if out.chunk.len() > MAX_TAIL_BYTES {
            let start = tail_start_boundary(&out.chunk, MAX_TAIL_BYTES);
            let dropped = start as u64;
            let chunk = out.chunk[start..].to_string();
            CoalescedProgress {
                seq_start: out.seq_end.saturating_sub(chunk.len() as u64),
                chunk,
                dropped_bytes: out.dropped_bytes + dropped,
                truncated: true,
                ..out
            }
        } else {
            out
        };
        entry.window_start = now;
        entry.seq_start = seq_end;
        entry.flushed_len = seq_end;
        out
    }
}

fn tail_start_boundary(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return 0;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_within_16ms_window() {
        let t0 = Instant::now();
        let mut c = ToolProgressCoalescer::with_now(t0);
        assert!(c.push("c1", "stdout", 0, "ab").is_none());
        assert!(c.push("c1", "stdout", 2, "cd").is_none());
        // 窗口内无产出
        let out = c.flush("c1", "stdout").expect("flush");
        assert_eq!(out.chunk, "abcd");
        assert_eq!(out.seq_start, 0);
        assert_eq!(out.seq_end, 4);
    }

    #[test]
    fn window_expiry_produces_immediately() {
        let mut c = ToolProgressCoalescer::with_now(Instant::now());
        c.push("c1", "stdout", 0, "ab");
        // 推进 20ms
        c.now = Some(c.clock() + std::time::Duration::from_millis(20));
        let out = c.push("c1", "stdout", 2, "cd").expect("window expired");
        assert_eq!(out.chunk, "ab");
        let next = c.flush("c1", "stdout").expect("rollover chunk retained");
        assert_eq!(next.chunk, "cd");
        assert_eq!((next.seq_start, next.seq_end), (2, 4));
    }

    #[test]
    fn oversize_batch_flushes_early() {
        let mut c = ToolProgressCoalescer::new();
        let big = "x".repeat(MAX_BATCH_BYTES + 4096);
        // 第一次 push 超限立即产出
        let out = c.push("c1", "stdout", 0, &big).expect("oversize flush");
        assert_eq!(out.chunk.len(), MAX_TAIL_BYTES, "tail truncated to 256KiB");
        assert!(out.truncated);
        assert_eq!(
            out.dropped_bytes as usize,
            MAX_BATCH_BYTES + 4096 - MAX_TAIL_BYTES
        );
    }

    #[test]
    fn terminal_discard_drops_pending() {
        let mut c = ToolProgressCoalescer::new();
        c.push("c1", "stdout", 0, "stale");
        c.discard("c1", "stdout");
        assert!(c.flush("c1", "stdout").is_none());
    }

    #[test]
    fn streams_are_independent() {
        let mut c = ToolProgressCoalescer::new();
        c.push("c1", "stdout", 0, "a");
        c.push("c1", "stderr", 0, "e");
        c.push("c2", "stdout", 0, "b");
        let out = c.flush("c1", "stderr").expect("stderr");
        assert_eq!(out.chunk, "e");
        let out = c.flush("c1", "stdout").expect("stdout");
        assert_eq!(out.chunk, "a");
        let out = c.flush("c2", "stdout").expect("c2");
        assert_eq!(out.chunk, "b");
    }

    #[test]
    fn seq_continues_across_flushes() {
        let t0 = Instant::now();
        let mut c = ToolProgressCoalescer::with_now(t0);
        c.push("c1", "stdout", 0, "ab");
        let first = c.flush("c1", "stdout").expect("first");
        assert_eq!((first.seq_start, first.seq_end), (0, 2));
        c.push("c1", "stdout", 2, "cd");
        let second = c.flush("c1", "stdout").expect("second");
        assert_eq!((second.seq_start, second.seq_end), (2, 4));
    }

    #[test]
    fn unicode_oversize_keeps_a_valid_utf8_tail() {
        let mut c = ToolProgressCoalescer::new();
        let big = "界".repeat(MAX_BATCH_BYTES / 3 + 10);
        let out = c.push("c1", "stdout", 0, &big).expect("oversize flush");
        assert!(out.chunk.len() <= MAX_TAIL_BYTES);
        assert_eq!(out.seq_end - out.seq_start, out.chunk.len() as u64);
        assert!(out.truncated);
    }
}
