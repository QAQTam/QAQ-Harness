//! OpenAI Chat Completions API streaming client — synchronous facade over reqwest.
//! Includes retry with exponential backoff for transient errors (429, 500, 503, transport).

use futures::StreamExt;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_types::{CacheTokenField, ThinkingParamMode};
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sse::SseDecoder;
use super::types::{
    ProviderConfig, StreamEvent, normalize_reasoning_effort, safe_provider_error_body,
};

/// Polling interval for SSE streaming. When no data arrives within this
/// interval, the outer Tokio timeout lets us check the cancel flag before
/// polling the same stream again.
const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Crate-global tokio runtime for reqwest I/O.
/// Uses current-thread scheduler — all async I/O serialises on the
/// calling thread via Runtime::block_on.
static FALLBACK_RT: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create qaqh-gate fallback tokio runtime")
});

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    FALLBACK_RT.block_on(f)
}

/// Check whether the cancel flag is set.
fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false)
}

/// Sleep for `delay` but wake up every 100ms to check the cancel flag.
/// Returns `true` if cancelled during the sleep.
fn sleep_with_cancel(delay: Duration, cancel: Option<&Arc<AtomicBool>>) -> bool {
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

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_SECS: u64 = 1;

fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}

/// Providers use several OpenAI-compatible names for the same hidden
/// reasoning stream. Keep that data out of `content`, which is user-visible.
fn reasoning_delta<'a>(delta: &'a serde_json::Value) -> Option<&'a str> {
    [
        "reasoning_content",
        "reasoning",
        "thinking",
        "analysis_content",
    ]
    .into_iter()
    .find_map(|key| delta.get(key).and_then(|value| value.as_str()))
}

/// Some compatible endpoints put reasoning inside `content` using think tags.
/// Split complete tags before events reach the frontend. The normal provider
/// fields above remain the authoritative path; this is a compatibility guard.
fn split_inline_thinking(text: &str, in_thinking: &mut bool) -> Vec<(bool, String)> {
    let mut result = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let marker = if *in_thinking { "</think>" } else { "<think>" };
        match rest.find(marker) {
            Some(index) => {
                if index > 0 {
                    result.push((*in_thinking, rest[..index].to_string()));
                }
                *in_thinking = !*in_thinking;
                rest = &rest[index + marker.len()..];
            }
            None => {
                result.push((*in_thinking, rest.to_string()));
                break;
            }
        }
    }
    result
}

/// Reusable HTTP client shared across all chat requests.
/// Connection pool, DNS cache, and TLS session cache are preserved.
static GLOBAL_CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(|| {
    Client::builder()
        // A streaming response can legitimately last longer than five minutes.
        // Bound connection establishment separately and keep idle pooled sockets
        // alive, while leaving enough total budget for long reasoning streams.
        .connect_timeout(Duration::from_secs(15))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .pool_idle_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(30 * 60))
        .user_agent(qaqh_types::QAQH_USER_AGENT)
        .build()
        .expect("failed to build reqwest client")
});

fn backoff_delay(attempt: u32) -> Duration {
    let secs = BASE_DELAY_SECS * 2u64.pow(attempt.saturating_sub(1));
    Duration::from_secs(secs.min(30))
}

