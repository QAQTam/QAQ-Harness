//! Backfill 阶段：工具结果回填 → skills/ContinueTurn 判定 (knife-7 A2 step6)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use qaqh_types::UsageInfo;

use crate::ringing_v1::engine_tool::ToolEngine;
use crate::ringing_v1::types::{Outcome, RingContext};
use crate::services::dashboard;
use crate::util;

// ── helpers (from engine_turn.rs, duplicated for phase decoupling) ──

fn domain_failure(
    code: &str,
    message: String,
    dedupe_key: Option<&str>,
) -> qaqh_domain::DomainError {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    qaqh_domain::DomainError {
        error_id: format!("err-{code}-{ts}"),
        code: code.to_string(),
        message,
        retryable: false,
        dedupe_key: dedupe_key.map(|s| s.to_string()),
    }
}

fn occurrence_id() -> String {
    format!(
        "occ-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
}

// ── emit_completed_tool_round (moved verbatim from engine_turn.rs) ──

/// Emit timeline + domain events for one completed tool round.
///
/// Verbatim from `TurnEngine::emit_completed_tool_round`. Shared by
/// `handle_tools_done` (run_lap fast path) and by the suspend/resume
/// handlers that remain in `engine_turn.rs` (`handle_*`).
pub(crate) fn emit_completed_tool_round(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
) -> Vec<(String, String, String, bool, Option<String>)> {
    let results = ctx.agent.msg.last_step_tool_results();
    let ts = util::chrono_local_datetime();
    for (tc_id, name, content, success, diff) in &results {
        let args = ctx
            .agent
            .msg
            .tool_call_args(tc_id)
            .map(|a| a.to_string())
            .unwrap_or_default();
        let summary: String = content
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        ToolEngine::emit_timeline_tool_result(
            ctx,
            turn_id,
            round_num,
            tc_id,
            name,
            &args,
            content,
            *success,
            diff.clone(),
        );
        // Ringing 双发：AuditRecorded（args 只进 content store，事件仅携带引用）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::AuditRecorded {
                tool_name: name.clone(),
                result_summary: summary.clone(),
                success: *success,
                time: ts.clone(),
                args_ref: None,
            },
        ));
        // Ringing 终态：ToolFinished（legacy 汇总 ToolResults 退役后的替代——
        // 批量执行路径每个工具单独发终态，与 UI 主动调用路径一致）。发射
        // 时机在 round 交互全部裁决之后（ask/plan/permission 收口处），
        // 保证事件顺序为"裁决先、终态后"，前端/测试按序消费。
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::ToolFinished {
                tool_call_id: tc_id.clone(),
                turn_id: turn_id.to_string(),
                round_num,
                result: if *success {
                    qaqh_types::ToolResult::ok(content.clone())
                } else {
                    qaqh_types::ToolResult::error(content.clone())
                },
            },
        ));
    }

    for (tool_call_id, _, _, _, _) in &results {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_call_id}"),
            });
    }
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
            turn_id: turn_id.to_string(),
            round_num,
            is_final: false,
        });
    // Refresh status bar tasks after every tool round
    if !results.is_empty() {
        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_seed: ctx.agent.session.seed.clone(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
            },
        ));
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardSnapshot {
                snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
            },
        ));
    }
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    results
}

// ── skills / ContinueTurn backfill (verbatim tail of run_lap) ──

/// Tail for `Effect::None` after tool execution: backfill + skills lap + next turn.
///
/// Verbatim from the `Effect::None` branch of `run_lap` after
/// `admit_and_dispatch` returns `None` (all tools resolved).
pub(crate) fn handle_tools_done(
    ctx: &mut RingContext,
    turn_id: String,
    round_num: u32,
    last_usage: Option<UsageInfo>,
) -> Outcome {
    // All tools from this round are now resolved.
    emit_completed_tool_round(ctx, &turn_id, round_num);

    if let Err(error) = ctx.agent.skills.complete_model_lap() {
        // Ringing 双发：OperationFailed（skill lap 失败）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::OperationFailed {
                occurrence_id: occurrence_id(),
                scope: qaqh_domain::ErrorScope::Tool,
                error: domain_failure("skill_lap", error, Some("skill_lap")),
                operation_id: None,
            },
        ));
    }
    // Host-forced activation (second ignored lap) changes the
    // active set: materialize the envelope at the latest
    // message position before the next model reply.
    ctx.agent.sync_skill_injection(ctx.flow);

    // Another lap: tools executed, back to Gate
    Outcome::ContinueTurn {
        turn_id,
        round_num: round_num + 1,
        usage: last_usage,
    }
}

/// Tail for `Effect::TurnComplete` (and fall-through ` _`): flush, skill check, seal or continue.
///
/// Verbatim from the post-`match effect` tail of `run_lap`.
pub(crate) fn handle_turn_complete(
    ctx: &mut RingContext,
    turn_id: String,
    round_num: u32,
    last_usage: Option<UsageInfo>,
) -> Outcome {
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    let forced = match ctx.agent.skills.complete_model_lap() {
        Ok(forced) => forced,
        Err(error) => {
            // Ringing 双发：OperationFailed（skill lap 失败）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: occurrence_id(),
                    scope: qaqh_domain::ErrorScope::Tool,
                    error: domain_failure("skill_lap", error, Some("skill_lap")),
                    operation_id: None,
                },
            ));
            Vec::new()
        }
    };
    if ctx.agent.skills.has_requested() || !forced.is_empty() {
        // Host-forced activation changed the active set: materialize
        // the envelope before the next model lap.
        ctx.agent.sync_skill_injection(ctx.flow);
        return Outcome::ContinueTurn {
            turn_id,
            round_num: round_num + 1,
            usage: last_usage,
        };
    }
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
            turn_id: turn_id.clone(),
            state: qaqh_domain::TimelineTurnState::Completed,
            failure: None,
        });
    Outcome::TurnComplete {
        turn_id,
        usage: last_usage,
    }
}
