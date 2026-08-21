//! Provider / endpoint model.
//!
//! Provider = the company/service (e.g. DeepSeek).
//! Endpoint = a concrete protocol endpoint for that provider (e.g. OpenAI-compatible, Anthropic-native).
//!
//! The user picks (provider_id, endpoint_id) and the rest auto-fills:
//!   protocol + base_url from EndpointSpec
//!   models from GET /models (with fallback to default_model)

#[derive(Debug, Clone)]
/// Where the user identifier is placed in the API request.
pub enum UserSendMode {
    /// User ID is sent in the JSON request body.
    Body,
}

/// How the reasoning/thinking parameter is sent to the model.
///
/// Different providers expect different formats for the thinking/reasoning
/// toggle parameter in the request body.
#[derive(Debug, Clone, Default)]
pub enum ThinkingParamMode {
    /// Standard OpenAI format: `{"type": "enabled"|"disabled"}` at top-level body.
    /// Used by: DeepSeek, GLM, Kimi, MiMo, Doubao, OpenAI.
    #[default]
    OpenAi,
    /// Boolean `enable_thinking: true/false` at top-level body. Used by: Qwen.
    QwenEnableThinking,
    /// `{"type": "adaptive"}` + `reasoning_split: true` at top-level. Used by: MiniMax.
    MiniMaxAdaptive,
}

/// Where the cache token count is located in the usage response.
///
/// Different providers return cache hit/miss info in different JSON paths.
#[derive(Debug, Clone, Default)]
pub enum CacheTokenField {
    /// Top-level: `usage.prompt_cache_hit_tokens` + `usage.prompt_cache_miss_tokens`.
    /// Used by: DeepSeek.
    #[default]
    PromptCacheHitTokens,
    /// Nested: `usage.prompt_tokens_details.cached_tokens`. Used by: Qwen, GLM.
    PromptDetailsCached,
    /// Top-level single value: `usage.cached_tokens`. Used by: Kimi.
    UsageCachedTokens,
    /// No cache information returned. Used by: MiMo, MiniMax.
    None,
}

/// Configuration for a single API endpoint (protocol variant) of a provider.
///
/// Each provider may expose multiple endpoints (e.g. OpenAI-compatible and
/// Anthropic-native). The user selects a (provider, endpoint) pair, and
/// the protocol, base URL, and model list are auto-filled from this spec.
#[derive(Debug, Clone)]
pub struct EndpointSpec {
    /// Internal endpoint identifier (e.g. "openai", "anthropic").
    pub id: String,
    /// Human-readable label shown in settings UI (e.g. "OpenAI-compatible").
    pub display: String,
    /// Protocol name: "openai" or "anthropic". Determines the HTTP API format.
    pub protocol: String,
    /// Base URL for API requests without trailing path (e.g. "https://api.deepseek.com").
    pub base_url: String,
    /// Fallback model when no model is selected by the user.
    pub default_model: String,
    /// Cached list of available model names fetched from the API.
    pub models: Vec<String>,
    /// URL for the `GET /models` endpoint. `None` = use `base_url/models`.
    pub models_url: Option<String>,
    /// Where to send the user identifier parameter. `None` = not sent.
    pub user_id_mode: Option<UserSendMode>,