/// Send a chat completion request and stream SSE events via `on_event`.
///
/// `cancel` is an optional `Arc<AtomicBool>` that, when set to `true`, causes
/// the streaming loop to abort within one `SSE_POLL_INTERVAL`. This keeps
/// cancellation responsive while the HTTP response body is being streamed.
#[allow(clippy::string_slice)]
pub fn chat_stream_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let messages = normalize_skill_envelope(provider, messages).map_err(anyhow::Error::msg)?;
    // Stateful 模式：只发增量消息（最后一条 user + 其后的 tool 结果）
    let messages = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        messages
    };

    let api_msgs = convert_messages(provider, messages, None);

    let openai_tools: Option<Vec<serde_json::Value>> = tools.map(|tds| {
        tds.into_iter()
            .map(|td| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": td.function.name,
                        "description": td.function.description,
                        "parameters": td.function.parameters,
                    }
                })
            })
            .collect()
    });

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("messages".into(), serde_json::Value::Array(api_msgs));
    body_map.insert("stream".into(), serde_json::json!(true));
    if provider.include_stream_usage {
        body_map.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    if provider.supports_thinking {
        match provider.thinking_mode {
            ThinkingParamMode::OpenAi => {
                body_map.insert("thinking".into(), serde_json::json!({"type": "enabled"}));
            }
            ThinkingParamMode::QwenEnableThinking => {
                body_map.insert("enable_thinking".into(), serde_json::json!(true));
            }
            ThinkingParamMode::MiniMaxAdaptive => {
                body_map.insert("thinking".into(), serde_json::json!({"type": "adaptive"}));
                body_map.insert("reasoning_split".into(), serde_json::json!(true));
            }
        }
    }
    body_map.insert("max_tokens".into(), serde_json::json!(max_tokens));

    if provider.supports_reasoning_effort {
        if let Some(ref e) = effort {
            // QAQ-Harness always reasons: promote none/minimal/disable to the
            // lowest thinking level instead of sending them through.
            let e = normalize_reasoning_effort(Some(e)).unwrap_or_else(|| e.clone());
            body_map.insert("reasoning_effort".into(), serde_json::json!(e));
        }
    }
    if let Some(sample) = provider.do_sample {
        body_map.insert("do_sample".into(), serde_json::json!(sample));
    }
    if let Some(ref t) = openai_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
        if provider.require_provider_parameters {
            body_map.insert(
                "provider".into(),
                serde_json::json!({"require_parameters": true}),
            );
        }
    }
    if let Some(ref uid) = user_id {
        if provider.user_id_mode.is_some() {
            body_map.insert("user_id".into(), serde_json::json!(uid));
        }
    }

    let body = serde_json::Value::Object(body_map);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    let mut attempt = 0u32;
    // Reuse the module-level client for connection pooling. Cancellation
    // responsiveness is handled by the polling timeout in stream_sse, not by
    // a client-level read timeout.

    loop {
        attempt += 1;

        // Check cancel before sending the request
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        match block_on(async {
            GLOBAL_CLIENT
                .post(&url)
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
        }) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 200 && status < 300 {
                    return stream_sse(resp, provider, user_id.as_deref(), cancel, on_event);
                }
                // HTTP error — read body for details
                let text = block_on(resp.text()).unwrap_or_default();
                let code_desc = http_error_description(status);
                if attempt >= MAX_RETRIES || !is_retryable(status) {
                    let msg = format!("OpenAI API HTTP {} ({})", status, code_desc);
                    let detail = if status == 401 {
                        "authentication failed".into()
                    } else {
                        safe_provider_error_body(&text, &provider.api_key)
                    };
                    on_event(StreamEvent::Error(format!("{}: {}", msg, detail)));
                    return Err(anyhow::anyhow!("{}", msg));
                }

                let delay = backoff_delay(attempt);
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: MAX_RETRIES,
                    delay_secs: delay.as_secs(),
                    error: format!("HTTP {} ({})", status, code_desc),
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
            Err(e) => {
                // Transport / timeout / connection errors
                if attempt >= MAX_RETRIES {
                    let msg = format!("HTTP transport error: {e}");
                    on_event(StreamEvent::Error(msg.clone()));
                    return Err(anyhow::anyhow!("{}", msg));
                }

                let delay = backoff_delay(attempt);
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: MAX_RETRIES,
                    delay_secs: delay.as_secs(),
                    error: format!("{e}"),
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
        }
    }
}

/// `QAQH_SSE_TRACE=<path>`：将 gate 派生的每个流式事件按到达序追加写入文件
/// （`<seq>\t<类型>\t<长度>`），用于核对 reasoning/content/tool 在链路的忠实
/// 流转与先后顺序（诊断思考链/正文交错问题）。不设该变量时零开销。
struct SseTrace {
    file: Option<std::fs::File>,
    seq: u64,
}

impl SseTrace {
    fn from_env() -> Self {
        let file = std::env::var_os("QAQH_SSE_TRACE").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self { file, seq: 0 }
    }

