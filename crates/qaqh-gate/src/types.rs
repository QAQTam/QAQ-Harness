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

/// Clamp a requested effort to an endpoint's sparse allowlist
/// (`EndpointSpec::effort_allowlist`, mirrored onto `ProviderConfig`).
///
/// Router models often accept a non-contiguous subset of the ladder (ox-alpha:
/// max/high/low). Sending an off-domain value is either silently ignored or
/// rejected by the router, so we snap to the nearest allowed level by ladder
/// distance; ties resolve upward because QAQ always prefers strong thinking.
/// Unknown (non-ladder) requested values fall back to the highest allowed
/// level; an allowlist with no ladder-known entries returns the input as-is.
pub fn clamp_effort_to_allowlist(effort: &str, allowlist: &[String]) -> String {
    let ladder_idx = |v: &str| EFFORT_LADDER.iter().position(|x| *x == v);
    let known: Vec<(usize, String)> = allowlist
        .iter()
        .filter_map(|a| ladder_idx(a).map(|i| (i, a.clone())))
        .collect();
    if known.is_empty() {
        return effort.to_string();
    }
    let pick = |want: usize| -> String {
        known
            .iter()
            .min_by(|a, b| {
                let da = a.0.abs_diff(want);
                let db = b.0.abs_diff(want);
                da.cmp(&db).then(b.0.cmp(&a.0))
            })
            .map(|(_, v)| v.clone())
            .expect("known non-empty")
    };
    match ladder_idx(effort) {
        Some(want) => pick(want),
        // Unknown passthrough value: strongest thinking the endpoint allows.
        None => known
            .iter()
            .max_by_key(|(i, _)| *i)
            .map(|(_, v)| v.clone())
            .expect("known non-empty"),
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

/// Identity presented to the OpenCode gateway when management headers are on.
/// The official desktop app injects `OPENCODE_CLIENT=desktop`; the CLI default
/// is `cli` (packages/opencode/src/effect/runtime-flags.ts).
pub const OPENCODE_CLIENT_ID: &str = "cli";
/// Official client version these headers were mirrored against.
pub const OPENCODE_CLIENT_VERSION: &str = "1.18.22";

/// Upstream closed the stream (clean TCP EOF) before producing any content
/// and without a protocol terminal marker (`[DONE]` / `response.completed`).
/// Busy-shedding endpoints do this instead of returning an error code.
/// Retryable: nothing was streamed to the caller yet, so a whole request
/// retry loses nothing.
#[derive(Debug)]
pub(crate) struct EmptyStreamEof;

impl std::fmt::Display for EmptyStreamEof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream closed stream before any content")
    }
}

impl std::error::Error for EmptyStreamEof {}

/// OpenCode gateway management headers, mirroring the official client
/// (opencode session/llm/request.ts). The gateway records them for per-session
/// API management, so sending well-formed IDs makes harness sessions visible
/// and groupable in the console.
#[derive(Debug, Clone, PartialEq)]
pub struct OpencodeHeaders {
    /// `x-opencode-session` — stable for the whole conversation (`ses_…`).
    pub session_id: String,
    /// `x-opencode-request` — one per logical request (`msg_…`): the user
    /// message that triggered a turn in the official client; here one per
    /// turn / title / compact request.
    pub request_id: String,
}

impl OpencodeHeaders {
    /// Derive both IDs deterministically from `(session_seed, tag)` in the
    /// official identifier format `<prefix>_<12 hex><14 base62>`
    /// (@opencode-ai/schema/identifier: 48-bit time-like field + 14 random
    /// base62 chars). Deterministic derivation keeps the same conversation on
    /// the same `x-opencode-session` across process restarts.
    pub fn derive(session_seed: &str, tag: &str) -> Self {
        Self {
            session_id: derive_opencode_id("ses", session_seed),
            request_id: derive_opencode_id("msg", tag),
        }
    }
}