    // ── Multi-provider adaptation fields ──
    /// Chat completions path override (appended to `base_url`).
    /// `None` = default `/chat/completions`.
    pub chat_path: Option<String>,
    /// Responses API path override (appended to `base_url`).
    /// `None` = default `/responses`.
    pub responses_path: Option<String>,
    /// Balance query path override (appended to `base_url`).
    /// `None` = default `/user/balance`.
    pub balance_path: Option<String>,
    /// How the thinking/reasoning parameter is formatted for this endpoint.
    /// Default: `OpenAi`.
    pub thinking_mode: ThinkingParamMode,
    /// Which field in the usage response carries the cache token count.
    /// Default: `PromptCacheHitTokens`.
    pub cache_field: CacheTokenField,
    /// Request an additional usage chunk before the streaming terminator.
    /// Kept per endpoint because not every OpenAI-compatible provider accepts
    /// `stream_options.include_usage`.
    pub include_stream_usage: bool,
    /// Whether this endpoint has a balance/info endpoint. Default: true.
    pub has_balance: bool,
    /// Whether this endpoint supports the thinking/reasoning parameter. Default: true.
    pub supports_thinking: bool,
    /// Whether this endpoint accepts OpenAI's `reasoning_effort` parameter.
    /// Kept separately because an endpoint may support neither vendor-specific
    /// thinking toggles nor OpenAI reasoning effort.
    pub supports_reasoning_effort: bool,
    /// Whether assistant history entries that contain tool calls must include
    /// an explicit `content: null`. Some OpenAI-compatible upstreams reject a
    /// missing content member even though the OpenAI schema permits null.
    pub tool_call_content_null: bool,
    /// Whether provider-specific `reasoning_content` may be sent back as
    /// assistant history. This is disabled for routers that target many
    /// different upstream schemas.
    pub supports_reasoning_content: bool,
    /// Ask a router to select only upstreams that implement every request
    /// parameter. Used by OpenRouter tool calls to avoid lax fallback routing.
    pub require_provider_parameters: bool,
    /// When true, the gate sends only incremental messages instead of full conversation
    /// history. Used for stateful proxy endpoints (e.g. DeepSeek Web CDP proxy).
    pub stateful: bool,
    /// Experimental/Beta endpoint flag surfaced to the settings UI as a badge.
    /// Beta endpoints are additive: they never replace a stable endpoint.
    pub beta: bool,
    /// Explicitly set `do_sample` in the request body. GLM-5.2 benefits from `false` for
    /// deterministic code generation. `None` means the field is not sent (API default).
    pub do_sample: Option<bool>,

    // ── Responses API adaptation fields ──
    /// Inject the built-in `web_search` tool so the model can search on its own
    /// (server-side execution). OpenAI / DeepSeek support it; some compatible
    /// endpoints ignore unknown tool types. Default: true.
    pub responses_web_search: bool,
    /// Allow echoing `web_search_call` output items back verbatim in the next
    /// turn so the server restores its search results (stateless multi-turn).
    /// DeepSeek documents this; OpenAI stateless mode behaves the same.
    /// Default: true.
    pub responses_echo_web_search_call: bool,
    /// Send `include: ["reasoning.encrypted_content"]`. OpenAI supports it;
    /// DeepSeek silently ignores it; a few strict compatible endpoints reject
    /// unknown members with 400. Default: true (OpenAI semantics).
    pub responses_send_include: bool,
    /// Upper bound for `reasoning.effort`. OpenAI: `"high"`; DeepSeek extends
    /// the ladder to `"xhigh"` / `"max"`. Values above the bound are clamped.
    /// Default: "high".
    pub responses_effort_max: String,
    /// Send the `user` field (rate-limit & KVCache isolation per end user).
    /// Default: true.
    pub responses_supports_user: bool,
    /// Provider-facing alias for QAQ-Harness's canonical `search` function tool.
    /// Some Responses-compatible providers reserve `search` while their
    /// built-in `web_search` tool is enabled. `None` keeps the canonical name.
    pub responses_search_function_alias: Option<String>,
    /// Echo assistant `reasoning` items back verbatim in the next turn's
    /// input. Default: true — DeepSeek / MiMo reject tool-loop continuations
    /// without them (HTTP 400), Kimi K3 & k2.7-code require them for preserved
    /// thinking, and GLM / Qwen / MiniMax / OpenAI accept them silently.
    pub responses_echo_reasoning_content: bool,
}

impl Default for EndpointSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            display: String::new(),
            protocol: "openai".into(),
            base_url: String::new(),
            default_model: String::new(),
            models: Vec::new(),
            models_url: None,
            user_id_mode: None,
            chat_path: None,
            responses_path: None,
            balance_path: None,
            thinking_mode: ThinkingParamMode::default(),
            cache_field: CacheTokenField::default(),
            include_stream_usage: false,
            has_balance: true,
            supports_thinking: true,
            supports_reasoning_effort: true,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            stateful: false,
            beta: false,
            do_sample: None,
            responses_web_search: true,
            responses_echo_web_search_call: true,
            responses_send_include: true,
            responses_effort_max: "high".into(),
            responses_supports_user: true,
            responses_search_function_alias: None,
            responses_echo_reasoning_content: true,
        }
    }
}

/// Top-level provider definition (e.g. DeepSeek, Qwen, OpenAI).
///
/// A provider is a company or service that hosts LLM models. Each provider
/// may have one or more endpoints (protocol variants).
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    /// Unique provider identifier (e.g. "deepseek", "qwen").
    pub id: String,
    /// Human-readable display name for settings UI.
    pub display: String,
    /// Available API endpoints for this provider.
    pub endpoints: Vec<EndpointSpec>,
}
