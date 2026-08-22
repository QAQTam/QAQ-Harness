//! Parse lap 阶段：响应→assistant 入库 + RoundComplete + seal (knife-7 A2 step4)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use std::collections::HashSet;

use qaqh_message::Effect;
use qaqh_types::{ContentBlock, Message, ToolCall};

use crate::ringing_v1::types::RingContext;
use crate::util;

/// 一次 parse 阶段的聚合结果（knife-7 A2 step4：从 run_lap 收敛出的贯穿状态）。
///
/// `parsed`/`assistant_msg`/`effect` 供后续 admit/tools 消费；
/// `active_stream_block`/`timeline_tools_open` 通过 `&mut` 原地更新，调用方直接持有。
pub(crate) struct ParseOutput {
    pub(crate) parsed: Vec<ToolCall>,
    pub(crate) assistant_msg: Message,
    pub(crate) effect: Effect,
}

/// 响应→assistant 入库 + RoundComplete + seal。
///
/// 覆盖原 `run_lap` 中 gate 成功后到 `match effect` 前的全部步骤（verbatim）：
/// - `util::parse_tool_calls_from_response`
/// - 为未提前 `ToolCallProgress` 的结构化工具调用补 `BlockOpened`
/// - `util::build_assistant_message` + `response_output_items` 追加
/// - `ctx.flow.ingest(MODEL, assistant_msg)` + `unwrap_or(Effect::None)` + `flush_meta`
/// - `util::emit_round_complete_via_emitter`
/// - `gate::seal_active_stream_block`
/// - `parsed.is_empty() ⇒ RoundSealed(is_final:true)`
///
/// `active_stream_block` / `timeline_tools_open` 通过 `&mut` 原地更新；
/// 返回的 `ParseOutput` 供剩余 `run_lap` 消费（tools/admit 回填）。
pub(crate) fn parse_and_ingest(
    ctx: &mut RingContext<'_>,
    turn_id: &str,
    round_num: u32,
    content: &str,
    reasoning: &str,
    tool_calls_raw: &serde_json::Value,
    response_output_items: Vec<ContentBlock>,
    active_stream_block: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
    timeline_tools_open: &mut HashSet<String>,
) -> ParseOutput {
    // ── Parse ──
    let parsed =
        util::parse_tool_calls_from_response(content, reasoning, tool_calls_raw, &ctx.agent);
    // Structured/non-streamed tool calls can arrive without a prior
    // ToolCallProgress event. Open their native blocks here so every
    // later lifecycle patch has one stable target.
    for tool_call in &parsed {
        if timeline_tools_open.insert(tool_call.id.clone()) {
            let args_json = tool_call.function.arguments.clone();
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id: format!("tool:{}", tool_call.id),
                    kind: qaqh_domain::TimelineBlockKind::Tool,
                    tool: Some(qaqh_domain::TimelineTool {
                        tool_call_id: tool_call.id.clone(),
                        name: tool_call.function.name.clone(),
                        state: qaqh_domain::TimelineToolState::Prepared,
                        summary: None,
                        args_json: Some(args_json),
                        output: None,
                        diff: None,
                        progress: String::new(),
                        failure: None,
                        permission: None,
                    }),
                });
        }
    }
    let mut assistant_msg = util::build_assistant_message(content, reasoning, &parsed);
    assistant_msg.content.extend(response_output_items);
    let receipt = ctx.flow.ingest(
        &mut ctx.agent.msg,
        qaqh_message::builtin::MODEL,
        assistant_msg.clone(),
    );
    // model 源（Sink::Step）的 store 层决策透传：TurnComplete 结束回合，
    // None 表示可能有工具待执行。
    let effect = receipt.effect.unwrap_or(Effect::None);
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

    util::emit_round_complete_via_emitter(
        ctx.emitter,
        turn_id,
        round_num,
        &assistant_msg,
        content,
        reasoning,
        &parsed,
    );

    // Native transcript sealing is independent from the legacy
    // RoundComplete projection. A Markdown consumer only sees these
    // blocks as final after the explicit seal.
    super::gate::seal_active_stream_block(ctx, turn_id, round_num, active_stream_block);
    if parsed.is_empty() {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.to_string(),
                round_num,
                is_final: true,
            });
    }

    // BUG-015 run_lap 级断言：parse 后 timeline_tools_open 必须与 parsed 去重一致，且 seal 已完成
    debug_assert!(
        timeline_tools_open.len() >= parsed.len(),
        "BUG-015: timeline_tools_open {} < parsed {}",
        timeline_tools_open.len(),
        parsed.len()
    );
    for tc in &parsed {
        debug_assert!(
            timeline_tools_open.contains(&tc.id),
            "BUG-015: parsed tool {} not in timeline_tools_open",
            tc.id
        );
    }
    if let Some((kind, block_id)) = active_stream_block.as_ref() {
        debug_assert!(
            matches!(
                kind,
                qaqh_domain::TimelineBlockKind::Text | qaqh_domain::TimelineBlockKind::Reasoning
            ),
            "active_stream_block must be text/reasoning, got {:?}",
            kind
        );
        debug_assert!(
            block_id.starts_with(&format!("round-{round_num}:")),
            "active_stream_block not round-scoped: {}",
            block_id
        );
    }

    ParseOutput {
        parsed,
        assistant_msg,
        effect,
    }
}