    fn record(&mut self, event: &StreamEvent) {
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

/// 解析单个 SSE chunk 的 `delta` 对象，按模型输出意图派生流式事件。
///
/// **字段处理顺序（顺序即语义）**：
/// 1. `reasoning_content`/`reasoning`/`thinking`/`analysis_content` 在前；
/// 2. `content`（含 inline think 标签切分 + DSML 工具检测）其次；
/// 3. 原生 `tool_calls` 最后。
///
/// 同一 chunk 可能**同时携带 `reasoning_content`（思考尾部）与 `content`
/// （正文开头）两个字段**——模型输出顺序是 reasoning 在前、content 在后
/// （journal `server_ts` 同毫秒拆分的两条 `round_delta` 即证据）。若先发
/// content 会把正文插到思考链中间，造成前端"思考链与正文错排"。
fn emit_delta_fields(
    delta: &serde_json::Value,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_acc: &mut HashMap<usize, (String, String, String)>,
    dsml_buf: &mut String,
    dsml_seen: &mut HashSet<String>,
    inline_thinking: &mut bool,
    traced: &mut dyn FnMut(StreamEvent),
) {
    // 1. Reasoning content 先于 content（同一 chunk 双字段时的正确顺序）。
    if let Some(rc) = reasoning_delta(delta) {
        let r = rc.to_string();
        reasoning_buf.push_str(&r);
        traced(StreamEvent::ReasoningDelta(r));
    }

    // 2. Text content（含 inline thinking 切分与 DSML 工具检测）。
    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
        for (is_reasoning, t) in split_inline_thinking(text, inline_thinking) {
            if is_reasoning {
                reasoning_buf.push_str(&t);
                traced(StreamEvent::ReasoningDelta(t));
            } else {
                text_buf.push_str(&t);
                traced(StreamEvent::ContentDelta(t.clone()));

                // DSML tool call detection in content stream
                dsml_buf.push_str(&t);
                let mut search_from = 0usize;
                while let Some(start) = dsml_buf[search_from..].find("<｜DSML｜invoke name=\"") {
                    let abs_start = search_from + start;
                    let after_tag = abs_start + "<｜DSML｜invoke name=\"".len();
                    if let Some(rest) = dsml_buf.get(after_tag..) {
                        if let Some(quote_end) = rest.find('"') {
                            let name = rest[..quote_end].to_string();
                            if dsml_seen.insert(name.clone()) {
                                let idx = dsml_seen.len() - 1;
                                traced(StreamEvent::ToolCallProgress {
                                    index: idx,
                                    id: format!("dsml_tc_{}", idx),
                                    name,
                                    args_so_far: String::new(),
                                });
                            }
                            search_from = after_tag + quote_end + 1;
                            continue;
                        }
                    }
                    break;
                }
            }
        }
    }

    // 3. Tool calls (native OpenAI format)
    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let entry = tool_acc.entry(idx).or_insert_with(|| {
                let tid = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tname = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                (tid, tname, String::new())
            });
            if let Some(args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
            {
                entry.2.push_str(args);
                traced(StreamEvent::ToolCallProgress {
                    index: idx,
                    id: entry.0.clone(),
                    name: entry.1.clone(),
                    args_so_far: entry.2.clone(),
                });
            }
        }
    }
}

/// 单个 SSE 帧的处理结果。
enum FrameAction {
    /// 继续消费流。
    Continue,
    /// 收到 `[DONE]` 终止标记：停止读取。
    Done,
}

