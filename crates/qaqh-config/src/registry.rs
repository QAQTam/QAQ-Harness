//! Provider registry — known providers and their endpoints.
//!
//! Architecture:
//!   Provider (e.g. DeepSeek) has 1..N Endpoints (all OpenAI-compatible for now).
//!   User selects (provider_id, endpoint_id) → protocol + base_url auto-fill.
//!   Model list is fetched from endpoint's /models URL at runtime.
//!
//! Backward compat: old provider_id "deepseek-openai"/"deepseek-anthropic" are
//! auto-migrated to provider_id="deepseek" + endpoint="openai".

use qaqh_types::{CacheTokenField, EndpointSpec, ProviderSpec, ThinkingParamMode, UserSendMode};

fn deepseek() -> ProviderSpec {
    ProviderSpec {
        id: "deepseek".into(),
        display: "DeepSeek".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "OpenAI-compatible".into(),
                protocol: "openai".into(),
                base_url: "https://api.deepseek.com".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://api.deepseek.com".into()),
                user_id_mode: Some(UserSendMode::Body),
                include_stream_usage: true,
                // chat_path: None → "/chat/completions" (default)
                // thinking_mode: OpenAi (default)
                // cache_field: PromptCacheHitTokens (default)
                ..Default::default()
            },
            // DeepSeek Responses API (Beta): 目前仅支持 deepseek-v4-flash。
            // 模型列表静态锁定，避免 /models 探测在 Beta 阶段引入不稳定模型。
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API".into(),
                protocol: "responses".into(),
                base_url: "https://api.deepseek.com".into(),
                default_model: "deepseek-v4-flash".into(),
                models: vec!["deepseek-v4-flash".into()],
                responses_path: Some("/responses".into()),
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                // DeepSeek silently ignores `include` (no encrypted reasoning),
                // so skip it; and its effort ladder extends to "max".
                responses_send_include: false,
                responses_effort_max: "max".into(),
                // DeepSeek rejects a request that combines its built-in
                // web_search with a custom function literally named `search`.
                // Alias only at the provider boundary; QAQ-Harness keeps `search`
                // canonical in execution, events, and persisted history.
                responses_search_function_alias: Some("qaqh_search".into()),
                // Reasoning echo is the default (responses_echo_reasoning_content
                // defaults to true): DeepSeek's thinking mode requires assistant
                // reasoning_text to be passed back whenever the input continues a
                // tool loop (ends with function_call_output), otherwise HTTP 400.
                beta: true,
                ..Default::default()
            },
        ],
    }
}

fn qwen() -> ProviderSpec {
    ProviderSpec {
        id: "qwen".into(),
        display: "Qwen (阿里百炼)".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "OpenAI-compatible".into(),
                protocol: "openai".into(),
                base_url: "https://dashscope.aliyuncs.com".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
                chat_path: Some("/compatible-mode/v1/chat/completions".into()),
                thinking_mode: ThinkingParamMode::QwenEnableThinking,
                cache_field: CacheTokenField::PromptDetailsCached,
                has_balance: false,
                ..Default::default()
            },
            // Qwen Responses API (bridge): dashscope exposes the Responses
            // protocol at the OpenAI-compatible prefix. Known differences are
            // tracked in docs/responses-api-support.md (R1: reasoning events
            // use `response.reasoning_summary_text.delta`; R3: effort ladder
            // unverified). Beta until those are confirmed.
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API".into(),
                protocol: "responses".into(),
                base_url: "https://dashscope.aliyuncs.com".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
                responses_path: Some("/compatible-mode/v1/responses".into()),
                thinking_mode: ThinkingParamMode::QwenEnableThinking,
                cache_field: CacheTokenField::PromptDetailsCached,
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                has_balance: false,
                beta: true,
                ..Default::default()
            },
        ],
    }
}

fn glm() -> ProviderSpec {
    ProviderSpec {
        id: "glm".into(),
        display: "GLM (智谱AI)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://open.bigmodel.cn".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://open.bigmodel.cn/api/paas/v4".into()),
            chat_path: Some("/api/paas/v4/chat/completions".into()),
            cache_field: CacheTokenField::PromptDetailsCached,
            do_sample: Some(false),
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn kimi() -> ProviderSpec {
    ProviderSpec {
        id: "kimi".into(),
        display: "Kimi (月之暗面)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.moonshot.cn/v1".into()),
            balance_path: Some("/users/me/balance".into()),
            cache_field: CacheTokenField::UsageCachedTokens,
            ..Default::default()
        }],
    }
}

