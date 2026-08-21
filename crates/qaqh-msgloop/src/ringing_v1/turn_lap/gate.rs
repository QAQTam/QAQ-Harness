//! Gate lap 阶段：provider 构建 + chat_stream + 流式聚合/错误归一 (knife-7 S2-3)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化以便
//! 后续 `parse`/`admit`/`backfill` 共享 terminal helpers。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use qaqh_types::UsageInfo;

use crate::ringing_v1::types::{Emitter, LoopPhase, Outcome, RingContext};
use crate::util;

// ── 流式节流常量（原 engine_turn.rs） ──

pub(crate) const CHECKPOINT_TOKEN_INTERVAL: u32 = 64;
pub(crate) const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
pub(crate) const USAGE_EMIT_INTERVAL: Duration = Duration::from_secs(1);

// ── Gate 请求聚合结果 ──

/// 一次 gate 请求的聚合结果（knife-7 S2：从 run_lap 收敛出的贯穿状态）。
///
/// `content`/`reasoning`/`tool_calls_raw`/`response_output_items` 供 parse 消费；
/// `active_stream_block`/`timeline_tools_open` 供后续 seal 消费；
/// `had_error`/`done_seen`/`gate_error`/`request_error` 供错误归一；
/// `current_request_usage` 供 token calibration；`last_usage` 回传给调用方。
pub(crate) struct GateRequestResult {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls_raw: serde_json::Value,
    pub(crate) response_output_items: Vec<qaqh_types::ContentBlock>,
    pub(crate) active_stream_block: Option<(qaqh_domain::TimelineBlockKind, String)>,
    pub(crate) timeline_tools_open: HashSet<String>,
    pub(crate) had_error: bool,
    pub(crate) done_seen: bool,
    pub(crate) gate_error: Option<String>,
    pub(crate) current_request_usage: Option<UsageInfo>,
    pub(crate) request_error: Option<String>,
    pub(crate) last_usage: Option<UsageInfo>,
}

// ── stream / block 辅助 ──

pub(crate) fn maybe_emit_block_checkpoint(
    emitter: &dyn Emitter,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: qaqh_domain::RoundDeltaKind,
    round_text: &str,
    block_text: &str,
    tokens_since_checkpoint: &mut u32,
    last_checkpoint_at: &mut Instant,
) {
    *tokens_since_checkpoint += 1;
    if *tokens_since_checkpoint < CHECKPOINT_TOKEN_INTERVAL
        && last_checkpoint_at.elapsed() < CHECKPOINT_INTERVAL
    {
        return;
    }
    *tokens_since_checkpoint = 0;
    *last_checkpoint_at = Instant::now();
    emitter.emit_domain(qaqh_domain::DomainEvent::Conversation(
        qaqh_domain::ConversationEvent::BlockCheckpoint {
            turn_id: turn_id.to_string(),
            round_num,
            kind,
            text: round_text.to_string(),
            char_count: round_text.chars().count() as u32,
        },
    ));
    emitter.emit_timeline(qaqh_domain::TimelineIntent::BlockCheckpoint {
        turn_id: turn_id.to_string(),
        round_num,
        block_id: block_id.to_string(),
        text: block_text.to_string(),
    });
}

pub(crate) fn append_stream_block_delta(
    block_id: &str,
    delta: &str,
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
) {
    if stream_block_id.as_deref() != Some(block_id) {
        *stream_block_id = Some(block_id.to_string());
        stream_block_text.clear();
    }
    stream_block_text.push_str(delta);
}

pub(crate) fn reset_stream_block_checkpoint(
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
) {
    *stream_block_id = None;
    stream_block_text.clear();
}

pub(crate) fn emit_stream_block_checkpoint(
    emitter: &dyn Emitter,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: qaqh_domain::RoundDeltaKind,
    round_text: &str,
    delta: &str,
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
    tokens_since_checkpoint: &mut u32,
    last_checkpoint_at: &mut Instant,
) {
    append_stream_block_delta(block_id, delta, stream_block_id, stream_block_text);
    // BUG-015 run_lap 级不变量：block 文本必须为 round 文本的局部后缀，而非整轮重复前缀。
    debug_assert!(
        stream_block_text.len() <= round_text.len()
            && round_text.ends_with(stream_block_text.as_str()),
        "BUG-015: block checkpoint must be local suffix: block {} len {} round len {}",
        block_id,
        stream_block_text.len(),
        round_text.len()
    );
    debug_assert!(
        block_id.starts_with(&format!("round-{round_num}:")),
        "block_id must be round-scoped: {}",
        block_id
    );
    maybe_emit_block_checkpoint(
        emitter,
        turn_id,
        round_num,
        block_id,
        kind,
        round_text,
        stream_block_text,
        tokens_since_checkpoint,
        last_checkpoint_at,
    );
}

