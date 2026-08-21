//! 游标式 SSE 帧解码器（共享于 chat 与 responses 两条流式路径）。
//!
//! 背景：旧实现两条路径各有性能缺陷——
//! - openai.rs 用 `eventsource_stream`（逐字节 poll 推进，实测 ~180KB/s CPU
//!   吞吐，且 200 token/s 时表现"总是很慢"）；
//! - responses.rs 用 `Vec::drain(..=line_end)` 逐行从头部搬移剩余字节
//!   （O(n²)，实测 20 万行/12MB 数据 5 分钟+ 无法跑完）。
//!
//! 本实现按 `\n` 定位行（O(n) 总体、无搬移，实测 ~143MB/s 与 serde_json
//! 解析同级），聚合 SSE 事件的 `data:` 字段，空行分隔事件。
//!
//! 语义与旧实现保持一致：
//! - 只在拿到完整 `\n` 结尾行后做严格 UTF-8 解码；非法行跳过（绝不 lossy）；
//! - `event:`/`id:`/`retry:` 行忽略（事件类型在 data 的 JSON 内）；
//! - 无空行分隔时（`event: X\ndata: A\nevent: Y\ndata: B`）`event:` 行触发
//!   前一事件冲刷，保证事件正确分离；
//! - 多 `data:` 行聚合为单事件（SSE 规范以单个 `\n` 连接）。

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
    /// 已消费前缀长度（未压缩，超过阈值时统一搬移一次摊销 O(n)）。
    consumed: usize,
    /// 当前事件的聚合 data payload。
    pending: Option<String>,
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

    /// 取下一完整帧（聚合的 data payload）。`None` = 暂无完整帧，需等更多数据。
    pub(crate) fn next_frame(&mut self) -> Option<Result<String, ()>> {
        loop {
            let rel = self.buf[self.consumed..].iter().position(|&b| b == b'\n')?;
            let end = self.consumed + rel;
            let raw = &self.buf[self.consumed..end];
            self.consumed = end + 1;

            let line = match std::str::from_utf8(raw) {
                Ok(line) => line.trim_end(),
                Err(_) => continue, // 非法 UTF-8 行：跳过
            };

            if line.is_empty() {
                // 空行 = 事件结束。
                if let Some(payload) = self.pending.take() {
                    return Some(Ok(payload));
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // 注释行
            }
            if line.starts_with("event:") || line.starts_with("id:") || line.starts_with("retry:") {
                // 事件边界：无空行时 `event:` 行也触发前一事件冲刷。
                if let Some(payload) = self.pending.take() {
                    return Some(Ok(payload));
                }
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.strip_prefix(' ').unwrap_or(payload);
                match &mut self.pending {
                    Some(p) => {
                        p.push('\n');
                        p.push_str(payload);
                    }
                    None => self.pending = Some(payload.to_string()),
                }
            }
            // 其他字段（unknown）忽略。
        }
    }

    /// 是否有未消费的原始字节或未冲刷的聚合帧（EOF 收尾路径用）。
    pub(crate) fn has_pending(&self) -> bool {
        self.buf.len() > self.consumed || self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_data_line_per_event() {
        let mut d = SseDecoder::new();
        d.push(b"data: {\"a\":1}\n\n");
        assert_eq!(d.next_frame(), Some(Ok("{\"a\":1}".into())));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn multiple_data_lines_aggregate_into_one_event() {
        let mut d = SseDecoder::new();
        d.push(b"data: first\n");
        assert_eq!(d.next_frame(), None);
        d.push(b"data: second\n\n");
        assert_eq!(d.next_frame(), Some(Ok("first\nsecond".into())));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn event_line_without_blank_separator_flushes_previous_event() {
        let mut d = SseDecoder::new();
        d.push(b"event: response.output_text.delta\ndata: {\"a\":1}\n");
        assert_eq!(d.next_frame(), None);
        d.push(b"event: response.output_text.delta\ndata: {\"b\":2}\n\n");
        assert_eq!(d.next_frame(), Some(Ok("{\"a\":1}".into())));
        assert_eq!(d.next_frame(), Some(Ok("{\"b\":2}".into())));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn utf8_char_split_across_chunks_is_not_corrupted() {
        let mut d = SseDecoder::new();
        // "中" = E4 B8 AD，切到两个 push。
        d.push(b"data: {\"delta\":\"\xe4\xb8");
        assert_eq!(d.next_frame(), None);
        d.push(b"\xad\"}\n\n");
        assert_eq!(d.next_frame(), Some(Ok("{\"delta\":\"中\"}".into())));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn invalid_utf8_line_is_skipped() {
        let mut d = SseDecoder::new();
        d.push(b"data: \xff\xfe broken\n\n");
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn done_marker_comes_through_as_payload() {
        let mut d = SseDecoder::new();
        d.push(b"data: [DONE]\n\n");
        assert_eq!(d.next_frame(), Some(Ok("[DONE]".into())));
    }

    #[test]
    fn comments_and_ignored_fields_do_not_emit() {
        let mut d = SseDecoder::new();
        d.push(b": keepalive\nretry: 100\nid: 1\ndata: {\"ok\":true}\n\n");
        assert_eq!(d.next_frame(), Some(Ok("{\"ok\":true}".into())));
        assert_eq!(d.next_frame(), None);
    }
}