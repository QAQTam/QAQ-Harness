//! Shared gate types — provider config and unified stream events.

use qaqh_types::Message;
use qaqh_types::{CacheTokenField, ThinkingParamMode};

/// Global reasoning-effort ladder. QAQ-Harness always enables thinking, so the
/// `none` / `disable` levels are not part of the presets: any value that
/// would turn reasoning off is promoted to the lowest thinking level.
pub const EFFORT_LADDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Values that would disable or minimize reasoning. Never sent to the API:
/// thinking is a hard requirement of QAQ-Harness, so they are promoted to `low`.
const EFFORT_OFF: [&str; 6] = ["none", "minimal", "disable", "disabled", "off", ""];

/// Normalize a reasoning-effort string against the global ladder.
///
/// `None` stays `None` (caller decides whether to send the field); values
/// that disable thinking (`none` / `minimal` / `disable` / `off` / empty)
/// are promoted to `low` so the provider always reasons. Unknown values are
/// passed through untouched so future provider levels are not rejected.
pub fn normalize_reasoning_effort(effort: Option<&str>) -> Option<String> {
    let e = effort?;
    if EFFORT_OFF.contains(&e) {
        Some("low".to_string())
    } else {
        Some(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderKind {
    OpenAi,
    Responses,
}

impl ProviderKind {
    pub fn from_str(s: &str) -> Self {
        match s {
            "responses" => Self::Responses,
            _ => Self::OpenAi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub user_id_mode: Option<qaqh_types::UserSendMode>,

    // ── Multi-provider adaptation fields ──
    pub chat_path: Option<String>,
    pub responses_path: Option<String>,
    pub thinking_mode: ThinkingParamMode,
    pub cache_field: CacheTokenField,
    pub include_stream_usage: bool,
    pub supports_thinking: bool,
    pub supports_reasoning_effort: bool,
    pub tool_call_content_null: bool,
    pub supports_reasoning_content: bool,
    pub require_provider_parameters: bool,
    /// When Some, explicitly sets `do_sample` in the request body. Used by GLM for
    /// deterministic codegen (do_sample=false). None means don't send the field.
    pub do_sample: Option<bool>,

    // ── Stateful proxy mode (e.g. DeepSeek Web CDP proxy) ──
    /// When true, only send incremental messages (not full history).
    /// The proxy remembers conversation context.
    pub stateful: bool,
    /// Whether the endpoint accepts a system message after history/tools.
    pub supports_tail_system: bool,
    /// Responses API capability differences from the OpenAI reference semantics.
    /// Configured from `EndpointSpec` (registry) so new providers only need a
    /// config change, never gate code.
    pub responses_compat: ResponsesCompat,
    /// Prompt cache key for prefix KV reuse (opencode `promptCacheKey`).
    /// `None` = not sent. For opencode/muse this is `session.seed`.
    pub prompt_cache_key: Option<String>,
}

/// Responses API provider capability differences.
///
/// Defaults follow OpenAI's official Responses API semantics — every
/// compatible endpoint uses that format as its reference (DeepSeek's docs
/// say so explicitly). Providers that diverge override the fields they
/// differ on; unknown request members are ignored silently by DeepSeek and
/// rejected only by a few strict endpoints.
#[derive(Debug, Clone)]
pub struct ResponsesCompat {
    /// Inject the built-in `web_search` tool so the model can search on its
    /// own (server-side execution). Default: true.
    pub web_search: bool,
    /// Allow echoing `web_search_call` items back verbatim to restore
    /// server-side search results across stateless turns. Default: true.
    pub echo_web_search_call: bool,
    /// Send `include: ["reasoning.encrypted_content"]`. Default: true.
    pub send_include: bool,
    /// Upper bound for `reasoning.effort` ("high" for OpenAI, "max" for
    /// DeepSeek). Higher requested values are clamped. Default: "high".
    pub effort_max: String,
    /// Send the `user` field (rate-limit & KVCache isolation). Default: true.
    pub supports_user: bool,
    /// Provider-facing alias for the canonical QAQ-Harness `search` function.
    /// The alias is reversed before tool events leave the gate.
    pub search_function_alias: Option<String>,
    /// Echo assistant `reasoning` items back verbatim in the next turn's
    /// input. Default: true — DeepSeek / MiMo reject tool-loop continuations
    /// without them (HTTP 400), Kimi K3 & k2.7-code require them for preserved
    /// thinking, and GLM / Qwen / MiniMax / OpenAI accept them silently.
    pub echo_reasoning_content: bool,
}

impl Default for ResponsesCompat {
    fn default() -> Self {
        Self {
            web_search: true,
            echo_web_search_call: true,
            send_include: true,
            effort_max: "high".into(),
            supports_user: true,
            search_function_alias: None,
            echo_reasoning_content: true,
        }
    }
}

/// Produce a bounded provider error that is safe to persist and display.
/// Providers occasionally echo credentials in error bodies, and byte slicing
/// arbitrary UTF-8 can panic while handling the original failure.
pub(crate) fn safe_provider_error_body(body: &str, api_key: &str) -> String {
    let redacted = if api_key.is_empty() {
        body.to_owned()
    } else {
        body.replace(api_key, "[REDACTED]")
    };
    redacted.chars().take(200).collect()
}

impl ProviderConfig {
    pub fn openai(
        base_url: &str,
        api_key: &str,
        model: &str,
        user_id_mode: Option<qaqh_types::UserSendMode>,
        chat_path: Option<String>,
        thinking_mode: ThinkingParamMode,
        cache_field: CacheTokenField,
        supports_thinking: bool,
        do_sample: Option<bool>,
    ) -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            user_id_mode,
            chat_path,
            responses_path: None,
            thinking_mode,
            cache_field,
            include_stream_usage: false,
            supports_thinking,
            supports_reasoning_effort: true,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            do_sample,
            stateful: false,
            supports_tail_system: true,
            responses_compat: ResponsesCompat::default(),
            prompt_cache_key: None,
        }
    }

    /// Build a Responses API provider config.
    pub fn responses(
        base_url: &str,
        api_key: &str,
        model: &str,
        responses_path: Option<String>,
    ) -> Self {
        Self {
            kind: ProviderKind::Responses,
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            user_id_mode: None,
            chat_path: None,
            responses_path,
            thinking_mode: ThinkingParamMode::OpenAi,
            cache_field: CacheTokenField::default(),
            include_stream_usage: false,
            supports_thinking: false,
            supports_reasoning_effort: true,
            tool_call_content_null: false,
            supports_reasoning_content: false,
            require_provider_parameters: false,
            do_sample: None,
            stateful: false,
            supports_tail_system: true,
            responses_compat: ResponsesCompat::default(),
            prompt_cache_key: None,
        }
    }

    /// Configure this provider for stateful mode (web proxy).
    pub fn with_stateful(mut self, stateful: bool) -> Self {
        self.stateful = stateful;
        self
    }

    pub fn with_stream_usage(mut self, include: bool) -> Self {
        self.include_stream_usage = include;
        self
    }

    /// Apply OpenRouter's strict OpenAI-compatible tool-history contract.
    pub fn with_openrouter_compat(mut self) -> Self {
        self.supports_thinking = false;
        self.supports_reasoning_effort = false;
        self.tool_call_content_null = true;
        self.supports_reasoning_content = false;
        self.require_provider_parameters = true;
        self
    }

    pub fn with_tail_system_support(mut self, supported: bool) -> Self {
        self.supports_tail_system = supported;
        self
    }
}

// ── StreamEvent ──

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress {
        index: usize,
        id: String,
        name: String,
        args_so_far: String,
    },
    /// Server-side web search progress (Responses API built-in tool).
    /// Payload is one of "in_progress" | "searching" | "completed".
    WebSearchStatus(String),
    Done {
        raw_message: Message,
        usage: Option<qaqh_types::UsageInfo>,
        stop_reason: Option<String>,
    },
    /// Emitted whenever the API reports updated usage mid-stream (cache hits may appear in any chunk).
    UsageUpdate(qaqh_types::UsageInfo),
    Error(String),
    /// Emitted when the gate is retrying after a retryable error.
    Retrying {
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error: String,
    },
}