/// Run-lap 级 BUG-015 不变量断言（debug only）：验证一次 gate 结果的块级隔离。
pub(crate) fn debug_assert_gate_invariants(result: &GateRequestResult, round_num: u32) {
    if cfg!(not(debug_assertions)) {
        return;
    }
    if let Some((_, block_id)) = &result.active_stream_block {
        debug_assert!(
            block_id.starts_with(&format!("round-{round_num}:")),
            "active_stream_block not round-scoped: {}",
            block_id
        );
        debug_assert!(
            !block_id.starts_with("tool:"),
            "active stream block must not be a tool block: {}",
            block_id
        );
    }
    if let Some(arr) = result.tool_calls_raw.as_array() {
        let parsed_ids: std::collections::HashSet<&str> =
            arr.iter().filter_map(|v| v.get("id").and_then(|x| x.as_str())).collect();
        for id in &result.timeline_tools_open {
            debug_assert!(
                parsed_ids.contains(id.as_str()) || result.done_seen,
                "timeline_tools_open {} not in parsed tool_calls_raw {:?}",
                id,
                parsed_ids
            );
        }
    }
    if result.done_seen {
        debug_assert!(
            !result.content.contains('\u{0}'),
            "content contains null after Done reconciliation"
        );
    }
}

// ── terminal / timeline 辅助（也被 run_lap / handle_* 复用） ──

pub(crate) fn abort_running_turn(
    ctx: &mut RingContext,
    turn_id: String,
    usage: Option<UsageInfo>,
) -> Outcome {
    ctx.agent.msg.remove_last_step_if_incomplete();
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    Outcome::TurnAborted { turn_id, usage }
}

pub(crate) fn seal_timeline_terminal_round(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active_stream_block: Option<&(qaqh_domain::TimelineBlockKind, String)>,
    tool_ids: &HashSet<String>,
    state: qaqh_domain::TimelineTurnState,
    failure: Option<qaqh_domain::TimelineFailure>,
) {
    if let Some((_, block_id)) = active_stream_block {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: block_id.clone(),
            });
    }
    for tool_id in tool_ids {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_id}"),
            });
    }
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
            turn_id: turn_id.to_string(),
            round_num,
            is_final: true,
        });
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
            turn_id: turn_id.to_string(),
            state,
            failure,
        });
    // 标题生成挂点：仅 Completed 终态触发（取消/失败不生成）；幂等由
    // engine_title 内部冻结守卫保证（首 turn 后一次）。
    if state == qaqh_domain::TimelineTurnState::Completed {
        crate::ringing_v1::engine_title::maybe_generate_title(ctx);
    }
}

pub(crate) fn seal_active_stream_block(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
) {
    if let Some((_, block_id)) = active.take() {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id,
            });
    }
}

pub(crate) fn ensure_stream_block(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
    segment: &mut u32,
    kind: qaqh_domain::TimelineBlockKind,
) -> String {
    if let Some((active_kind, block_id)) = active {
        if *active_kind == kind {
            return block_id.clone();
        }
    }
    seal_active_stream_block(ctx, turn_id, round_num, active);
    let label = match kind {
        qaqh_domain::TimelineBlockKind::Reasoning => "reasoning",
        qaqh_domain::TimelineBlockKind::Text => "text",
        _ => "stream",
    };
    let block_id = format!("round-{round_num}:{label}:{}", *segment);
    *segment = (*segment).saturating_add(1);
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.clone(),
            kind,
            tool: None,
        });
    *active = Some((kind, block_id.clone()));
    block_id
}

// ── 主 gate lap：chat_stream + 流式事件聚合 ──