fn mimo() -> ProviderSpec {
    ProviderSpec {
        id: "mimo".into(),
        display: "MiMo (小米)".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "OpenAI-compatible".into(),
                protocol: "openai".into(),
                base_url: "https://api.xiaomimimo.com/v1".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://api.xiaomimimo.com/v1".into()),
                cache_field: CacheTokenField::None,
                has_balance: false,
                ..Default::default()
            },
            // MiMo Responses API (bridge): https://mimo.mi.com/docs/zh-CN/api/chat/responses
            // Uses the standard OpenAI item format and reasoning_text events.
            // Constraint: previous_response_id / background / context_management
            // are NOT supported (rejected); the gate never sends them.
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API".into(),
                protocol: "responses".into(),
                base_url: "https://api.xiaomimimo.com/v1".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://api.xiaomimimo.com/v1".into()),
                responses_path: Some("/responses".into()),
                cache_field: CacheTokenField::None,
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                responses_effort_max: "high".into(),
                has_balance: false,
                beta: true,
                ..Default::default()
            },
        ],
    }
}

fn minimax() -> ProviderSpec {
    ProviderSpec {
        id: "minimax".into(),
        display: "MiniMax (稀宇)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.minimaxi.com/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.minimaxi.com/v1".into()),
            thinking_mode: ThinkingParamMode::MiniMaxAdaptive,
            cache_field: CacheTokenField::None,
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn doubao() -> ProviderSpec {
    ProviderSpec {
        id: "doubao".into(),
        display: "Doubao (火山方舟)".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "OpenAI-compatible".into(),
                protocol: "openai".into(),
                base_url: "https://ark.cn-beijing.volces.com".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://ark.cn-beijing.volces.com/api/v3".into()),
                chat_path: Some("/api/v3/chat/completions".into()),
                ..Default::default()
            },
            // Doubao Responses API (bridge): 火山方舟 exposes the Responses
            // protocol at /api/v3/responses. Uses the standard OpenAI item
            // format. Known differences are tracked in
            // docs/responses-api-support.md (R2: thinking embedding and
            // thinking params unverified). Beta until confirmed.
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API".into(),
                protocol: "responses".into(),
                base_url: "https://ark.cn-beijing.volces.com".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://ark.cn-beijing.volces.com/api/v3".into()),
                responses_path: Some("/api/v3/responses".into()),
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                has_balance: false,
                beta: true,
                ..Default::default()
            },
        ],
    }
}

fn openai() -> ProviderSpec {
    ProviderSpec {
        id: "openai".into(),
        display: "OpenAI".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "Chat Completions".into(),
                protocol: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://api.openai.com/v1".into()),
                ..Default::default()
            },
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API".into(),
                protocol: "responses".into(),
                base_url: "https://api.openai.com/v1".into(),
                default_model: String::new(),
                models: vec![],
                models_url: Some("https://api.openai.com/v1".into()),
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                ..Default::default()
            },
        ],
    }
}

/// OpenRouter exposes a normalized OpenAI Chat Completions endpoint, but can
/// route one request to many vendor backends. Keep its request surface strict:
/// free and non-reasoning models must not receive vendor-specific thinking or
/// reasoning-history fields, and tool calls require providers that advertise
/// support for every supplied parameter.
fn openrouter() -> ProviderSpec {
    ProviderSpec {
        id: "openrouter".into(),
        display: "OpenRouter".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible (text)".into(),
            protocol: "openai".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: String::new(),
            models: vec![],
            // Limit the picker to text-only models that declare native tool
            // support. QAQ-Harness does not yet serialize multimodal content.
            models_url: Some(
                "https://openrouter.ai/api/v1/models?output_modalities=text&supported_parameters=tools&sort=pricing-low-to-high"
                    .into(),
            ),
            has_balance: false,
            supports_thinking: false,
            supports_reasoning_effort: false,
            tool_call_content_null: true,
            supports_reasoning_content: false,
            require_provider_parameters: true,
            ..Default::default()
        }],
    }
}

fn deepseek_web() -> ProviderSpec {
    ProviderSpec {
        id: "deepseek-web".into(),
        display: "DeepSeek Web (CDP Proxy)".into(),
        endpoints: vec![EndpointSpec {
            id: "cdp".into(),
            display: "CDP Proxy (localhost:8080)".into(),
            protocol: "openai".into(),
            base_url: "http://localhost:8080/v1".into(),
            default_model: "deepseek-v4-pro".into(),
            models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
            models_url: Some("http://localhost:8080/v1".into()),
            user_id_mode: Some(UserSendMode::Body),
            has_balance: false,
            supports_thinking: true,
            stateful: true,
            ..Default::default()
        }],
    }
}