/// 处理一帧 chat completions SSE 数据（`data:` payload）。
///
/// 由 [`stream_sse`] 调用；帧行解码与聚合由共享的 [`SseDecoder`] 完成，
/// 本函数只负责 JSON 解析与事件派生，便于独立测试。
#[allow(clippy::too_many_arguments)]
fn handle_chat_frame(
    data_str: &str,
    provider: &ProviderConfig,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_acc: &mut HashMap<usize, (String, String, String)>,
    dsml_buf: &mut String,
    dsml_seen: &mut HashSet<String>,
    usage_info: &mut Option<UsageInfo>,
    stop_reason: &mut Option<String>,
    inline_thinking: &mut bool,
    traced: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<FrameAction> {
    if data_str.is_empty() {
        return Ok(FrameAction::Continue);
    }
    // `[DONE]` 是 chat completions 官方流终止标记：立即结束读取。
    // 此前此处 `continue` 会在 [DONE] 后继续读——若连接随后被服务器
    // RST（资源紧张/代理超时），events.next() 返回 Err → 已完整输出
    // 的作答被误判为 TurnFailed（"完成作答后返回错误"的根因）。
    if data_str == "[DONE]" {
        return Ok(FrameAction::Done);
    }

    let ev: serde_json::Value = match serde_json::from_str(data_str) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("OpenAI SSE: deserialize fail: {} — data: {}", e, data_str);
            return Ok(FrameAction::Continue);
        }
    };

    // Parse choices
    if let Some(choices) = ev.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            let finish = choice.get("finish_reason").and_then(|v| v.as_str());
            if let Some(fr) = finish {
                if !fr.is_empty() && fr != "null" {
                    *stop_reason = Some(fr.to_string());
                }
            }

            if let Some(delta) = choice.get("delta") {
                emit_delta_fields(
                    delta,
                    text_buf,
                    reasoning_buf,
                    tool_acc,
                    dsml_buf,
                    dsml_seen,
                    inline_thinking,
                    traced,
                );
            }
        }
    }

    // Usage info (may appear in any chunk).
    // When stream_options.include_usage=true the field is present on
    // every chunk but is null for all intermediate chunks; only the final
    // chunk before [DONE] carries actual token counts.  Skip null to avoid
    // emitting zero-value UsageUpdate events that cause the info panel to
    // flicker between 0 and real values.
    if let Some(u) = ev.get("usage").filter(|v| !v.is_null()) {
        let pt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let ct = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let (hit, miss, cache_usage_reported) = match provider.cache_field {
            CacheTokenField::PromptCacheHitTokens => {
                let hit_value = u.get("prompt_cache_hit_tokens");
                let miss_value = u.get("prompt_cache_miss_tokens");
                (
                    hit_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    miss_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    hit_value.is_some() || miss_value.is_some(),
                )
            }
            CacheTokenField::PromptDetailsCached => {
                let cached_value = u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"));
                let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                (cached, pt.saturating_sub(cached), cached_value.is_some())
            }
            CacheTokenField::UsageCachedTokens => {
                let cached_value = u.get("cached_tokens");
                let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                (cached, pt.saturating_sub(cached), cached_value.is_some())
            }
            CacheTokenField::None => (0, 0, false),
        };
        let rt = u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let usage = UsageInfo {
            prompt_tokens: pt,
            completion_tokens: ct,
            total_tokens: pt + ct,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            reasoning_tokens: rt,
            cache_usage_reported: Some(cache_usage_reported),
        };
        *usage_info = Some(usage.clone());
        traced(StreamEvent::UsageUpdate(usage));
    }

    Ok(FrameAction::Continue)
}