/// Run one gate lap: stream one model request and aggregate streamed
/// state into a [`GateRequestResult`] (knife-7 S2; extracted from run_lap).
///
/// Extracted verbatim — behavior identical. Error normalisation, token
/// calibration and parse stay in `run_lap`, consuming this result.
pub(crate) fn gate_request(
    ctx: &mut RingContext,
    provider: &qaqh_gate::ProviderConfig,
    messages: Vec<qaqh_types::Message>,
    tools: Option<Vec<qaqh_types::ToolDef>>,
    turn_id: &str,
    round_num: u32,
    mut last_usage: Option<UsageInfo>,
) -> GateRequestResult {
    let mut content = String::new();
    let mut reasoning = String::new();
    // Opaque Responses output items are persisted for protocol replay
    // only. They never enter timeline/UI projections.
    let mut response_output_items: Vec<qaqh_types::ContentBlock> = Vec::new();
    // A1：block_checkpoint 节流（delta 次数 + 时间窗，先到为准）。
    let mut checkpoint_tokens: u32 = 0;
    let mut last_checkpoint_at = Instant::now();
    // A3：usage 节流状态（最后发射时刻 + 已发 total，终值补发依据）。
    let mut last_usage_emit_at: Option<Instant> = None;
    let mut last_emitted_usage_total: u32 = 0;
    let mut tool_calls_raw = serde_json::Value::Null;
    let mut active_stream_block: Option<(qaqh_domain::TimelineBlockKind, String)> = None;
    let mut stream_block_id: Option<String> = None;
    let mut stream_block_text = String::new();
    let mut timeline_segment = 0u32;
    let mut timeline_tools_open = HashSet::new();
    let mut had_error = false;
    // 已收到 Done（内容完整流式输出）标记：gate 尾部错误不再否定完成。
    let mut done_seen = false;
    let mut gate_error = None;
    let mut current_request_usage: Option<UsageInfo> = None;

    *ctx.phase = LoopPhase::GateRunning;
    let cancel_arc = ctx.cancel.arc();

    // ── SSE Gate Request ──
    log::info!(
        "[TURN] run_lap turn_id={} round_num={} calling chat_stream",
        turn_id,
        round_num
    );
    let result = qaqh_gate::chat_stream(
        provider,
        messages,
        tools,
        ctx.agent.config.max_tokens,
        Some(ctx.agent.config.reasoning_effort.clone()),
        Some(ctx.agent.session.seed.clone()),
        Some(&cancel_arc),
        &mut |event| match event {
            qaqh_gate::StreamEvent::ContentDelta(d) => {
                if ctx.cancel.is_set() {
                    return;
                }
                content.push_str(&d);
                let block_id = ensure_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                    &mut timeline_segment,
                    qaqh_domain::TimelineBlockKind::Text,
                );
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TextDelta {
                        turn_id: turn_id.to_string(),
                        round_num,
                        block_id: block_id.clone(),
                        delta: d.clone(),
                    });
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::RoundDelta {
                            turn_id: turn_id.to_string(),
                            round_num,
                            kind: qaqh_domain::RoundDeltaKind::Answering,
                            delta: d.clone(),
                        },
                    ));
                // A1：Conversation 发整轮值，timeline 发当前 block 局部值。
                emit_stream_block_checkpoint(
                    ctx.emitter,
                    turn_id,
                    round_num,
                    &block_id,
                    qaqh_domain::RoundDeltaKind::Answering,
                    &content,
                    &d,
                    &mut stream_block_id,
                    &mut stream_block_text,
                    &mut checkpoint_tokens,
                    &mut last_checkpoint_at,
                );
            }
            qaqh_gate::StreamEvent::ReasoningDelta(r) => {
                if ctx.cancel.is_set() {
                    return;
                }
                reasoning.push_str(&r);
                let block_id = ensure_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                    &mut timeline_segment,
                    qaqh_domain::TimelineBlockKind::Reasoning,
                );
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TextDelta {
                        turn_id: turn_id.to_string(),
                        round_num,
                        block_id: block_id.clone(),
                        delta: r.clone(),
                    });
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::RoundDelta {
                            turn_id: turn_id.to_string(),
                            round_num,
                            kind: qaqh_domain::RoundDeltaKind::Thinking,
                            delta: r.clone(),
                        },
                    ));
                // A1：Conversation 发整轮值，timeline 发当前 block 局部值。
                emit_stream_block_checkpoint(
                    ctx.emitter,
                    turn_id,
                    round_num,
                    &block_id,
                    qaqh_domain::RoundDeltaKind::Thinking,
                    &reasoning,
                    &r,
                    &mut stream_block_id,
                    &mut stream_block_text,
                    &mut checkpoint_tokens,
                    &mut last_checkpoint_at,
                );
            }
            qaqh_gate::StreamEvent::Done {
                raw_message, usage, ..
            } => {
                done_seen = true;
                if let Some(ref u) = usage {
                    ctx.agent.session.record_usage(u);
                    if !ctx.agent.ephemeral {
                        qaqh_session::SessionManager::global().persist_usage(
                            &ctx.agent.session.seed,
                            ctx.agent.session.usage_totals.clone(),
                            ctx.agent.session.last_usage.clone(),
                            ctx.agent.session.usage_requests,
                            ctx.agent.session.cache_reported_requests,
                        );
                    }
                    util::record_token_usage(u, &ctx.agent.config.model);
                    last_usage = usage.clone();
                    current_request_usage = usage.clone();
                }
                // A3：终值必发——节流窗口可能吞掉最后一条流式值，此处补发
                // 请求权威终值（replaceable 覆盖；与 done 前的 record_usage 一致）。
                if let Some(final_usage) = current_request_usage.clone() {
                    if final_usage.total_tokens != last_emitted_usage_total {
                        last_emitted_usage_total = final_usage.total_tokens;
                        ctx.emitter
                            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                                qaqh_domain::ConversationEvent::UsageUpdated {
                                    turn_id: turn_id.to_string(),
                                    round_num,
                                    usage: final_usage.clone(),
                                    context_limit: ctx.agent.config.context_limit,
                                    model: ctx.agent.config.model.clone(),
                                },
                            ));
                    }
                }
                content.clear();
                reasoning.clear();
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                for block in &raw_message.content {
                    match block {
                        qaqh_types::ContentBlock::Text { text } => content.push_str(text),
                        qaqh_types::ContentBlock::Reasoning { reasoning: r } => {
                            reasoning.push_str(r)
                        }
                        qaqh_types::ContentBlock::ToolUse { id, name, input } => {
                            blocks.push(serde_json::json!({
                                "id": id, "name": name, "arguments": input.to_string(),
                            }));
                        }
                        qaqh_types::ContentBlock::ResponseOutputItem { .. } => {
                            response_output_items.push(block.clone());
                        }
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    tool_calls_raw = serde_json::Value::Array(blocks);
                }
            }
            qaqh_gate::StreamEvent::ToolCallProgress {
                index: _,
                id,
                name,
                args_so_far,
            } => {
                let block_id = format!("tool:{id}");
                seal_active_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                );
                reset_stream_block_checkpoint(
                    &mut stream_block_id,
                    &mut stream_block_text,
                );
                if timeline_tools_open.insert(id.clone()) {
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: block_id.clone(),
                            kind: qaqh_domain::TimelineBlockKind::Tool,
                            tool: Some(qaqh_domain::TimelineTool {
                                tool_call_id: id.clone(),
                                name: name.clone(),
                                state: qaqh_domain::TimelineToolState::Prepared,
                                summary: None,
                                args_json: Some(args_so_far.clone()),
                                output: None,
                                diff: None,
                                progress: String::new(),
                                failure: None,
                                permission: None,
                            }),
                        });
                }
                // Ringing 双发：ToolCallPrepared（replaceable 预览，可被 ToolStarted 覆盖）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                    qaqh_domain::ToolEvent::ToolCallPrepared {
                        tool_call_id: id.clone(),
                        turn_id: turn_id.to_string(),
                        round_num,
                        name: name.clone(),
                        args_so_far: args_so_far.clone(),
                    },
                ));
            }
            qaqh_gate::StreamEvent::WebSearchStatus(status) => {
                // Ringing 双发：ProviderToolStatus（replaceable，按 call_id 合并）
                let provider_state = match status.as_str() {
                    "completed" | "done" => qaqh_domain::ProviderToolState::Completed,
                    "searching" | "running" | "in_progress" => {
                        qaqh_domain::ProviderToolState::Searching
                    }
                    _ => qaqh_domain::ProviderToolState::InProgress,
                };
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ProviderToolStatus {
                            turn_id: turn_id.to_string(),
                            round_num,
                            call_id: format!("ws-{turn_id}-{round_num}"),
                            tool_kind: "web_search".into(),
                            state: provider_state,
                        },
                    ));
            }
            qaqh_gate::StreamEvent::UsageUpdate(u) => {
                last_usage = Some(u.clone());
                current_request_usage = Some(u.clone());
                ctx.agent.session.tokens =
                    ctx.agent.session.tokens.max(u.total_tokens as u64);
                // A3：节流 ~1s（replaceable 覆盖显示）；终值由 Done 分支补发。
                let due = last_usage_emit_at
                    .map_or(true, |at| at.elapsed() >= USAGE_EMIT_INTERVAL);
                if due {
                    last_usage_emit_at = Some(Instant::now());
                    last_emitted_usage_total = u.total_tokens;
                    ctx.emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::UsageUpdated {
                                turn_id: turn_id.to_string(),
                                round_num,
                                usage: u.clone(),
                                context_limit: ctx.agent.config.context_limit,
                                model: ctx.agent.config.model.clone(),
                            },
                        ));
                }
            }
            qaqh_gate::StreamEvent::Retrying {
                attempt,
                max_retries,
                delay_secs,
                error,
            } => {
                // Ringing 双发：ProviderRetrying（重试可见性）
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ProviderRetrying {
                            turn_id: turn_id.to_string(),
                            round_num,
                            attempt,
                            max_retries,
                            delay_secs,
                            error_message: error,
                        },
                    ));
            }
            qaqh_gate::StreamEvent::Error(msg) => {
                log::error!(
                    "[TURN] gate error turn_id={turn_id} round_num={round_num}: {msg}"
                );
                gate_error = Some(msg);
                had_error = true;
            }
        },
    );
    let out = GateRequestResult {
        content,
        reasoning,
        tool_calls_raw,
        response_output_items,
        active_stream_block,
        timeline_tools_open,
        had_error,
        done_seen,
        gate_error,
        current_request_usage,
        request_error: result.err().map(|e| e.to_string()),
        last_usage,
    };
    debug_assert_gate_invariants(&out, round_num);
    out
}