/// OpenCode Go（订阅）：https://opencode.ai/zen/go/v1
///
/// 端点与参数语义以本家 opencode 客户端为准（模型目录
/// `https://models.opencode.ai/api.json`，provider id `opencode-go`）：
/// - 默认协议 `@ai-sdk/openai-compatible`（chat/completions）：kimi / deepseek /
///   glm / mimo / qwen / hy3 全走该通道；流式推理内容字段 `reasoning_content`
///   （api.json 各模型 `interleaved.field = "reasoning_content"`）。
/// - **推理开关**：本家对 opencode-go **不发** `thinking`/`enable_thinking`/
///   `chat_template_args`（那些只发给 zai/zhipuai、dashscope、baseten 等特定
///   provider）——推理默认开启，只发 OpenAI 标准 `reasoning_effort`。
///   故 `supports_thinking: false`、`supports_reasoning_effort: true`。
/// - 协议覆盖（api.json `model.provider.npm`）：
///   - `grok-4.5` / `gpt-5.6-luna` → `@ai-sdk/openai`（Responses API）→ 独立
///     `responses` 端点；
///   - `minimax-m3` / `minimax-m2.7` → `@ai-sdk/anthropic`（messages 协议，
///     QAQ-Harness 未实现 anthropic 通道）→ 暂不提供。
/// - 额外参数（仅 gpt-5.x + opencode 前缀 provider）：`promptCacheKey`（会话
///   ID）、`include: ["reasoning.encrypted_content"]`、`reasoningSummary: "auto"`
///   —— 前两者 QAQ-Harness 无对应概念，不发送；`include` 由
///   `responses_send_include: true` 等价覆盖。
/// - usage 缓存字段未验证（网关不保证 OpenAI 标准 usage）→ `CacheTokenField::None`。
fn opencode_go() -> ProviderSpec {
    ProviderSpec {
        id: "opencode-go".into(),
        display: "OpenCode Go (订阅)".into(),
        endpoints: vec![
            EndpointSpec {
                id: "openai".into(),
                display: "OpenAI-compatible".into(),
                protocol: "openai".into(),
                base_url: "https://opencode.ai/zen/go/v1".into(),
                default_model: "deepseek-v4-flash".into(),
                models: vec![
                    "kimi-k3".into(),
                    "kimi-k2.7-code".into(),
                    "kimi-k2.6".into(),
                    "deepseek-v4-pro".into(),
                    "deepseek-v4-flash".into(),
                    "glm-5.3".into(),
                    "glm-5.2".into(),
                    "glm-5.1".into(),
                    "mimo-v2.5".into(),
                    "mimo-v2.5-pro".into(),
                    "qwen3.8-max".into(),
                    "qwen3.7-max".into(),
                    "qwen3.7-plus".into(),
                    "qwen3.6-plus".into(),
                    "hy3".into(),
                ],
                models_url: Some("https://opencode.ai/zen/go/v1".into()),
                cache_field: CacheTokenField::None,
                has_balance: false,
                supports_thinking: false,
                supports_reasoning_effort: true,
                ..Default::default()
            },
            // Grok 4.5 / GPT-5.6 Luna：本家走 Responses API（@ai-sdk/openai）。
            // effort 档位上限取 "high"：grok-4.5 仅 low/medium/high（超档 400），
            // gpt-5.6-luna 的 xhigh/max 待网关验证后放开。
            EndpointSpec {
                id: "responses".into(),
                display: "Responses API (Grok 4.5 / GPT-5.6 Luna)".into(),
                protocol: "responses".into(),
                base_url: "https://opencode.ai/zen/go/v1".into(),
                default_model: "grok-4.5".into(),
                models: vec!["grok-4.5".into(), "gpt-5.6-luna".into()],
                models_url: Some("https://opencode.ai/zen/go/v1".into()),
                responses_path: Some("/responses".into()),
                cache_field: CacheTokenField::None,
                has_balance: false,
                supports_thinking: false,
                supports_reasoning_effort: true,
                supports_reasoning_content: false,
                responses_effort_max: "high".into(),
                responses_web_search: false,
                responses_echo_web_search_call: false,
                beta: true,
                ..Default::default()
            },
        ],
    }
}

fn providers() -> Vec<ProviderSpec> {
    vec![
        deepseek(),
        qwen(),
        glm(),
        kimi(),
        mimo(),
        minimax(),
        doubao(),
        openai(),
        openrouter(),
        deepseek_web(),
        opencode_go(),
    ]
}