fn stream_sse(
    resp: reqwest::Response,
    provider: &ProviderConfig,
    _user_id: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut decoder = SseDecoder::new();
    let mut stream = resp.bytes_stream();

    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_acc: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut dsml_buf = String::new();
    let mut dsml_seen: HashSet<String> = HashSet::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;
    let mut inline_thinking = false;

    let mut trace = SseTrace::from_env();
    let callback = on_event;
    let mut traced = move |event: StreamEvent| {
        trace.record(&event);
        callback(event);
    };

    let mut done_reached = false;
    loop {
        // Check cancel before each read attempt
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        // 先消费缓冲中已完整的帧。
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_chat_frame(
                &data_str,
                provider,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_acc,
                &mut dsml_buf,
                &mut dsml_seen,
                &mut usage_info,
                &mut stop_reason,
                &mut inline_thinking,
                &mut traced,
            ) {
                Ok(FrameAction::Continue) => {}
                Ok(FrameAction::Done) => done_reached = true,
                Err(e) => return Err(e),
            }
        }
        if done_reached {
            break;
        }

        let chunk = match block_on(async {
            tokio::time::timeout(SSE_POLL_INTERVAL, stream.next()).await
        }) {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                let msg = format!("SSE read error: {e}");
                traced(StreamEvent::Error(msg.clone()));
                return Err(anyhow::anyhow!("{}", msg));
            }
            Ok(None) => break,         // EOF
            Err(_elapsed) => continue, // timeout → check cancel, retry
        };
        decoder.push(&chunk);
    }

    // EOF 残帧：处理未以空行收尾的聚合（补行尾+空行触发消费，与帧解析的
    // "空行定界事件"语义一致；等价于 eventsource-stream 的 EOF flush）。
    if !done_reached && decoder.has_pending() {
        decoder.push(b"\n\n");
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_chat_frame(
                &data_str,
                provider,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_acc,
                &mut dsml_buf,
                &mut dsml_seen,
                &mut usage_info,
                &mut stop_reason,
                &mut inline_thinking,
                &mut traced,
            ) {
                Ok(FrameAction::Continue) => {}
                Ok(FrameAction::Done) => {}
                Err(e) => return Err(e),
            }
        }
    }

    // Build final message from accumulated content
    let mut blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_buf.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning_buf,
        });
    }

    // ── DSML integration: extract tool calls from text content ──
    let _final_text = if crate::tool_parser::has_dsml(&text_buf) {
        let (cleaned, dsml_tcs) = crate::tool_parser::parse_dsml_tool_calls(&text_buf, &[]);
        // Merge DSML tool calls into tool_acc (with unique ids to avoid collision)
        let base_idx = tool_acc.len();
        for (i, tc) in dsml_tcs.iter().enumerate() {
            let idx = base_idx + i;
            tool_acc.insert(
                idx,
                (
                    tc.id.clone(),
                    tc.function.name.clone(),
                    tc.function.arguments.to_string(),
                ),
            );
        }
        if !cleaned.is_empty() {
            blocks.push(ContentBlock::text(&cleaned));
        }
        cleaned
    } else {
        if !text_buf.is_empty() {
            blocks.push(ContentBlock::text(&text_buf));
        }
        text_buf.clone()
    };

    let mut sorted: Vec<(usize, String, String, String)> = tool_acc
        .into_iter()
        .map(|(idx, (id, name, args))| (idx, id, name, args))
        .collect();
    sorted.sort_by_key(|(idx, _, _, _)| *idx);
    for (_idx, id, name, args_json) in sorted {
        let input: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
        blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    let raw_message = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    };

    traced(StreamEvent::Done {
        raw_message,
        usage: usage_info,
        stop_reason,
    });

    Ok(())
}

// ── Message conversion ──

/// Stateful 模式：只保留增量消息。
/// Web 代理端已记住完整上下文。
/// 规则：
///   - 首次请求（无 assistant 历史）：发 system + 所有消息
///   - 后续请求：只发最后一条 assistant 之后的消息
fn filter_stateful_messages(messages: Vec<Message>) -> Vec<Message> {
    if messages.is_empty() {
        return messages;
    }

    let last_asst_idx = messages.iter().rposition(|m| m.role == "assistant");
    let start = last_asst_idx.map(|i| i + 1).unwrap_or(0);
    let is_first = start == 0;

    // Debug: 打印过滤前的消息角色序列
    #[cfg(debug_assertions)]
    {
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        eprintln!(
            "[filter] 输入: {:?} | last_asst={:?} start={}",
            roles, last_asst_idx, start
        );
    }

    if is_first {
        return messages;
    }

    let mut out: Vec<Message> = Vec::new();

    // 保留 start 之后的新消息
    for msg in &messages[start..] {
        out.push(msg.clone());
    }

    // 兜底：如果没有任何新消息，且最后一条是 user/tool（非 assistant），保留它
    if out.is_empty() {
        if let Some(last) = messages.last() {
            if last.role != "assistant" {
                out.push(last.clone());
            }
        }
    }

    let out_roles: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
    eprintln!("[filter] 输出: {:?} (is_first={})", out_roles, is_first);

    out
}

