//! gate::transport — 三协议共享的传输零件（Phase 3-1 收敛）。
//!
//! `chat_completions_api` / `message_api` / `responses_api` 曾各持一份字节级
//! 相同的实现：3 个独立 current-thread tokio runtime、cancel 轮询、重试退避、
//! 错误描述、skill envelope 归一、stateful 过滤、`SseTrace` 诊断。本模块是其
//! 单一来源；协议本质差异（convert_messages ×3、帧处理 ×3、convert_tools ×3）
//! 保留在各协议文件。
//!
//! 收敛时消除的行为漂移：
//! - 3 个独立 runtime → 1 个共享 runtime（此前三协议各建各的 current-thread RT）；
//! - `responses_api` 的内联 `2u64.pow(attempt)` 无 30s 上限 → 统一走
//!   `backoff_delay`（`BASE_DELAY_SECS * 2^(attempt-1)`，上限 30s）；
//! - `chat_completions_api::filter_stateful_messages` 在 release 也打
//!   `eprintln!("[filter] 输出…")` → 随统一删除（stderr 污染缺陷，见 Phase 4）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_types::{ContentBlock, Message};

use super::types::{ProviderConfig, StreamEvent};

/// SSE 轮询间隔：无数据到达时以外层 Tokio timeout 检查 cancel 标志。
pub(crate) const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

// 与官方客户端会话重试策略对齐（opencode session/retry.ts：5 次重试、
// 2s 初始、翻倍），吸收网关瞬时 5xx  burst。
pub(crate) const MAX_RETRIES: u32 = 5;
pub(crate) const BASE_DELAY_SECS: u64 = 2;

/// Crate-global tokio runtime for reqwest I/O.
/// Uses current-thread scheduler — all async I/O serialises on the
/// calling thread via Runtime::block_on.
static FALLBACK_RT: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create qaqh-gate shared tokio runtime")
});

pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    FALLBACK_RT.block_on(f)
}

pub(crate) fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false)
}

pub(crate) fn sleep_with_cancel(delay: Duration, cancel: Option<&Arc<AtomicBool>>) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < delay {
        if is_cancelled(cancel) {
            return true;
        }
        let remaining = delay - start.elapsed();
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    false
}

pub(crate) fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}

pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    let secs = BASE_DELAY_SECS * 2u64.pow(attempt.saturating_sub(1));
    Duration::from_secs(secs.min(30))
}

pub(crate) fn http_error_description(status: u16) -> &'static str {
    match status {
        400 => "Bad Request — 格式错误",
        401 => "Unauthorized — API key 无效",
        402 => "Payment Required — 余额不足",
        422 => "Unprocessable — 参数错误",
        429 => "Rate Limit — 请求速率超限",
        500 => "Internal Error — 服务器故障",
        503 => "Service Unavailable — 服务器繁忙",
        _ => "Unknown",
    }
}

pub(crate) fn filter_stateful_messages(messages: Vec<Message>) -> (Vec<Message>, usize) {
    if messages.is_empty() {
        return (messages, 0);
    }
    let last_asst_idx = messages.iter().rposition(|m| m.role == "assistant");
    let start = last_asst_idx.map(|i| i + 1).unwrap_or(0);
    let is_first = start == 0;
    if is_first {
        return (messages, 0);
    }
    let dropped_images = messages[..start]
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(
                b,
                ContentBlock::Image { .. } | ContentBlock::ImageRef { .. }
            )
        })
        .count();
    let mut out: Vec<Message> = Vec::new();
    for msg in &messages[start..] {
        out.push(msg.clone());
    }
    if out.is_empty()
        && let Some(last) = messages.last()
        && last.role != "assistant"
    {
        out.push(last.clone());
    }
    (out, dropped_images)
}

pub(crate) fn normalize_skill_envelope(
    provider: &ProviderConfig,
    mut messages: Vec<Message>,
) -> Result<Vec<Message>, String> {
    let is_envelope = messages.last().is_some_and(|message| {
        message.role == "system" && message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.starts_with("<skill_context_envelope"))
        })
    });
    if !is_envelope || provider.supports_tail_system {
        return Ok(messages);
    }
    if provider.stateful {
        return Err("SKILL_CONTEXT_SYNC_UNSUPPORTED: stateful provider cannot accept the authoritative tail system envelope; rebuild the remote session with a compatible provider".into());
    }
    let envelope = messages.pop().expect("checked last message");
    let dynamic_slot = messages
        .iter()
        .take_while(|message| message.role == "system")
        .count();
    messages.insert(dynamic_slot, envelope);
    log::warn!("skill context moved to head dynamic system slot; prompt-prefix cache degraded");
    Ok(messages)
}

/// `QAQH_SSE_TRACE=<path>`：将 gate 派生的每个流式事件按到达序追加写入文件
/// （`<seq>\t<类型>\t<长度>`），用于核对 reasoning/content/tool 在链路的忠实
/// 流转与先后顺序（诊断思考链/正文交错问题）。不设该变量时零开销。
pub(crate) struct SseTrace {
    pub(crate) file: Option<std::fs::File>,
    pub(crate) seq: u64,
}

impl SseTrace {
    pub(crate) fn from_env() -> Self {
        let file = std::env::var_os("QAQH_SSE_TRACE").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self { file, seq: 0 }
    }
    pub(crate) fn record(&mut self, event: &StreamEvent) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        use std::io::Write;
        let tag = match event {
            StreamEvent::ReasoningDelta(d) => format!("reasoning\t{}", d.chars().count()),
            StreamEvent::ContentDelta(d) => format!("content\t{}", d.chars().count()),
            StreamEvent::ToolCallProgress { .. } => "tool_call_progress".to_string(),
            StreamEvent::Done { .. } => "done".to_string(),
            StreamEvent::UsageUpdate(_) => "usage".to_string(),
            StreamEvent::WebSearchStatus(_) => "web_search_status".to_string(),
            _ => "other".to_string(),
        };
        let _ = writeln!(file, "{}\t{}", self.seq, tag);
        self.seq += 1;
    }
}