/// Build an official-format identifier deterministically from `key`.
fn derive_opencode_id(prefix: &str, key: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut a = DefaultHasher::new();
    prefix.hash(&mut a);
    key.hash(&mut a);
    let hi = a.finish();
    let mut b = DefaultHasher::new();
    b.write(key.as_bytes());
    b.write_u8(0x5a);
    let lo = b.finish();
    // 48-bit field as 12 hex chars — matches the official timestamp slot.
    let time_hex = format!("{:012x}", hi & 0xFFFF_FFFF_FFFF);
    // 14 chars drawn from the official base62 alphabet via an LCG stream.
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut state = lo;
    let mut rand_part = String::with_capacity(14);
    for _ in 0..14 {
        rand_part.push(CHARS[(state % 62) as usize] as char);
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
    }
    format!("{prefix}_{time_hex}{rand_part}")
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
    /// Sparse allowlist of accepted `reasoning_effort` values (mirrored from
    /// `EndpointSpec::effort_allowlist`). When set, the requested effort is
    /// snapped to the nearest allowed ladder level before sending. See
    /// [`clamp_effort_to_allowlist`].
    pub effort_allowlist: Option<Vec<String>>,
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
    /// OpenCode gateway management headers (`x-opencode-*` + UA override).
    /// `None` = send nothing (all non-opencode providers).
    pub opencode_headers: Option<OpencodeHeaders>,
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
            effort_allowlist: None,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            do_sample,
            stateful: false,
            supports_tail_system: true,
            responses_compat: ResponsesCompat::default(),
            prompt_cache_key: None,
            opencode_headers: None,
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
            effort_allowlist: None,
            tool_call_content_null: false,
            supports_reasoning_content: false,
            require_provider_parameters: false,
            do_sample: None,
            stateful: false,
            supports_tail_system: true,
            responses_compat: ResponsesCompat::default(),
            prompt_cache_key: None,
            opencode_headers: None,
        }
    }

    /// Attach OpenCode gateway management headers when the endpoint is an
    /// OpenCode gateway (`opencode.ai/zen/...`); no-op for everyone else.
    ///
    /// `session_seed` identifies the conversation (stable across restarts),
    /// `request_tag` the logical request (turn id / "title" / "compact").
    pub fn with_opencode_headers(mut self, session_seed: &str, request_tag: &str) -> Self {
        if self.base_url.contains("opencode.ai/zen") {
            self.opencode_headers = Some(OpencodeHeaders::derive(session_seed, request_tag));
        }
        self
    }

    /// Apply the management headers onto an HTTP request builder, mirroring
    /// the official client's LLM request headers. The per-request User-Agent
    /// overrides the client-level default set in `GLOBAL_CLIENT`.
    pub(crate) fn apply_opencode_headers(
        &self,
        req: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match &self.opencode_headers {
            None => req,
            Some(h) => req
                .header("x-opencode-session", &h.session_id)
                .header("x-opencode-request", &h.request_id)
                .header("x-opencode-client", OPENCODE_CLIENT_ID)
                .header("User-Agent", format!("opencode/{OPENCODE_CLIENT_VERSION}")),
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

#[cfg(test)]
mod opencode_headers_tests {
    use super::*;

    const B62: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    #[test]
    fn derived_ids_follow_official_format_and_are_stable() {
        let h = OpencodeHeaders::derive("seed-abc", "turn-1");
        assert!(h.session_id.starts_with("ses_"));
        assert!(h.request_id.starts_with("msg_"));
        for tail in [
            h.session_id.trim_start_matches("ses_"),
            h.request_id.trim_start_matches("msg_"),
        ] {
            assert_eq!(tail.len(), 26);
            assert!(tail[..12].chars().all(|c| c.is_ascii_hexdigit()));
            assert!(tail[12..].chars().all(|c| B62.contains(c)));
        }
        // Deterministic: same inputs, same IDs (stable across restarts).
        let h2 = OpencodeHeaders::derive("seed-abc", "turn-1");
        assert_eq!(h.session_id, h2.session_id);
        assert_eq!(h.request_id, h2.request_id);
        // Same conversation keeps the session id; each turn gets its own request id.
        let other_turn = OpencodeHeaders::derive("seed-abc", "turn-2");
        assert_eq!(h.session_id, other_turn.session_id);
        assert_ne!(h.request_id, other_turn.request_id);
        // Different conversations never collide.
        let other_session = OpencodeHeaders::derive("seed-xyz", "turn-1");
        assert_ne!(h.session_id, other_session.session_id);
    }

    #[test]
    fn with_opencode_headers_only_attaches_on_opencode_gateway() {
        let base = ProviderConfig::openai(
            "https://example.com/v1",
            "k",
            "m",
            None,
            None,
            ThinkingParamMode::OpenAi,
            CacheTokenField::None,
            false,
            None,
        );
        assert!(
            base.clone()
                .with_opencode_headers("seed", "turn")
                .opencode_headers
                .is_none()
        );
        let oc = ProviderConfig::openai(
            "https://opencode.ai/zen/go/v1",
            "k",
            "m",
            None,
            None,
            ThinkingParamMode::OpenAi,
            CacheTokenField::None,
            false,
            None,
        )
        .with_opencode_headers("seed", "turn");
        let hdrs = oc.opencode_headers.expect("headers must attach");
        assert!(hdrs.session_id.starts_with("ses_"));
        assert!(hdrs.request_id.starts_with("msg_"));
    }

    fn allowlist(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn clamp_snaps_to_nearest_allowed_ladder_level() {
        // ox-alpha 域:max/high/low(稀疏、跳档)。
        let al = allowlist(&["max", "high", "low"]);
        assert_eq!(clamp_effort_to_allowlist("low", &al), "low");
        assert_eq!(clamp_effort_to_allowlist("high", &al), "high");
        assert_eq!(clamp_effort_to_allowlist("max", &al), "max");
        // medium 夹在 low/high 正中间 → 并列取高档(永远偏好强思考)。
        assert_eq!(clamp_effort_to_allowlist("medium", &al), "high");
        // xhigh 同理 → max。
        assert_eq!(clamp_effort_to_allowlist("xhigh", &al), "max");
        // 连续域不受影响。
        assert_eq!(
            clamp_effort_to_allowlist("xhigh", &allowlist(&["high", "xhigh"])),
            "xhigh"
        );
    }

    #[test]
    fn clamp_unknown_request_falls_back_to_strongest_allowed() {
        let al = allowlist(&["low", "high"]);
        assert_eq!(clamp_effort_to_allowlist("ultra", &al), "high");
    }

    #[test]
    fn clamp_allowlist_without_ladder_entries_returns_input() {
        assert_eq!(
            clamp_effort_to_allowlist("medium", &allowlist(&["turbo"])),
            "medium"
        );
    }
}
