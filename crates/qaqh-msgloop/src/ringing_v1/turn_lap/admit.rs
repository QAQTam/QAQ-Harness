//! Admit/dispatch 阶段：权限/ask/plan 审查 + 工具执行分发 (knife-7 A2 step5)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use std::collections::HashSet;

use qaqh_types::UsageInfo;

use crate::ringing_v1::engine_tool::ToolEngine;
use crate::ringing_v1::turn_lap::gate::{abort_running_turn, seal_timeline_terminal_round};
use crate::ringing_v1::types::*;
use crate::services::{conflict, dashboard};

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

fn emit_active_ask(ctx: &mut RingContext, state: &TurnState) {
    if let Some(ask) = state.pending_asks.front() {
        // Ringing 双发：InteractionRequested（ask 交互请求）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionRequested {
                interaction_id: ask.call_id.clone(),
                turn_id: state.turn_id.clone(),
                mode: ask.mode,
                questions: ask
                    .questions
                    .iter()
                    .map(|q| qaqh_domain::AskQuestion {
                        id: q.id.clone(),
                        question: q.question.clone(),
                        options: q.options.clone(),
                        allow_custom: q.allow_custom,
                    })
                    .collect(),
            },
        ));
    }
}

// ── execute_admitted_batch (moved verbatim from engine_turn.rs) ──

