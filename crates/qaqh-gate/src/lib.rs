//! qaqh-gate: LLM API gateway — HTTP streaming + message format conversion.
//!
//! Supports OpenAI Chat Completions and Responses API protocols.
//!
//! # Note: string slices
//!
//! All string slices in this crate use indices from `find()` on ASCII
//! patterns (`<`, `>`, `\n`, `"data: "`, etc.), always on valid UTF-8
//! boundaries.  The clippy `string_slice` lint is allowed at the crate
//! level (see Cargo.toml).

pub mod guard;
mod openai;
mod responses;
#[cfg(test)]
mod rt_test;
mod sse;
pub mod tool_parser;
mod types;

pub use types::{ProviderConfig, ProviderKind, ResponsesCompat, StreamEvent};

use qaqh_types::{Message, ToolDef};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Send a chat completion request with SSE streaming.
///
/// `cancel` is an optional shared abort flag. When set to `true`, the
/// streaming read loop will return `Err("cancelled by user")` within
/// one 50ms polling interval, aborting the HTTP response promptly instead
/// of waiting for the server to finish.
#[allow(clippy::string_slice)]
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
        ProviderKind::Responses => responses::chat_stream_responses(
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
        ProviderKind::OpenAi => openai::chat_stream_openai(
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
            responses::chat_sync_responses(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::OpenAi => {
            openai::chat_sync_openai(provider, &provider.model, messages, max_tokens)
        }
    }
}