// ── Lookup ──

pub fn all_providers() -> Vec<ProviderSpec> {
    providers()
}

pub fn find_provider(id: &str) -> Option<ProviderSpec> {
    providers().into_iter().find(|p| p.id == id)
}

pub fn find_endpoint(provider_id: &str, endpoint_id: &str) -> Option<EndpointSpec> {
    find_provider(provider_id).and_then(|p| p.endpoints.into_iter().find(|e| e.id == endpoint_id))
}

pub fn first_endpoint_for(provider_id: &str) -> Option<EndpointSpec> {
    find_provider(provider_id).and_then(|p| p.endpoints.into_iter().next())
}

pub fn first_provider_endpoint() -> (String, String) {
    let providers = all_providers();
    let p = providers.first();
    let pid = p.map(|p| p.id.clone()).unwrap_or_else(|| "deepseek".into());
    let ep = first_endpoint_for(&pid)
        .map(|e| e.id.clone())
        .unwrap_or_else(|| "openai".into());
    (pid, ep)
}

// ── Model discovery ──

pub fn models_url_for(provider_id: &str, endpoint_id: &str) -> Option<String> {
    let ep = find_endpoint(provider_id, endpoint_id)?;
    let base = ep.models_url.as_deref().unwrap_or(&ep.base_url);
    // Most presets store a base URL, but OpenRouter's model discovery needs
    // documented query filters. Treat an explicit /models URL as complete.
    if base.contains("/models") {
        return Some(base.to_string());
    }
    let stripped = base.trim_end_matches('/');
    Some(format!("{}/models", stripped))
}

pub fn fetch_models(provider_id: &str, endpoint_id: &str, api_key: &str) -> Vec<String> {
    if find_endpoint(provider_id, endpoint_id).is_none() {
        return vec![];
    };

    let url = match models_url_for(provider_id, endpoint_id) {
        Some(u) => u,
        None => return vec![],
    };

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .user_agent(qaqh_types::QAQH_USER_AGENT)
        .build()
        .into();

    match agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", api_key))
        .call()
    {
        Ok(resp) => {
            let body: Result<serde_json::Value, _> = resp.into_body().read_json();
            match body {
                Ok(v) => {
                    let models: Vec<String> = v["data"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m["id"].as_str().map(String::from))
                                .filter(|id| !id.starts_with("deepseek-re"))
                                .collect()
                        })
                        .unwrap_or_default();
                    if models.is_empty() { vec![] } else { models }
                }
                Err(_) => vec![],
            }
        }
        Err(_) => vec![],
    }
}

pub fn default_model_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.default_model.clone())
        .unwrap_or_default()
}

pub fn protocol_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.protocol.clone())
        .unwrap_or_else(|| "openai".into())
}

pub fn base_url_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.base_url.clone())
        .unwrap_or_default()
}

// ── Backward compatibility ──

