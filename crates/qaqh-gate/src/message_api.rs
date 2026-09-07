//! Anthropic Messages API streaming client — synchronous facade over reqwest.
//! Covers `POST /v1/messages` and `POST /api/anthropic/v1/messages` (ZCode).
//!
//! Key design notes (mirrors the `openAIToAnthropic` mapping):
//! - `system` is a **top-level `system` field**, not a leading `messages`
//!   entry. `proxybun` verified `glm-5.3-flash` via
//!   `https://open.bigmodel.cn/api/anthropic/v1/messages` with this shape
//!   (`200`). `qaqh-gate:chat_completions_api.rs:convert_messages` treating `system` as
//!   first message is OpenAI-specific and must not be reused here.
//! - `messages` role alternation with `user`/`assistant` only; consecutive
//!   same-role messages are merged to satisfy Anthropic's strict alternation.
//! - Tools: `{name, description, input_schema}` (Anthropic) vs
//!   `{type:function,function:{name,description,parameters}}` (OpenAI).

use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sse::SseDecoder;
use super::transport::{MAX_RETRIES, SSE_POLL_INTERVAL};
use super::transport::{
    SseTrace, backoff_delay, block_on, filter_stateful_messages, http_error_description,
    is_cancelled, is_retryable, normalize_skill_envelope, sleep_with_cancel,
};
use super::types::{EmptyStreamEof, ProviderConfig, StreamEvent, safe_provider_error_body};

fn build_anthropic_url(base_url: &str, anthropic_path: Option<&str>) -> String {
    if let Some(path) = anthropic_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") || base.ends_with("/api/anthropic/v1/messages") {
        base.to_string()
    } else {
        format!("{}/v1/messages", base)
    }
}

fn map_anthropic_stop_reason(s: &str) -> String {
    match s {
        "end_turn" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        _ => s.to_string(),
    }
}