/// Execute a batch of already-authorized tools, partitioned by write conflicts.
///
/// Shared helper for `handle_permission_resolved` (deferred batch) and for
/// future callers that need the same parallel/serial + progress + code_delta
/// + skill-effects + dashboard logic. Verbatim from `TurnEngine::execute_admitted_batch`.
pub(crate) fn execute_admitted_batch(
    ctx: &mut RingContext,
    tool: &ToolEngine,
    mut admitted: Vec<AdmittedTool>,
    tool_call_order: &[String],
    serial_call_ids: &HashSet<String>,
    turn_id: &str,
    round_num: u32,
) -> bool {
    const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
    admitted.sort_by_key(|item| {
        tool_call_order
            .iter()
            .position(|id| id == &item.call_id)
            .unwrap_or(usize::MAX)
    });
    let mut ordered_skill_effects = Vec::new();
    let (mut parallel, serial): (Vec<_>, Vec<_>) = admitted
        .into_iter()
        .partition(|item| !serial_call_ids.contains(&item.call_id));

    while !parallel.is_empty() {
        let batch_len = parallel.len().min(MAX_PARALLEL_TOOL_WORKERS);
        let batch: Vec<_> = parallel.drain(..batch_len).collect();
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let mut handles = Vec::new();
        for admitted in batch {
            let tx = progress_tx.clone();
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
            let handle = std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn({
                    let auth = admitted.auth;
                    let id = call_id.clone();
                    // Tool workers run on spawned threads: reinstall the
                    // actor thread's per-actor tool scope (context /
                    // manager / mode / sandbox) so concurrent actors each
                    // execute under their own tool state.
                    let actor_scope = qaqh_workspace::runtime::ActorToolScope::capture();
                    move || {
                        let _scope = actor_scope.install();
                        let result = qaqh_workspace::execution::execute_authorized(*auth, Some(tx));
                        (
                            id,
                            result.content,
                            result.success,
                            result.result,
                            result.code_delta,
                            result.skill_effects,
                        )
                    }
                })
                .expect("tool thread spawn");
            handles.push((call_id, tool_name, handle));
        }
        drop(progress_tx);
        tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);

        let cancelled = ctx.cancel.is_set();
        for (call_id, tool_name, handle) in handles {
            if cancelled {
                let _ = handle.join();
                continue;
            }
            match handle.join() {
                Ok((_id, content, success, _canonical_result, code_delta, skill_effects)) => {
                    ctx.agent
                        .msg
                        .push_tool_result_direct(&call_id, &content, success);
                    ordered_skill_effects.push((call_id.clone(), skill_effects));
                    if let Some(ref delta) = code_delta {
                        ctx.stats.push_delta(delta.clone());
                        // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                            qaqh_domain::ToolEvent::CodeChanged {
                                tool_call_id: call_id.clone(),
                                turn_id: turn_id.to_string(),
                                round_num,
                                lines_added: delta.lines_added,
                                lines_removed: delta.lines_removed,
                                files_created: delta.files_created,
                                files_deleted: delta.files_deleted,
                                file: delta.file.clone(),
                            },
                        ));
                    }
                    // Instant refresh for todo tools
                    if matches!(tool_name.as_str(), "todo") {
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
                }
                Err(_) => ctx.agent.msg.push_tool_result_direct(
                    &call_id,
                    "[ERROR] tool thread panicked",
                    false,
                ),
            }
        }
    }

    for admitted in serial {
        if ctx.cancel.is_set() {
            return false;
        }
        let call_id = admitted.call_id;
        let tool_name = admitted.auth.tool_name().to_string();
        let tool_args = admitted.auth.args().clone();
        ToolEngine::emit_timeline_tool_running(
            ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
        );
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        // Tool workers run on spawned threads: carry the actor's per-actor
        // tool scope with them so concurrent actors stay isolated.
        let actor_scope = qaqh_workspace::runtime::ActorToolScope::capture();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let _scope = actor_scope.install();
                let result = qaqh_workspace::execution::execute_authorized(
                    *admitted.auth,
                    Some(progress_tx),
                );
                (
                    result.content,
                    result.success,
                    result.result,
                    result.code_delta,
                    result.skill_effects,
                )
            })
            .expect("tool thread spawn");
        tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);
        match handle.join() {
            Ok((content, success, _canonical_result, code_delta, skill_effects)) => {
                ctx.agent
                    .msg
                    .push_tool_result_direct(&call_id, &content, success);
                ordered_skill_effects.push((call_id.clone(), skill_effects));
                if let Some(ref delta) = code_delta {
                    ctx.stats.push_delta(delta.clone());
                    // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                        qaqh_domain::ToolEvent::CodeChanged {
                            tool_call_id: call_id.clone(),
                            turn_id: turn_id.to_string(),
                            round_num,
                            lines_added: delta.lines_added,
                            lines_removed: delta.lines_removed,
                            files_created: delta.files_created,
                            files_deleted: delta.files_deleted,
                            file: delta.file.clone(),
                        },
                    ));
                }
                // Instant refresh for todo tools
                if matches!(tool_name.as_str(), "todo") {
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
            }
            Err(_) => ctx.agent.msg.push_tool_result_direct(
                &call_id,
                "[ERROR] tool thread panicked",
                false,
            ),
        }
    }

    if ctx.cancel.is_set() {
        return false;
    }
    ordered_skill_effects.sort_by_key(|(call_id, _)| {
        tool_call_order
            .iter()
            .position(|id| id == call_id)
            .unwrap_or(usize::MAX)
    });
    for (_, effects) in ordered_skill_effects {
        ctx.agent.apply_tool_effects(effects, ctx.flow);
    }
    true
}

// ── Admit/dispatch for run_lap's Effect::None first batch ──