fn normalize_skill_envelope(
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

fn convert_messages(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    system: Option<String>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = system {
        if !sys.is_empty() {
            out.push(serde_json::json!({"role": "system", "content": sys}));
        }
    }

    for msg in messages {
        let name = &msg.name;
        match msg.role.as_str() {
            "system" | "developer" => {
                // `developer`（Responses 专属运行时注入角色）在 Chat
                // Completions 协议下不存在——降级为 system，语义最接近。
                if let Some(tb) = msg.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                }) {
                    let mut obj = serde_json::json!({"role": "system", "content": tb});
                    if let Some(n) = name {
                        obj["name"] = serde_json::json!(n);
                    }
                    out.push(obj);
                }
            }
            "user" => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut image_refs: Vec<String> = Vec::new();
                let mut img_idx: usize = 0;
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text_parts.push(t.clone()),
                        ContentBlock::Image { mime_type, data } => {
                            image_refs.push(format!(
                                "[Image #{img_idx}: {mime_type}, ~{} bytes — to analyze, call: image_query(image_index={img_idx}, prompt=\"describe this image\")]",
                                data.len()
                            ));
                            img_idx += 1;
                        }
                        _ => {}
                    }
                }
                let mut combined_text = text_parts.join("");
                if !image_refs.is_empty() {
                    if !combined_text.is_empty() {
                        combined_text.push('\n');
                    }
                    combined_text.push_str(&image_refs.join("\n"));
                }
                let mut obj = serde_json::json!({"role": "user", "content": combined_text});
                if let Some(n) = name {
                    obj["name"] = serde_json::json!(n);
                }
                out.push(obj);
            }
            "assistant" => {
                let mut content = String::new();
                let mut reasoning = String::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => content.push_str(text),
                        ContentBlock::Reasoning { reasoning: r } => reasoning.push_str(r),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                let mut obj = serde_json::json!({"role": "assistant"});
                if !content.is_empty() {
                    obj["content"] = serde_json::json!(content);
                } else if tool_calls.is_empty() && !reasoning.is_empty() {
                    obj["content"] = serde_json::json!("[Thinking complete]");
                }
                if provider.supports_reasoning_content && !reasoning.is_empty() {
                    obj["reasoning_content"] = serde_json::json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    if provider.tool_call_content_null && obj.get("content").is_none() {
                        obj["content"] = serde_json::Value::Null;
                    }
                    obj["tool_calls"] = serde_json::json!(tool_calls);
                }
                if obj.as_object().map_or(false, |m| m.len() > 1) {
                    out.push(obj);
                }
            }
            "tool" => {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        result,
                        ..
                    } = block
                    {
                        out.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": result.project_for_model().to_string(),
                        }));
                    }
                }
            }
            _ => {}
        }
    }

    out
}

// ── Synchronous (non-streaming) chat ──