fn extract_text(content: &[ContentBlock]) -> Option<String> {
    let mut out = String::new();
    for b in content {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Convert harness `Message`s to Anthropic `(system, messages)` pair.
///
/// `system` is top-level `system` (string, joined with "\n") per
/// The `openAIToAnthropic` mapping and the ZCode gateway
/// `open.bigmodel.cn/api/anthropic/v1/messages` spec.
fn convert_messages_to_anthropic(
    messages: Vec<Message>,
    image_index_base: usize,
) -> (Option<String>, Vec<serde_json::Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut raw: Vec<serde_json::Value> = Vec::new();
    let mut img_idx = image_index_base;

    for msg in messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(t) = extract_text(&msg.content) {
                    system_parts.push(t);
                }
            }
            "user" => {
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                content_parts.push(serde_json::json!({"type":"text","text": text}));
                            }
                        }
                        ContentBlock::Image { mime_type, data } => {
                            // Harness image placeholder — let model call `read_image`.
                            let placeholder = format!(
                                "[Image #{}: {}, ~{} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx,
                                mime_type,
                                data.len(),
                                img_idx
                            );
                            content_parts
                                .push(serde_json::json!({"type":"text","text": placeholder}));
                            img_idx += 1;
                        }
                        ContentBlock::ImageRef {
                            mime_type,
                            bytes_len,
                            ..
                        } => {
                            // A-2 L0：外置图片仅持索引，占位符用 bytes_len 显示。
                            let placeholder = format!(
                                "[Image #{}: {}, ~{bytes_len} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx, mime_type, img_idx
                            );
                            content_parts
                                .push(serde_json::json!({"type":"text","text": placeholder}));
                            img_idx += 1;
                        }
                        ContentBlock::ToolResult { .. } => {}
                        _ => {}
                    }
                }
                if content_parts.is_empty() {
                    content_parts.push(serde_json::json!({"type":"text","text":""}));
                }
                raw.push(serde_json::json!({"role":"user","content": content_parts}));
            }
            "assistant" => {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(serde_json::json!({"type":"text","text": text}));
                        }
                        ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                            // Preserve reasoning as Anthropic `thinking` block for
                            // round-trip fidelity when `thinking` is enabled.
                            // Strict endpoints safely ignore unknown block types;
                            // permissive ones (ZCode) accept it. Fallback to text
                            // would also work, but `thinking` keeps stream
                            // parity with `thinking_delta` events.
                            parts
                                .push(serde_json::json!({"type":"thinking","thinking": reasoning}));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            parts.push(serde_json::json!({"type":"tool_use","id": id, "name": name, "input": input}));
                        }
                        ContentBlock::ResponseOutputItem { .. } => {}
                        ContentBlock::WebSearchCall { .. } => {}
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    raw.push(serde_json::json!({"role":"assistant","content": parts}));
                }
            }
            "tool" => {
                // Each harness `tool` message → Anthropic `user` with `tool_result` block(s).
                let mut tr_parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } => {
                            let content_str = result.render_xml_envelope();
                            let is_error = !result.is_success();
                            tr_parts.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": content_str,
                                "is_error": is_error
                            }));
                            for img in &result.images {
                                tr_parts.push(serde_json::json!({
                                    "type": "image",
                                    "source": {"type":"base64","media_type": img.mime_type, "data": img.data}
                                }));
                            }
                        }
                        ContentBlock::Image { mime_type, data } => {
                            tr_parts.push(serde_json::json!({
                                "type": "image",
                                "source": {"type":"base64","media_type": mime_type, "data": data}
                            }));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => {
                            // A-2 L0：按需读盘后走同一 base64 source 路径。
                            match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                                Ok(data) => tr_parts.push(serde_json::json!({
                                    "type": "image",
                                    "source": {"type":"base64","media_type": mime_type, "data": data}
                                })),
                                Err(e) => log::warn!(
                                    "[gate] image {sha256} load failed, dropped from anthropic request: {e}"
                                ),
                            }
                        }
                        _ => {}
                    }
                }
                if !tr_parts.is_empty() {
                    raw.push(serde_json::json!({"role":"user","content": tr_parts}));
                }
            }
            _ => {}
        }
    }

    // Merge consecutive same-role messages (Anthropic requires strict alternation).
    // Only merge tool_result carriers together or plain text together — keep a
    // preceding plain `user` ("hi") separate from subsequent `tool_result`
    // blocks so `consecutive_user_messages_are_merged` stays meaningful and
    // real tool-loop batches still coalesce.
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for msg in raw {
        if let Some(last) = merged.last_mut() {
            let last_role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let cur_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if last_role == cur_role {
                let last_is_tool = last
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false);
                let cur_is_tool = msg
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false);
                // Merge only when kinds match (both tool or both non-tool)
                if last_is_tool == cur_is_tool {
                    let last_content = last.get_mut("content").and_then(|v| v.as_array_mut());
                    let cur_content = msg.get("content").and_then(|v| v.as_array());
                    if let (Some(dst), Some(src)) = (last_content, cur_content) {
                        for p in src {
                            dst.push(p.clone());
                        }
                        continue;
                    }
                }
            }
        }
        merged.push(msg);
    }

    // Ensure the sequence starts with a user message: Anthropic forbids
    // leading assistant. If the first merged entry is assistant (can happen
    // when history was tool-heavy), drop it or prepend a no-op user.
    if merged
        .first()
        .and_then(|v| v.get("role"))
        .and_then(|v| v.as_str())
        == Some("assistant")
    {
        log::warn!("anthropic: dropping leading assistant block to satisfy user-first constraint");
        merged.remove(0);
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n"))
    };
    (system, merged)
}

fn convert_tools(tools: Option<Vec<ToolDef>>) -> Option<Vec<serde_json::Value>> {
    tools.map(|tds| {
        tds.into_iter()
            .map(|td| {
                serde_json::json!({
                    "name": td.function.name,
                    "description": td.function.description,
                    "input_schema": td.function.parameters,
                })
            })
            .collect()
    })
}

// ── SSE streaming state ──

struct ToolState {
    id: String,
    name: String,
    buffer: String,
}