// ── provider 构建（已落地，保持不变） ──

/// 依当前 config / endpoint 重建 provider（Responses 或 OpenAI 形态）。
///
/// 从 `run_lap` 原样抽出的 provider 构建逻辑；`ep`/`is_responses` 仅在
/// 此处使用，迁移后 `run_lap` 不再持有它们。
pub(crate) fn provider_for(ctx: &RingContext) -> qaqh_gate::ProviderConfig {
    let ep = qaqh_config::registry::find_endpoint(
        &ctx.agent.config.provider_id,
        &ctx.agent.config.endpoint,
    );
    let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
    if is_responses {
        let mut p = qaqh_gate::ProviderConfig::responses(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.responses_path.clone()),
        );
        if let Some(endpoint) = ep.as_ref() {
            p.responses_compat = qaqh_gate::ResponsesCompat {
                web_search: endpoint.responses_web_search,
                echo_web_search_call: endpoint.responses_echo_web_search_call,
                send_include: endpoint.responses_send_include,
                effort_max: endpoint.responses_effort_max.clone(),
                supports_user: endpoint.responses_supports_user,
                search_function_alias: endpoint.responses_search_function_alias.clone(),
                echo_reasoning_content: endpoint.responses_echo_reasoning_content,
            };
        }
        // Muse Spark 专项：物理前缀缓存 + 关明文回放 + 放宽档位至 xhigh
        if p.model.contains("muse-spark") {
            p.prompt_cache_key = Some(ctx.agent.session.seed.clone());
            p.responses_compat.echo_reasoning_content = false;
            p.responses_compat.effort_max = "xhigh".into();
            p.responses_compat.web_search = false;
            p.responses_compat.echo_web_search_call = false;
        }
        p
    } else {
        let mut p = qaqh_gate::ProviderConfig::openai(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.user_id_mode.clone()),
            ep.as_ref().and_then(|e| e.chat_path.clone()),
            ep.as_ref()
                .map(|e| e.thinking_mode.clone())
                .unwrap_or_default(),
            ep.as_ref()
                .map(|e| e.cache_field.clone())
                .unwrap_or_default(),
            ep.as_ref().map(|e| e.supports_thinking).unwrap_or(true),
            ep.as_ref().and_then(|e| e.do_sample),
        )
        .with_stateful(ep.as_ref().map(|e| e.stateful).unwrap_or(false))
        .with_stream_usage(ep.as_ref().map(|e| e.include_stream_usage).unwrap_or(false));
        if let Some(endpoint) = ep.as_ref() {
            p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
            p.tool_call_content_null = endpoint.tool_call_content_null;
            p.supports_reasoning_content = endpoint.supports_reasoning_content;
            p.require_provider_parameters = endpoint.require_provider_parameters;
        }
        p
    }
}