/// Handle the full `Effect::None` tool cycle for one gate lap.
///
/// Covers verbatim the inline block from `run_lap` after `parse_and_ingest`:
/// - `LoopPhase::ToolsRunning`
/// - duplicate ID check (→ `Handled`)
/// - `MAX_TOOL_CALLS_PER_ROUND = 16` truncate
/// - `tool_call_order` + `serial_call_ids` via `conflict::resolve_write_conflicts`
/// - `tool.admit_batch` + pre-execution suspend (permission / plan / todo)
/// - bounded parallel + serial execution (with `_with_diff`, `CodeChanged`, cancel)
/// - post-execution suspend (permission / ask / plan / todo)
///
/// Returns `Some(Outcome)` if the turn must yield/abort/handled immediately;
/// `None` means the caller should continue to backfill / `ContinueTurn`.
///
/// `suspended` is set in-place when yielding, matching `TurnEngine.suspended`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_and_dispatch(
    ctx: &mut RingContext,
    tool: &mut ToolEngine,
    turn_id: &str,
    round_num: u32,
    last_usage: Option<UsageInfo>,
    active_stream_block: &Option<(qaqh_domain::TimelineBlockKind, String)>,
    timeline_tools_open: &HashSet<String>,
    suspended: &mut Option<TurnState>,
) -> Option<Outcome> {
    // ── Execute tools ──
    *ctx.phase = LoopPhase::ToolsRunning;

    let mut pending = ctx.agent.msg.get_last_step_pending();
    if pending.is_empty() {
        return None;
    }

    // Duplicate tool-call ID check
    {
        let mut seen = HashSet::new();
        if pending.iter().any(|t| !seen.insert(t.id.clone())) {
            ctx.agent.msg.remove_last_step_if_incomplete();
            // Ringing 双发：OperationFailed（结构化错误）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: occurrence_id(),
                    scope: qaqh_domain::ErrorScope::Tool,
                    error: domain_failure(
                        "duplicate_tool_call",
                        "Duplicate tool-call ID from model".into(),
                        Some("duplicate_tool_call"),
                    ),
                    operation_id: None,
                },
            ));
            return Some(Outcome::Handled);
        }
    }

    // A model response must not create unbounded work. Rejected
    // calls still receive a result so the next round can recover.
    const MAX_TOOL_CALLS_PER_ROUND: usize = 16;
    if pending.len() > MAX_TOOL_CALLS_PER_ROUND {
        let rejected = pending.split_off(MAX_TOOL_CALLS_PER_ROUND);
        for call in rejected {
            ctx.agent.msg.push_tool_result_direct(
                &call.id,
                "[ERROR] Tool-call limit exceeded for this round (max 16). Retry the remaining calls in a later round.",
                false,
            );
        }
    }

    // Admit the complete model batch before executing any member.
    let tool_call_order = pending
        .iter()
        .map(|call| call.id.clone())
        .collect::<Vec<_>>();
    log::info!(
        "[TURN] run_lap turn_id={} round_num={} admit_batch {} pending tools",
        turn_id,
        round_num,
        pending.len()
    );
    const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
    let (_serial_groups, serial_after) = conflict::resolve_write_conflicts(&pending);
    let serial_call_ids: HashSet<String> = serial_after
        .iter()
        .map(|index| pending[*index].id.clone())
        .collect();
    let admission = tool.admit_batch(ctx, &pending, turn_id, round_num);
    if !admission.pending_permission_ids.is_empty()
        || !admission.pending_plans.is_empty()
        || admission.pending_todo_activation.is_some()
    {
        let reason = if !admission.pending_permission_ids.is_empty() {
            YieldReason::PermissionPending
        } else if admission.pending_todo_activation.is_some() {
            YieldReason::PlanReview // same UI interaction, review_type distinguishes
        } else {
            YieldReason::PlanReview
        };
        // Capture plan info before moving into TurnState
        let plan_submitted = if reason == YieldReason::PlanReview {
            if let Some(ref todo_act) = admission.pending_todo_activation {
                Some((
                    todo_act.call_id.clone(),
                    String::new(),
                    "todo_activation".to_string(),
                    Some(todo_act.items.clone()),
                ))
            } else {
                admission.pending_plans.front().map(|plan| {
                    (
                        plan.call_id.clone(),
                        plan.content.clone(),
                        "plan".to_string(),
                        None,
                    )
                })
            }
        } else {
            None
        };
        *suspended = Some(TurnState {
            session_id: ctx.agent.session.seed.clone(),
            turn_id: turn_id.to_string(),
            round_num,
            pending_permission_ids: admission.pending_permission_ids,
            deferred_authorized: admission.authorized,
            tool_call_order,
            serial_call_ids,
            pending_asks: admission.pending_asks,
            pending_plans: admission.pending_plans,
            pending_todo_activation: admission.pending_todo_activation,
            usage: last_usage.clone(),
            reason,
        });
        if let Some((call_id, plan_content, review_type, todo_items)) = plan_submitted {
            // Ringing 双发：PlanReviewRequested（plan 评审请求）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewRequested {
                    interaction_id: call_id.clone(),
                    turn_id: turn_id.to_string(),
                    plan_content,
                    review_type,
                    todo_items: todo_items.map(|items| {
                        items
                            .into_iter()
                            .map(|t| qaqh_domain::TodoItem {
                                id: t.id,
                                title: t.title,
                                description: t.description,
                                complexity: t.complexity,
                            })
                            .collect()
                    }),
                },
            ));
        }
        return Some(Outcome::YieldToUser {
            turn_id: turn_id.to_string(),
            reason,
        });
    }

    // ── Inline admitted-batch execution (verbatim from run_lap) ──
    let mut ordered_skill_effects = Vec::new();
    let (mut parallel_authorized, serial_authorized): (Vec<_>, Vec<_>) = admission
        .authorized
        .into_iter()
        .partition(|admitted| !serial_call_ids.contains(&admitted.call_id));

    // Execute independent tools in bounded parallel batches.
    while !parallel_authorized.is_empty() {
        let batch_len = parallel_authorized.len().min(MAX_PARALLEL_TOOL_WORKERS);
        let batch: Vec<_> = parallel_authorized.drain(..batch_len).collect();
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let mut handles: Vec<(String, std::thread::JoinHandle<_>)> = Vec::new();

        for admitted in batch {
            let tx = progress_tx.clone();
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
            let handle = std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn({
                    let auth = admitted.auth;
                    let cid = call_id.clone();
                    let actor_scope = qaqh_workspace::runtime::ActorToolScope::capture();
                    move || {
                        let _scope = actor_scope.install();
                        let result = qaqh_workspace::execution::execute_authorized(*auth, Some(tx));
                        (
                            cid,
                            result.content,
                            result.success,
                            result.result,
                            result.code_delta,
                            result.skill_effects,
                        )
                    }
                })
                .expect("tool thread spawn");
            handles.push((call_id, handle));
        }
        drop(progress_tx);

        // Drain progress
        tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);

        // Collect results
        let cancelled = ctx.cancel.is_set();
        for (call_id, h) in handles {
            if cancelled {
                let _ = h.join(); // reap
            } else {
                match h.join() {
                    Ok((_cid, content, success, _canonical_result, code_delta, skill_effects)) => {
                        ctx.agent.msg.push_tool_result_direct_with_diff(
                            &call_id,
                            &content,
                            success,
                            _canonical_result.diff.clone(),
                        );
                        ordered_skill_effects.push((call_id.clone(), skill_effects));
                        if let Some(ref delta) = code_delta {
                            ctx.stats.push_delta(delta.clone());
                            // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                                qaqh_domain::ToolEvent::CodeChanged {
                                    tool_call_id: call_id.clone(),
                                    turn_id: turn_id.to_string(),
                                    round_num,
                                    lines_added: delta.lines_added,
                                    lines_removed: delta.lines_removed,
                                    files_created: delta.files_created,
                                    files_deleted: delta.files_deleted,
                                    file: delta.file.clone(),
                                },
                            ));
                        }
                    }
                    Err(_) => {
                        ctx.agent.msg.push_tool_result_direct(
                            &call_id,
                            "[ERROR] tool thread panicked",
                            false,
                        );
                    }
                }
            }
        }
    }

    // Execute later same-file writers exactly once, after the
    // first writer from their conflict group has completed.
    for admitted in serial_authorized {
        if ctx.cancel.is_set() {
            break;
        }
        let call_id = admitted.call_id;
        let tool_name = admitted.auth.tool_name().to_string();
        let tool_args = admitted.auth.args().clone();
        ToolEngine::emit_timeline_tool_running(
            ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
        );
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn({
                let auth = admitted.auth;
                let actor_scope = qaqh_workspace::runtime::ActorToolScope::capture();
                move || {
                    let _scope = actor_scope.install();
                    let result =
                        qaqh_workspace::execution::execute_authorized(*auth, Some(progress_tx));
                    (
                        result.content,
                        result.success,
                        result.result,
                        result.code_delta,
                        result.skill_effects,
                    )
                }
            })
            .expect("tool thread spawn");
        tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);
        match handle.join() {
            Ok((content, success, _canonical_result, code_delta, skill_effects)) => {
                ctx.agent.msg.push_tool_result_direct_with_diff(
                    &call_id,
                    &content,
                    success,
                    _canonical_result.diff.clone(),
                );
                ordered_skill_effects.push((call_id.clone(), skill_effects));
                if let Some(ref delta) = code_delta {
                    ctx.stats.push_delta(delta.clone());
                    // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                        qaqh_domain::ToolEvent::CodeChanged {
                            tool_call_id: call_id.clone(),
                            turn_id: turn_id.to_string(),
                            round_num,
                            lines_added: delta.lines_added,
                            lines_removed: delta.lines_removed,
                            files_created: delta.files_created,
                            files_deleted: delta.files_deleted,
                            file: delta.file.clone(),
                        },
                    ));
                }
            }
            Err(_) => ctx.agent.msg.push_tool_result_direct(
                &call_id,
                "[ERROR] tool thread panicked",
                false,
            ),
        }
    }

    if ctx.cancel.is_set() {
        seal_timeline_terminal_round(
            ctx,
            turn_id,
            round_num,
            active_stream_block.as_ref(),
            timeline_tools_open,
            qaqh_domain::TimelineTurnState::Cancelled,
            None,
        );
        return Some(abort_running_turn(ctx, turn_id.to_string(), last_usage));
    }
    ordered_skill_effects.sort_by_key(|(call_id, _)| {
        tool_call_order
            .iter()
            .position(|id| id == call_id)
            .unwrap_or(usize::MAX)
    });
    for (_, effects) in ordered_skill_effects {
        ctx.agent.apply_tool_effects(effects, ctx.flow);
    }

    // Suspend before the next gate lap while any approval,
    // ask_user call, or plan review from this assistant round
    // is unresolved.
    if !admission.pending_permission_ids.is_empty()
        || !admission.pending_asks.is_empty()
        || !admission.pending_plans.is_empty()
        || admission.pending_todo_activation.is_some()
    {
        let reason = if !admission.pending_permission_ids.is_empty() {
            YieldReason::PermissionPending
        } else if admission.pending_todo_activation.is_some() {
            YieldReason::PlanReview
        } else if !admission.pending_plans.is_empty() {
            YieldReason::PlanReview
        } else {
            YieldReason::AskUser
        };
        // Capture plan info before moving into TurnState
        let plan_submitted = if reason == YieldReason::PlanReview {
            if let Some(ref todo_act) = admission.pending_todo_activation {
                Some((
                    todo_act.call_id.clone(),
                    String::new(),
                    "todo_activation".to_string(),
                    Some(todo_act.items.clone()),
                ))
            } else {
                admission.pending_plans.front().map(|plan| {
                    (
                        plan.call_id.clone(),
                        plan.content.clone(),
                        "plan".to_string(),
                        None,
                    )
                })
            }
        } else {
            None
        };
        *suspended = Some(TurnState {
            session_id: ctx.agent.session.seed.clone(),
            turn_id: turn_id.to_string(),
            round_num,
            pending_permission_ids: admission.pending_permission_ids,
            deferred_authorized: Vec::new(),
            tool_call_order,
            serial_call_ids,
            pending_asks: admission.pending_asks,
            pending_plans: admission.pending_plans,
            pending_todo_activation: None,
            usage: last_usage.clone(),
            reason,
        });
        if reason == YieldReason::AskUser {
            emit_active_ask(ctx, suspended.as_ref().expect("suspended ask state"));
        }
        if let Some((call_id, plan_content, review_type, todo_items)) = plan_submitted {
            // Ringing 双发：PlanReviewRequested（plan 评审请求）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewRequested {
                    interaction_id: call_id.clone(),
                    turn_id: turn_id.to_string(),
                    plan_content,
                    review_type,
                    todo_items: todo_items.map(|items| {
                        items
                            .into_iter()
                            .map(|t| qaqh_domain::TodoItem {
                                id: t.id,
                                title: t.title,
                                description: t.description,
                                complexity: t.complexity,
                            })
                            .collect()
                    }),
                },
            ));
        }
        return Some(Outcome::YieldToUser {
            turn_id: turn_id.to_string(),
            reason,
        });
    }

    None
}
