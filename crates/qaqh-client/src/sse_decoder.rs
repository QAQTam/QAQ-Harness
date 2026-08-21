//! 游标式 SSE 帧解码器（qaqh-client 专用，替代两处 O(n²) 的
//! `split_off`+`extend` 搬移实现）。
//!
//! 背景：旧实现（`sse.rs` 与 `timeline.rs` 各一份 `drain_frames`）对每个
//! 完整帧把剩余字节从缓冲头部搬走（`Vec::split_off` + `extend`），累计
//! O(n²)；且用 `String::from_utf8_lossy` 解码（可能注入 U+FFFD 替换符，
//! 污染中文/emoji 文本）。
//!
//! 本实现按 `\n` 定位行（O(n) 总体、无搬移），在字节层面切分、整行严格
//! UTF-8 解码（非法行跳过，绝不 lossy），空行定界事件帧。与 daemon 发送端
//! `sse_frame()`/`timeline_sse_frame()`（`id:`/`event:`/`data:` + 空行）
//! 完全对齐。
//!
//! 行为与旧实现保持等价：
//! - 无空行时不产出帧（等待后续字节）；
//! - 注释行（`:` 开头）不产出帧（keepalive 帧被自然丢弃）；
//! - 多 `data:` 行聚合为单帧（SSE 规范以单个 `\n` 连接）；
//! - 流结束（EOF）不冲刷残帧——上层对无终帧的流直接报
//!   `SSE stream ended`（与旧实现一致）。

use crate::types::SseFrame;

/// 游标式 SSE 帧解码器。`push` 追加字节，`next_frame` 逐帧产出。
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    /// 已消费前缀长度（未压缩，超过阈值时统一搬移一次摊销 O(n)）。
    consumed: usize,
    /// 当前累积的帧（id/event/data 字段）。
    pending: Option<SseFrame>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            consumed: 0,
            pending: None,
        }
    }

    /// 追加新到达的字节块，并压缩已消费前缀（摊销 O(n)）。
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if self.consumed > 0 {
            self.buf.drain(..self.consumed);
            self.consumed = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// 取下一完整帧。`None` = 暂无完整帧，需等更多数据。
    pub(crate) fn next_frame(&mut self) -> Option<Result<SseFrame, ()>> {
        loop {
            let rel = self.buf[self.consumed..].iter().position(|&b| b == b'\n')?;
            let end = self.consumed + rel;
            let raw = &self.buf[self.consumed..end];
            self.consumed = end + 1;

            let line = match std::str::from_utf8(raw) {
                Ok(line) => line.trim_end(),
                Err(_) => continue, // 非法 UTF-8 行：跳过，绝不 lossy
            };

            if line.is_empty() {
                // 空行 = 帧结束。
                if let Some(frame) = self.pending.take() {
                    return Some(Ok(frame));
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // 注释行（keepalive）
            }

            let frame = self.pending.get_or_insert_with(SseFrame::default);
            if let Some(id) = line.strip_prefix("id:") {
                frame.id = id.trim().to_string();
            } else if let Some(event) = line.strip_prefix("event:") {
                frame.event_type = event.trim().to_string();
            } else if let Some(data) = line.strip_prefix("data:") {
                if !frame.data.is_empty() {
                    frame.data.push('\n');
                }
                frame.data.push_str(data.trim());
            }
            // 其他字段（unknown）忽略。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(frame: SseFrame) -> (String, String, String) {
        (frame.id, frame.event_type, frame.data)
    }

    #[test]
    fn parses_id_event_data_frame() {
        let mut d = SseDecoder::new();
        d.push(b"id: epoch-1:conversation:7\nevent: turn_started\ndata: {\"x\":1}\n\n");
        let frame = d.next_frame().expect("frame").expect("utf-8");
        assert_eq!(fields(frame), ("epoch-1:conversation:7".into(), "turn_started".into(), "{\"x\":1}".into()));
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn frame_split_across_chunks_is_reassembled() {
        let mut d = SseDecoder::new();
        d.push(b"id: e:tool:1\nevent: tool_star");
        assert!(d.next_frame().is_none());
        d.push(b"ted\ndata: {}\n\n");
        let frame = d.next_frame().expect("frame").expect("utf-8");
        assert_eq!(fields(frame), ("e:tool:1".into(), "tool_started".into(), "{}".into()));
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn utf8_char_split_across_chunks_is_not_corrupted() {
        let mut d = SseDecoder::new();
        // "中" = E4 B8 AD，切到两个 push。
        d.push(b"data: {\"t\":\"\xe4\xb8");
        assert!(d.next_frame().is_none());
        d.push(b"\xad\"}\n\n");
        let frame = d.next_frame().expect("frame").expect("utf-8");
        assert_eq!(frame.data, "{\"t\":\"中\"}");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn invalid_utf8_line_is_skipped() {
        let mut d = SseDecoder::new();
        d.push(b"data: \xff\xfe broken\n\n");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn multiple_data_lines_aggregate() {
        let mut d = SseDecoder::new();
        d.push(b"data: first\n");
        assert!(d.next_frame().is_none());
        d.push(b"data: second\n\n");
        let frame = d.next_frame().expect("frame").expect("utf-8");
        assert_eq!(frame.data, "first\nsecond");
    }

    #[test]
    fn keepalive_comment_frames_emit_nothing() {
        let mut d = SseDecoder::new();
        d.push(b": keepalive\n\n");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let mut d = SseDecoder::new();
        d.push(b"data: {\"a\":1}\n\n");
        d.push(b"data: {\"b\":2}\n\n");
        assert_eq!(d.next_frame().expect("frame").expect("utf-8").data, "{\"a\":1}");
        assert_eq!(d.next_frame().expect("frame").expect("utf-8").data, "{\"b\":2}");
        assert!(d.next_frame().is_none());
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let mut d = SseDecoder::new();
        d.push(b"data: {\"a\":1}\r\n\r\n");
        let frame = d.next_frame().expect("frame").expect("utf-8");
        assert_eq!(frame.data, "{\"a\":1}");
    }
}