pub fn chat_sync_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let messages = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        messages
    };
    let api_msgs = convert_messages(provider, messages, None);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    let mut body = serde_json::json!({
        "model": model,
        "messages": api_msgs,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if provider.supports_thinking {
        let thinking = match provider.thinking_mode {
            ThinkingParamMode::OpenAi => serde_json::json!({"type": "enabled"}),
            ThinkingParamMode::QwenEnableThinking => serde_json::json!(true),
            ThinkingParamMode::MiniMaxAdaptive => serde_json::json!({"type": "adaptive"}),
        };
        body["thinking"] = thinking;
    }

    let resp = block_on(
        GLOBAL_CLIENT
            .post(&url)
            .header("Authorization", format!("Bearer {}", provider.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send(),
    )
    .map_err(|e| format!("compact request failed: {e}"))?;

    let json: serde_json::Value =
        block_on(resp.json()).map_err(|e| format!("compact parse failed: {e}"))?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "compact: no content in response".to_string())
}

// ── URL builder ──

fn build_chat_url(base_url: &str, chat_path: Option<&str>) -> String {
    if let Some(path) = chat_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    }
}

// ── Error descriptions ──

fn http_error_description(status: u16) -> &'static str {
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

#[cfg(test)]
mod skill_envelope_tests {
    use super::*;

    #[test]
    fn stateful_first_request_does_not_duplicate_system_slots() {
        let messages = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::user("hi"),
            Message::system("envelope"),
        ];
        let filtered = filter_stateful_messages(messages.clone());
        assert_eq!(filtered.len(), messages.len());
    }

    #[test]
    fn stateful_increment_always_keeps_authoritative_tail_envelope() {
        let messages = vec![
            Message::system("base"),
            Message::user("old"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::text("done")],
            },
            Message::user("next"),
            Message::system("<skill_context_envelope />"),
        ];
        let filtered = filter_stateful_messages(messages);
        assert_eq!(
            filtered
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "system"]
        );
        assert!(
            matches!(&filtered[1].content[0], ContentBlock::Text { text } if text.contains("skill_context_envelope"))
        );
    }

    fn provider() -> ProviderConfig {
        ProviderConfig::openai(
            "http://test",
            "",
            "m",
            None,
            None,
            ThinkingParamMode::OpenAi,
            CacheTokenField::None,
            false,
            None,
        )
    }

    #[test]
    fn normalizes_reasoning_aliases_and_think_tags() {
        for key in [
            "reasoning_content",
            "reasoning",
            "thinking",
            "analysis_content",
        ] {
            let mut map = serde_json::Map::new();
            map.insert(
                key.to_string(),
                serde_json::Value::String("hidden".to_string()),
            );
            let delta = serde_json::Value::Object(map);
            assert_eq!(reasoning_delta(&delta), Some("hidden"));
        }

        let mut in_thinking = false;
        assert_eq!(
            split_inline_thinking("visible<think>hidden</think>done", &mut in_thinking),
            vec![
                (false, "visible".to_string()),
                (true, "hidden".to_string()),
                (false, "done".to_string()),
            ],
        );
        assert!(!in_thinking);
    }

    #[test]
    fn stateless_provider_can_explicitly_degrade_to_head_dynamic_slot() {
        let provider = provider().with_tail_system_support(false);
        let messages = vec![
            Message::system("base"),
            Message::user("hi"),
            Message::system("<skill_context_envelope />"),
        ];
        let normalized = normalize_skill_envelope(&provider, messages).unwrap();
        assert_eq!(
            normalized
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "system", "user"]
        );
    }

    #[test]
    fn dual_field_chunk_emits_reasoning_before_content() {
        // 同一 SSE chunk 的 delta 同时携带 reasoning_content（思考尾部）与
        // content（正文开头）时，必须按模型输出意图先 reasoning 后 content。
        // 反向（先 content）会把正文插到思考链中间——journal server_ts 同毫秒
        // 拆分的两条 round_delta 即该场景的证据（BUG：思考链与正文错排）。
        let mut delta = serde_json::Map::new();
        delta.insert(
            "reasoning_content".into(),
            serde_json::Value::String("engineer assistant.".into()),
        );
        delta.insert(
            "content".into(),
            serde_json::Value::String("你好！我是 Dee".into()),
        );
        let delta = serde_json::Value::Object(delta);

        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut tool_acc = HashMap::new();
        let mut dsml_buf = String::new();
        let mut dsml_seen = HashSet::new();
        let mut inline_thinking = false;
        let mut events = Vec::new();
        let mut traced = |ev: StreamEvent| events.push(ev);

        emit_delta_fields(
            &delta,
            &mut text_buf,
            &mut reasoning_buf,
            &mut tool_acc,
            &mut dsml_buf,
            &mut dsml_seen,
            &mut inline_thinking,
            &mut traced,
        );

        assert_eq!(
            events
                .iter()
                .map(|ev| match ev {
                    StreamEvent::ReasoningDelta(_) => "reasoning",
                    StreamEvent::ContentDelta(_) => "content",
                    _ => "other",
                })
                .collect::<Vec<_>>(),
            vec!["reasoning", "content"],
            "dual-field chunk must emit reasoning before content"
        );
        assert_eq!(reasoning_buf, "engineer assistant.");
        assert_eq!(text_buf, "你好！我是 Dee");
    }

    #[test]
    fn stateful_provider_refuses_silent_head_fallback() {
        let provider = provider()
            .with_stateful(true)
            .with_tail_system_support(false);
        let error = normalize_skill_envelope(
            &provider,
            vec![
                Message::user("hi"),
                Message::system("<skill_context_envelope />"),
            ],
        )
        .unwrap_err();
        assert!(error.contains("SKILL_CONTEXT_SYNC_UNSUPPORTED"));
    }
}
