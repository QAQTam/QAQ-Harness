//! qaqh-gate: LLM API gateway — HTTP streaming + message format conversion.
//!
//! Supports OpenAI Chat Completions, Responses API, and Anthropic Messages protocols.
//!
//! # Note: string slices
//!
//! All string slices in this crate use indices from `find()` on ASCII
//! patterns (`<`, `>`, `\n`, `"data: "`, etc.), always on valid UTF-8
//! boundaries.  The clippy `string_slice` lint is allowed at the crate
//! level (see Cargo.toml).

mod chat_completions_api;
mod message_api;
mod responses_api;
#[cfg(test)]
mod rt_test;
mod sse;
pub mod tool_parser;
mod transport;
mod types;

pub use types::{ProviderConfig, ProviderKind, ResponsesCompat, StreamEvent};

use qaqh_types::{Message, ToolDef};
use reqwest::Client;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// 进程级共享 reqwest 客户端（PR-2-1 单源化，审计 #2）。
///
/// 三协议适配器（chat completions / anthropic messages / responses）统一引用本构造，
/// 禁止再复制构造；未来 provider 级差异必须走显式配置字段。
///
/// 超时策略：连接建立 30s（§7 D-3 拍板，弱网保守判定）；
/// TCP keepalive 60s + 空闲连接池 120s 维持长连接；总预算 30min——
/// 流式响应可合法持续远超 5 分钟，而 reqwest 默认无总超时，缺省即可能永久挂起
/// （responses 适配器曾缺失此三项，为本 PR 的 bug 加固点）。
pub(crate) fn shared_http_client() -> &'static Client {
    static SHARED_CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .pool_idle_timeout(Duration::from_secs(120))
            .timeout(Duration::from_secs(30 * 60))
            .user_agent(qaqh_types::QAQH_USER_AGENT)
            .build()
            .expect("failed to build shared reqwest client")
    });
    &SHARED_CLIENT
}
/// Send a chat completion request with SSE streaming.
///
/// `cancel` is an optional shared abort flag. When set to `true`, the
/// streaming read loop will return `Err("cancelled by user")` within
/// one 50ms polling interval, aborting the HTTP response promptly instead
/// of waiting for the server to finish.
#[allow(clippy::string_slice)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    match provider.kind {
        ProviderKind::Responses => responses_api::chat_stream_responses(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
        ProviderKind::Anthropic => message_api::chat_stream_anthropic(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
        ProviderKind::OpenAi => chat_completions_api::chat_stream_openai(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
    }
}

/// Synchronous (non-streaming) chat for internal use (compact, etc.).
pub fn chat_sync(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    match provider.kind {
        ProviderKind::Responses => {
            responses_api::chat_sync_responses(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::Anthropic => {
            message_api::chat_sync_anthropic(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::OpenAi => {
            chat_completions_api::chat_sync_openai(provider, &provider.model, messages, max_tokens)
        }
    }
}