enum FrameAction {
    Continue,
    Done,
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
fn handle_anthropic_frame(
    data_str: &str,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_states: &mut HashMap<usize, ToolState>,
    usage_info: &mut Option<UsageInfo>,
    prompt_tokens_acc: &mut u32,
    stop_reason: &mut Option<String>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<FrameAction> {
    if data_str.is_empty() {
        return Ok(FrameAction::Continue);
    }
    // Anthropic error envelope without `type` (e.g. `{type:"error",error:{...}}`)
    let ev: serde_json::Value = match serde_json::from_str(data_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "Anthropic SSE: deserialize fail: {} — data: {}",
                e,
                data_str
            );
            return Ok(FrameAction::Continue);
        }
    };
    let typ = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "message_start" => {
            if let Some(msg) = ev.get("message")
                && let Some(usage) = msg.get("usage")
            {
                let pt = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                *prompt_tokens_acc = pt;
                // Anthropic 2024 prompt caching: ZCode bun 已透传
                // cache_read_input_tokens / cache_creation_input_tokens
                // （Anthropic usage 解析惯例），
                // 开启 cache_control 后命中可达 70%+，GLM 直连/未开启时为 0。
                let cached = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let created = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                // created 不计入 hit，单算写入；兼容 bun 将两者或计入 cache_read 的旧逻辑
                let hit = cached;
                let reported = cached != 0
                    || created != 0
                    || usage.get("cache_read_input_tokens").is_some()
                    || usage.get("cache_creation_input_tokens").is_some();
                if pt != 0 || reported {
                    let u = UsageInfo {
                        prompt_tokens: pt,
                        completion_tokens: 0,
                        total_tokens: pt,
                        prompt_cache_hit_tokens: hit,
                        prompt_cache_miss_tokens: pt.saturating_sub(hit),
                        reasoning_tokens: 0,
                        cache_usage_reported: Some(reported),
                    };
                    *usage_info = Some(u.clone());
                    on_event(StreamEvent::UsageUpdate(u));
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_start" => {
            let idx = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block) = ev.get("content_block") {
                let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if bt == "tool_use" {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    tool_states.insert(
                        idx,
                        ToolState {
                            id: id.clone(),
                            name: name.clone(),
                            buffer: String::new(),
                        },
                    );
                    on_event(StreamEvent::ToolCallProgress {
                        index: idx,
                        id,
                        name,
                        args_so_far: String::new(),
                    });
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_delta" => {
            let idx = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = ev.get("delta") {
                let d_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match d_type {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            text_buf.push_str(t);
                            on_event(StreamEvent::ContentDelta(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        // Anthropic streams `thinking_delta.thinking`
                        if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                reasoning_buf.push_str(t);
                                on_event(StreamEvent::ReasoningDelta(t.to_string()));
                            }
                        } else if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                            // Some proxies (zcode-proxy anthropic→openai layer) forward as text
                            if !t.is_empty() {
                                reasoning_buf.push_str(t);
                                on_event(StreamEvent::ReasoningDelta(t.to_string()));
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = delta.get("partial_json").and_then(|v| v.as_str())
                            && !pj.is_empty()
                        {
                            let entry = tool_states.entry(idx).or_insert(ToolState {
                                id: String::new(),
                                name: String::new(),
                                buffer: String::new(),
                            });
                            entry.buffer.push_str(pj);
                            on_event(StreamEvent::ToolCallProgress {
                                index: idx,
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                                args_so_far: entry.buffer.clone(),
                            });
                        }
                    }
                    "signature_delta" => {
                        // Anthropic thinking signature — ignore but keep flow.
                    }
                    _ => {}
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_stop" => Ok(FrameAction::Continue),
        "message_delta" => {
            if let Some(delta) = ev.get("delta")
                && let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str())
                && !sr.is_empty()
                && sr != "null"
            {
                *stop_reason = Some(map_anthropic_stop_reason(sr));
            }
            if let Some(usage) = ev.get("usage") {
                let ot = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let pt = *prompt_tokens_acc;
                // delta 阶段也可能携带最终 cache 计费（部分上游延迟上报）
                let delta_cached = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let hit = delta_cached.unwrap_or_else(|| {
                    usage_info
                        .as_ref()
                        .map(|u| u.prompt_cache_hit_tokens)
                        .unwrap_or(0)
                });
                let reported = delta_cached.is_some()
                    || usage_info
                        .as_ref()
                        .and_then(|u| u.cache_usage_reported)
                        .unwrap_or(false);
                let u = UsageInfo {
                    prompt_tokens: pt,
                    completion_tokens: ot,
                    total_tokens: pt + ot,
                    prompt_cache_hit_tokens: hit,
                    prompt_cache_miss_tokens: pt.saturating_sub(hit),
                    reasoning_tokens: 0,
                    cache_usage_reported: Some(reported),
                };
                *usage_info = Some(u.clone());
                on_event(StreamEvent::UsageUpdate(u));
            }
            Ok(FrameAction::Continue)
        }
        "message_stop" => Ok(FrameAction::Done),
        "ping" => Ok(FrameAction::Continue),
        "error" => {
            let msg = ev
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| ev.get("message").and_then(|m| m.as_str()))
                .unwrap_or("anthropic error");
            on_event(StreamEvent::Error(msg.to_string()));
            Err(anyhow::anyhow!("anthropic error: {}", msg))
        }
        "" => {
            // Some proxies emit `data: [DONE]` style termination even for anthropic
            if data_str == "[DONE]" {
                return Ok(FrameAction::Done);
            }
            Ok(FrameAction::Continue)
        }
        _ => Ok(FrameAction::Continue),
    }
}

fn stream_sse_anthropic(
    resp: reqwest::Response,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut decoder = SseDecoder::new();
    let mut stream = resp.bytes_stream();
    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_states: HashMap<usize, ToolState> = HashMap::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut prompt_tokens_acc: u32 = 0;
    let mut stop_reason: Option<String> = None;
    let mut trace = SseTrace::from_env();
    let callback = on_event as *mut dyn FnMut(StreamEvent);
    // SAFETY: trace wrapper borrows callback for duration of this fn
    let mut traced = |event: StreamEvent| {
        trace.record(&event);
        unsafe { (*callback)(event) };
    };
    let mut done_reached = false;
    loop {
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_anthropic_frame(
                &data_str,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_states,
                &mut usage_info,
                &mut prompt_tokens_acc,
                &mut stop_reason,
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
            Ok(None) => break,
            Err(_elapsed) => continue,
        };
        decoder.push(&chunk);
    }
    if !done_reached && decoder.has_pending() {
        decoder.push(b"\n\n");
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            let _ = handle_anthropic_frame(
                &data_str,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_states,
                &mut usage_info,
                &mut prompt_tokens_acc,
                &mut stop_reason,
                &mut traced,
            );
        }
    }
    if !done_reached && stop_reason.is_none() {
        if reasoning_buf.is_empty() && text_buf.is_empty() && tool_states.is_empty() {
            return Err(anyhow::Error::new(EmptyStreamEof));
        }
        log::warn!(
            "Anthropic SSE: upstream closed stream without message_stop — partial output kept"
        );
    }
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !reasoning_buf.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning_buf,
        });
    }
    if !text_buf.is_empty() {
        blocks.push(ContentBlock::text(&text_buf));
    }
    let mut sorted: Vec<(usize, String, String, String)> = tool_states
        .into_iter()
        .map(|(idx, s)| (idx, s.id, s.name, s.buffer))
        .collect();
    sorted.sort_by_key(|(idx, _, _, _)| *idx);
    for (_idx, id, name, args_json) in sorted {
        let input: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
        // Anthropic tool `input` may be null when model bails mid-json; pass empty object
        let input = if input.is_null() {
            serde_json::json!({})
        } else {
            input
        };
        let id = if id.is_empty() {
            format!("toolu_{}", uuid_simple())
        } else {
            id
        };
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

fn uuid_simple() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

// ── Public entry ──

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_anthropic(
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
    let (messages, image_index_base) = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        (messages, 0)
    };
    let (system, api_messages) = convert_messages_to_anthropic(messages, image_index_base);
    let anth_tools = convert_tools(tools);

    // max_tokens: Anthropic requires >=1 and > budget when thinking enabled.
    let mut max_toks = if max_tokens == 0 { 8192 } else { max_tokens };
    // Pre-normalize effort (QAQ always forces thinking at least `low`).
    let effort_norm = crate::types::normalize_reasoning_effort(effort.as_deref());

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("messages".into(), serde_json::Value::Array(api_messages));
    body_map.insert("stream".into(), serde_json::json!(true));
    body_map.insert("max_tokens".into(), serde_json::json!(max_toks));
    if let Some(sys) = system
        && !sys.is_empty()
    {
        body_map.insert("system".into(), serde_json::json!(sys));
    }
    if let Some(t) = anth_tools
        && !t.is_empty()
    {
        body_map.insert("tools".into(), serde_json::Value::Array(t));
    }
    // Thinking budget: only when provider explicitly supports it.
    if provider.supports_thinking
        && let Some(e) = effort_norm.as_deref()
    {
        // 大上下文模型（EndpointSpec::thinking_budget_large，如 zcode
        // GLM-5.3 1M 窗口）放宽到 16k-96k；默认档保持 1k-16k。
        let budget: u32 = if provider.thinking_budget_large {
            match e {
                "low" => 16384,
                "medium" => 32768,
                "high" => 65536,
                "xhigh" => 81920,
                "max" => 96000,
                _ => 32768,
            }
        } else {
            match e {
                "low" => 1024,
                "medium" => 2048,
                "high" => 4096,
                "xhigh" => 8192,
                "max" => 16384,
                _ => 4096,
            }
        };
        max_toks = max_toks.max(budget + 1024);
        body_map.insert("max_tokens".into(), serde_json::json!(max_toks));
        body_map.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled","budget_tokens": budget}),
        );
    }
    // If effort is None but thinking is supported, do not force thinking;
    // let provider default (CLAUDE streams thinking only when asked).

    let body = serde_json::Value::Object(body_map);
    let url = build_anthropic_url(&provider.base_url, provider.anthropic_path.as_deref());

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        match block_on(async {
            let mut req = provider
                .apply_opencode_headers(crate::shared_http_client().post(&url))
                .header("Content-Type", "application/json")
                .header("x-api-key", &provider.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("Authorization", format!("Bearer {}", provider.api_key));
            // 透传 harness session 到 bun 网关按会话看 1M/131K 占比
            // bun: getSessionId() 读 X-Session-Id|X-Task-Id|metadata.session_id，默认 default
            // 仅网关侧落地 usage_logs.session_id + dashboard bySession，不回传上游 z.ai
            if let Some(ref sid) = user_id
                && !sid.is_empty()
            {
                req = req.header("X-Session-Id", sid.clone());
                // 兼容旧 zcode 前缀头
                req = req.header("X-Task-Id", sid.clone());
            }
            req.json(&body).send().await
        }) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    match stream_sse_anthropic(resp, cancel, on_event) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.downcast_ref::<EmptyStreamEof>().is_some() => {
                            if attempt >= MAX_RETRIES {
                                let msg = "upstream closed stream before any content".to_string();
                                on_event(StreamEvent::Error(msg.clone()));
                                return Err(anyhow::anyhow!("{}", msg));
                            }
                            let delay = backoff_delay(attempt);
                            on_event(StreamEvent::Retrying {
                                attempt,
                                max_retries: MAX_RETRIES,
                                delay_secs: delay.as_secs(),
                                error: "stream closed early (no content)".into(),
                            });
                            if sleep_with_cancel(delay, cancel) {
                                return Err(anyhow::anyhow!("cancelled by user"));
                            }
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                let text = block_on(resp.text()).unwrap_or_default();
                let code_desc = http_error_description(status);
                if attempt >= MAX_RETRIES || !is_retryable(status) {
                    let msg = format!("Anthropic API HTTP {} ({})", status, code_desc);
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

pub fn chat_sync_anthropic(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let (messages, image_index_base) = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        (messages, 0)
    };
    let (system, api_messages) = convert_messages_to_anthropic(messages, image_index_base);
    let mut max_toks = if max_tokens == 0 { 4096 } else { max_tokens };
    if provider.supports_thinking {
        // Keep a minimal budget headroom for sync thinking paths.
        max_toks = max_toks.max(4096);
    }
    let mut body = serde_json::json!({
        "model": model,
        "messages": api_messages,
        "max_tokens": max_toks,
        "stream": false,
    });
    if let Some(sys) = system
        && !sys.is_empty()
    {
        body["system"] = serde_json::json!(sys);
    }
    let url = build_anthropic_url(&provider.base_url, provider.anthropic_path.as_deref());
    let resp = block_on(
        provider
            .apply_opencode_headers(crate::shared_http_client().post(&url))
            .header("Content-Type", "application/json")
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Authorization", format!("Bearer {}", provider.api_key))
            .json(&body)
            .send(),
    )
    .map_err(|e| format!("compact request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = block_on(resp.text()).unwrap_or_default();
        return Err(format!(
            "anthropic HTTP {}: {}",
            status,
            safe_provider_error_body(&text, &provider.api_key)
        ));
    }
    let json: serde_json::Value =
        block_on(resp.json()).map_err(|e| format!("compact parse failed: {e}"))?;
    // Anthropic non-stream: `content: [{type:"text",text:"..."}]`
    if let Some(arr) = json.get("content").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|v| v.as_str()) == Some("text")
                && let Some(t) = block.get("text").and_then(|v| v.as_str())
            {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    // Fallback: direct string field
    json.get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("compact: no content in anthropic response: {}", json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::ContentBlock;

    #[test]
    fn system_is_top_level_not_message() {
        let msgs = vec![Message::system("you are helpful"), Message::user("hi")];
        let (system, api) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("you are helpful"));
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "user");
        // system must NOT appear in messages array
        assert!(!api.iter().any(|m| m["role"] == "system"));
    }

    #[test]
    fn multiple_system_joined_with_newline() {
        let msgs = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::user("hi"),
        ];
        let (system, _) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("base\ncatalog"));
    }

    #[test]
    fn developer_also_goes_to_system() {
        let msgs = vec![Message::developer("injected skills"), Message::user("hi")];
        let (system, _) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("injected skills"));
    }

    #[test]
    fn tool_result_mapped_to_user_tool_result_block() {
        let tool_msg = Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                result: qaqh_types::ToolResult::ok("42"),
            }],
        };
        // need provider; but conversion doesn't need provider directly
        let (system, api) = convert_messages_to_anthropic(
            vec![
                Message::user("call tool"),
                Message {
                    msg_id: None,
                    role: "assistant".into(),
                    name: None,
                    content: vec![ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({"cmd":"echo 42"}),
                    }],
                },
                tool_msg,
            ],
            0,
        );
        assert_eq!(system, None);
        // messages: user, assistant, user(tool_result)
        assert_eq!(api.len(), 3);
        assert_eq!(api[2]["role"], "user");
        let content = api[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn consecutive_user_messages_are_merged() {
        let msgs = vec![
            Message::user("hi"),
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    result: qaqh_types::ToolResult::ok("1"),
                }],
            },
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "b".into(),
                    result: qaqh_types::ToolResult::ok("2"),
                }],
            },
        ];
        let (_, api) = convert_messages_to_anthropic(msgs, 0);
        // user, then merged tool results as single user with 2 blocks
        assert_eq!(api.len(), 2);
        assert_eq!(api[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn anthropic_stream_text_delta_emits_content_delta() {
        let mut text_buf = String::new();
        let mut reasoning = String::new();
        let mut tools = HashMap::new();
        let mut usage = None;
        let mut pt = 0;
        let mut sr = None;
        let mut events = Vec::new();
        let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}});
        let s = delta.to_string();
        let action = handle_anthropic_frame(
            &s,
            &mut text_buf,
            &mut reasoning,
            &mut tools,
            &mut usage,
            &mut pt,
            &mut sr,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(matches!(action, FrameAction::Continue));
        assert_eq!(text_buf, "Hello");
        assert!(matches!(&events[0], StreamEvent::ContentDelta(d) if d=="Hello"));
    }

    #[test]
    fn anthropic_url_builder() {
        assert_eq!(
            build_anthropic_url("https://api.anthropic.com", None),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_anthropic_url(
                "https://open.bigmodel.cn",
                Some("/api/anthropic/v1/messages")
            ),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
        assert_eq!(
            build_anthropic_url(
                "https://open.bigmodel.cn/",
                Some("/api/anthropic/v1/messages")
            ),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
    }
}