pub fn migrate_provider_id(old_pid: &str) -> (String, String) {
    if find_provider(old_pid).is_some() {
        let ep = first_endpoint_for(old_pid)
            .map(|e| e.id.clone())
            .unwrap_or_else(|| "openai".into());
        (old_pid.to_string(), ep)
    } else {
        ("deepseek".into(), "openai".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_text_endpoint_has_router_safe_capabilities() {
        let endpoint = find_endpoint("openrouter", "openai").expect("OpenRouter endpoint");
        assert_eq!(endpoint.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(
            models_url_for("openrouter", "openai").as_deref(),
            Some(
                "https://openrouter.ai/api/v1/models?output_modalities=text&supported_parameters=tools&sort=pricing-low-to-high"
            )
        );
        assert!(!endpoint.has_balance);
        assert!(!endpoint.supports_thinking);
        assert!(!endpoint.supports_reasoning_effort);
        assert!(endpoint.tool_call_content_null);
        assert!(!endpoint.supports_reasoning_content);
        assert!(endpoint.require_provider_parameters);
    }

    #[test]
    fn existing_openai_preset_keeps_legacy_capabilities() {
        let endpoint = find_endpoint("openai", "openai").expect("OpenAI endpoint");
        assert!(endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.tool_call_content_null);
        assert!(endpoint.supports_reasoning_content);
        assert!(!endpoint.require_provider_parameters);
    }

    #[test]
    fn openai_responses_endpoint_exists() {
        let endpoint = find_endpoint("openai", "responses").expect("OpenAI Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.openai.com/v1");
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert!(endpoint.responses_search_function_alias.is_none());
    }

    #[test]
    fn protocol_for_responses_endpoint() {
        let proto = protocol_for("openai", "responses");
        assert_eq!(proto, "responses");
    }

    #[test]
    fn chat_endpoint_still_works() {
        let proto = protocol_for("openai", "openai");
        assert_eq!(proto, "openai");
        let url = base_url_for("openai", "openai");
        assert_eq!(url, "https://api.openai.com/v1");
    }

    #[test]
    fn deepseek_responses_endpoint_exists() {
        let endpoint = find_endpoint("deepseek", "responses").expect("DeepSeek Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.deepseek.com");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(endpoint.default_model, "deepseek-v4-flash");
        assert_eq!(endpoint.models, vec!["deepseek-v4-flash".to_string()]);
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert_eq!(
            endpoint.responses_search_function_alias.as_deref(),
            Some("qaqh_search")
        );
    }

    #[test]
    fn deepseek_responses_protocol_flows_through() {
        assert_eq!(protocol_for("deepseek", "responses"), "responses");
        assert_eq!(protocol_for("deepseek", "openai"), "openai");
        // Unknown endpoint falls back to the openai protocol (backward compat).
        assert_eq!(protocol_for("deepseek", "unknown"), "openai");
    }

    #[test]
    fn qwen_responses_endpoint_exists() {
        let endpoint = find_endpoint("qwen", "responses").expect("Qwen Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://dashscope.aliyuncs.com");
        assert_eq!(
            endpoint.responses_path.as_deref(),
            Some("/compatible-mode/v1/responses")
        );
        assert_eq!(
            models_url_for("qwen", "responses").as_deref(),
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        // Bridge must not disturb the default chat endpoint.
        assert_eq!(protocol_for("qwen", "openai"), "openai");
    }

    #[test]
    fn doubao_responses_endpoint_exists() {
        let endpoint = find_endpoint("doubao", "responses").expect("Doubao Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://ark.cn-beijing.volces.com");
        assert_eq!(
            endpoint.responses_path.as_deref(),
            Some("/api/v3/responses")
        );
        assert_eq!(
            models_url_for("doubao", "responses").as_deref(),
            Some("https://ark.cn-beijing.volces.com/api/v3/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert_eq!(protocol_for("doubao", "openai"), "openai");
    }

    #[test]
    fn mimo_responses_endpoint_exists() {
        let endpoint = find_endpoint("mimo", "responses").expect("MiMo Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.xiaomimimo.com/v1");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(
            models_url_for("mimo", "responses").as_deref(),
            Some("https://api.xiaomimimo.com/v1/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert_eq!(endpoint.responses_effort_max, "high");
        assert!(!endpoint.supports_reasoning_content);
        assert_eq!(protocol_for("mimo", "openai"), "openai");
    }

    #[test]
    fn opencode_go_chat_endpoint_exists() {
        let endpoint = find_endpoint("opencode-go", "openai").expect("opencode-go endpoint");
        assert_eq!(endpoint.protocol, "openai");
        assert_eq!(endpoint.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(endpoint.default_model, "deepseek-v4-flash");
        assert_eq!(
            endpoint.models.len(),
            15,
            "official Go model list (chat channel)"
        );
        assert!(endpoint.models.contains(&"deepseek-v4-flash".to_string()));
        assert!(endpoint.models.contains(&"kimi-k3".to_string()));
        assert!(!endpoint.models.contains(&"grok-4.5".to_string()));
        assert!(!endpoint.models.contains(&"minimax-m3".to_string()));
        // 本家不发 thinking 参数（推理默认开），只发 reasoning_effort。
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(matches!(endpoint.cache_field, CacheTokenField::None));
        assert!(!endpoint.has_balance);
        assert_eq!(
            models_url_for("opencode-go", "openai").as_deref(),
            Some("https://opencode.ai/zen/go/v1/models")
        );
    }

    #[test]
    fn opencode_go_responses_endpoint_exists() {
        let endpoint = find_endpoint("opencode-go", "responses").expect("opencode-go Responses");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(
            endpoint.models,
            vec!["grok-4.5".to_string(), "gpt-5.6-luna".to_string()]
        );
        assert_eq!(endpoint.default_model, "grok-4.5");
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        // grok-4.5 最高档 high（超档 400）；luna 的 xhigh/max 待验证后放开。
        assert_eq!(endpoint.responses_effort_max, "high");
        assert!(!endpoint.supports_reasoning_content);
        // minimax 走 anthropic messages 协议（未实现）→ 不进任何端点。
        assert!(!endpoint.models.contains(&"minimax-m3".to_string()));
    }
}
