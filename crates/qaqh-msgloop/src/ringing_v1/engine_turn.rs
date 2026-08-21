//! TurnEngine: drives the gate→tools→repeat cycle.
//!
//! Owns: suspended TurnState.
//! Receives: RingContext + ToolEngine (for tool execution).
//! Returns: Outcome (ContinueTurn, YieldToUser, TurnComplete, Error).

use std::collections::HashMap;

use qaqh_domain::AskAnswer;
use qaqh_message::Effect;
use qaqh_types::UsageInfo;

use super::engine_tool::ToolEngine;
use super::types::*;
use crate::ringing_v1::turn_lap::admit as turn_admit;
use crate::ringing_v1::turn_lap::backfill as turn_backfill;
use crate::ringing_v1::turn_lap::gate::{
    GateRequestResult, abort_running_turn, gate_request, provider_for,
    seal_timeline_terminal_round,
};


/// Why the turn is being resumed.
pub enum ResumeReason {
    /// User answered permission dialogs — all approvals resolved.
    PermissionResolved,
}

/// 传输层请求快照 dump（诊断工具）：`QAQH_REQUEST_LOG=1` 时，每次 gate
/// 请求构建后把**真实传输的消息列表**写入 `<data>/sessions/<seed>/request-log.jsonl`
/// （每请求一行 JSON），与 `messages.jsonl`（store 理论落盘）对比，二分定位
/// "落盘 ≠ 传输"分叉点（例如 trailing 注入已持久化但模型请求未携带）。
/// 默认关闭，无任何正常路径开销。
fn dump_request_log(
    seed: &str,
    rev: u64,
    turns: usize,
    trailing: usize,
    messages: &[qaqh_types::Message],
) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var("QAQH_REQUEST_LOG")
            .map(|v| v == "1")
            .unwrap_or(false)
    }) {
        return;
    }
    if seed.is_empty() {
        return;
    }
    let dir = qaqh_types::platform::data_dir().join("sessions").join(seed);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("request-log.jsonl"))
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rec = serde_json::json!({
        "ts": now,
        "rev": rev,
        "turns": turns,
        "trailing": trailing,
        "n": messages.len(),
        "messages": messages.iter().map(|m| {
            let text = m.content.iter()
                .filter_map(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::json!({ "role": m.role, "text": text })
        }).collect::<Vec<_>>(),
    });
    use std::io::Write;
    let _ = writeln!(f, "{rec}");
}

/// TurnEngine manages a single LLM turn lifecycle.
pub struct TurnEngine {
    /// If Some, a turn is suspended waiting for permission or ask_user.
    pub(crate) suspended: Option<TurnState>,
}

impl TurnEngine {
    // ── gate stream helpers moved to turn_lap::gate (A2 step3) ──

    pub fn new() -> Self {
        Self { suspended: None }
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended.is_some()
    }

    /// Returns the reason the turn was suspended, or None if not suspended.
    pub fn suspended_reason(&self) -> Option<YieldReason> {
        self.suspended.as_ref().map(|s| s.reason)
    }

    pub fn suspended_turn_id(&self) -> Option<&str> {
        self.suspended.as_ref().map(|state| state.turn_id.as_str())
    }

    // ── Public API ──

    /// Run one full lap around the gate→tools ring.
    /// Called initially by InputEngine after user input, and recursively
    /// by Loop::apply_outcome for ContinueTurn.
    pub fn run(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        turn_id: String,
        round_num: u32,
        last_usage: Option<UsageInfo>,
    ) -> Outcome {
        self.run_lap(ctx, tool, turn_id, round_num, last_usage)
    }

    /// Resume a suspended turn.
    pub fn resume(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        reason: ResumeReason,
    ) -> Outcome {
        let saved = match self.suspended.take() {
            Some(s) => s,
            None => return Outcome::Error("No suspended turn to resume".into()),
        };
        if saved.session_id != ctx.agent.session.seed {
            log::warn!("[TURN] refusing to resume stale turn {}", saved.turn_id);
            return Outcome::Handled;
        }

        match reason {
            ResumeReason::PermissionResolved => {
                log::info!(
                    "[TURN] resuming turn {} round {}",
                    saved.turn_id,
                    saved.round_num
                );
                self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
                self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
            }
        }
    }

    /// Resolve one LLM permission by call ID. The turn only advances after
    /// every permission from the assistant round has been accounted for.
    pub fn handle_permission_resolved(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        admitted: Option<AdmittedTool>,
    ) -> Outcome {
        let Some(saved) = self.suspended.as_mut() else {
            log::warn!("[TURN] permission resolved without a suspended turn: {call_id}");
            return Outcome::Handled;
        };
        if saved.reason != YieldReason::PermissionPending
            || !saved.pending_permission_ids.iter().any(|id| id == call_id)
        {
            log::warn!("[TURN] stale permission resolution ignored: {call_id}");
            return Outcome::Handled;
        }

        if let Some(admitted) = admitted {
            saved.deferred_authorized.push(admitted);
        }
        saved.pending_permission_ids.retain(|id| id != call_id);
        if !saved.pending_permission_ids.is_empty() {
            return Outcome::YieldToUser {
                turn_id: saved.turn_id.clone(),
                reason: YieldReason::PermissionPending,
            };
        }

        let mut saved = self.suspended.take().expect("permission suspension exists");
        let deferred_authorized = std::mem::take(&mut saved.deferred_authorized);
        if !turn_admit::execute_admitted_batch(
            ctx,
            tool,
            deferred_authorized,
            &saved.tool_call_order,
            &saved.serial_call_ids,
            &saved.turn_id,
            saved.round_num,
        ) {
            let tool_ids = saved.tool_call_order.iter().cloned().collect();
            seal_timeline_terminal_round(
                ctx,
                &saved.turn_id,
                saved.round_num,
                None,
                &tool_ids,
                qaqh_domain::TimelineTurnState::Cancelled,
                None,
            );
            return abort_running_turn(ctx, saved.turn_id, saved.usage);
        }

        if !saved.pending_plans.is_empty() {
            saved.reason = YieldReason::PlanReview;
            let turn_id = saved.turn_id.clone();
            if let Some(plan) = saved.pending_plans.front() {
                // Ringing 双发：PlanReviewRequested（resume 重放）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::PlanReviewRequested {
                        interaction_id: plan.call_id.clone(),
                        turn_id: turn_id.clone(),
                        plan_content: plan.content.clone(),
                        review_type: "plan".to_string(),
                        todo_items: None,
                    },
                ));
            }
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::PlanReview,
            };
        }

        if !saved.pending_asks.is_empty() {
            saved.reason = YieldReason::AskUser;
            let turn_id = saved.turn_id.clone();
            Self::emit_active_ask(ctx, &saved);
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::AskUser,
            };
        }

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Validate and apply an answer to the front ask without consuming state
    /// on identity or payload errors.
    pub fn handle_ask_response(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        ask_id: &str,
        answers: &[AskAnswer],
    ) -> Outcome {
        let active = match self.suspended.as_ref() {
            Some(state) if state.reason == YieldReason::AskUser => {
                match state.pending_asks.front() {
                    Some(active) => active,
                    None => {
                        Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                        return Outcome::Handled;
                    }
                }
            }
            _ => {
                Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                return Outcome::Handled;
            }
        };

        if active.call_id != ask_id {
            Self::emit_ask_rejected(ctx, ask_id, "ask_id does not match the active prompt");
            return Outcome::Handled;
        }
        let ordered = match Self::validate_answers(active, answers) {
            Ok(ordered) => ordered,
            Err(message) => {
                Self::emit_ask_rejected(ctx, ask_id, &message);
                return Outcome::Handled;
            }
        };

        let mut saved = self.suspended.take().expect("active ask suspension exists");
        let active = saved.pending_asks.pop_front().expect("active ask exists");
        let content = serde_json::json!({
            "status": "answered",
            "answers": ordered,
        })
        .to_string();
        ctx.agent
            .msg
            .push_tool_result_direct(&active.call_id, &content, true);
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：InteractionResolved（ask 已回答）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionResolved {
                interaction_id: active.call_id,
                resolution: qaqh_domain::AskResolution::Answered,
            },
        ));

        if !saved.pending_asks.is_empty() {
            saved.reason = YieldReason::AskUser;
            let turn_id = saved.turn_id.clone();
            Self::emit_active_ask(ctx, &saved);
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::AskUser,
            };
        }

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Validate and apply a plan review decision without consuming state
    /// on identity or payload errors.
    pub fn handle_plan_response(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        approved: bool,
        message: &str,
        autonomous: bool,
    ) -> Outcome {
        let active_id = self
            .suspended
            .as_ref()
            .filter(|state| state.reason == YieldReason::PlanReview)
            .and_then(|state| {
                state
                    .pending_plans
                    .front()
                    .map(|p| p.call_id.as_str())
                    .or_else(|| {
                        state
                            .pending_todo_activation
                            .as_ref()
                            .map(|t| t.call_id.as_str())
                    })
            });
        if active_id != Some(call_id) {
            log::warn!("[TURN] plan response without a suspended review: {call_id}");
            return Outcome::Handled;
        }

        let mut saved = self
            .suspended
            .take()
            .expect("plan review suspension exists");

        // ── Todo activation path (Goal mode frozen) ──
        if let Some(todo_act) = saved.pending_todo_activation.take() {
            if approved {
                let content =
                    "Goal automation is temporarily unavailable. Use manual todo tools instead."
                        .to_string();
                ctx.agent
                    .msg
                    .push_tool_result_direct(&todo_act.call_id, &content, false);
                log::warn!(
                    "[TURN] approved todo activation rejected — Goal mode frozen: {}",
                    content
                );
            } else {
                ctx.agent.msg.push_tool_result_direct(
                    &todo_act.call_id,
                    &format!(
                        "Todo activation rejected: {}",
                        if message.is_empty() {
                            "no reason given"
                        } else {
                            message
                        }
                    ),
                    false,
                );
            }
            ctx.agent
                .msg
                .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
            // Ringing 双发：PlanReviewResolved（todo 激活裁决）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewResolved {
                    interaction_id: todo_act.call_id,
                    approved,
                },
            ));
            self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
            return self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage);
        }

        // ── Plan review path ──
        let plan = saved
            .pending_plans
            .pop_front()
            .expect("pending plan exists");

        let content = if approved && autonomous {
            format!(
                "Plan approved. Goal automation is currently frozen. Track execution with todo(action=\"create\") and todo(action=\"set\", id=\"T…\", status=\"…\"); mark unnecessary work with status=\"cancelled\".\n\n{}",
                plan.content
            )
        } else if approved {
            format!("Plan approved.\n\n{}", plan.content)
        } else {
            format!(
                "Plan rejected: {}\n\n{}",
                if message.is_empty() {
                    "no reason given"
                } else {
                    message
                },
                plan.content
            )
        };
        ctx.agent
            .msg
            .push_tool_result_direct(&plan.call_id, &content, approved);
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：PlanReviewResolved（plan 裁决）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::PlanReviewResolved {
                interaction_id: plan.call_id,
                approved,
            },
        ));

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Abort the active suspended ask. A stale dismiss leaves state untouched.
    pub fn handle_ask_dismiss(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        ask_id: &str,
    ) -> Outcome {
        let active_id = self
            .suspended
            .as_ref()
            .filter(|state| state.reason == YieldReason::AskUser)
            .and_then(|state| state.pending_asks.front())
            .map(|ask| ask.call_id.as_str());
        if active_id != Some(ask_id) {
            Self::emit_ask_rejected(ctx, ask_id, "ask_id does not match the active prompt");
            return Outcome::Handled;
        }

        let saved = self.suspended.take().expect("active ask suspension exists");
        let tool_ids = saved.tool_call_order.iter().cloned().collect();
        seal_timeline_terminal_round(
            ctx,
            &saved.turn_id,
            saved.round_num,
            None,
            &tool_ids,
            qaqh_domain::TimelineTurnState::Cancelled,
            None,
        );
        tool.clear_pending();
        ctx.agent.msg.remove_last_step_if_incomplete();
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：InteractionResolved��ask 交互终结）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionResolved {
                interaction_id: ask_id.to_string(),
                resolution: qaqh_domain::AskResolution::Dismissed,
            },
        ));
        Outcome::TurnAborted {
            turn_id: saved.turn_id,
            usage: saved.usage,
        }
    }

    fn emit_ask_rejected(ctx: &mut RingContext, ask_id: &str, message: &str) {
        // legacy AskRejected 退役：无 Ringing 专用事件，按 §4.2 登记
        // "由 OperationFailed（ErrorScope::Control, code=ask_rejected）覆盖"。
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::OperationFailed {
                occurrence_id: format!("occ-ask-rejected-{ask_id}"),
                scope: qaqh_domain::ErrorScope::Control,
                error: qaqh_domain::DomainError {
                    error_id: format!("ask-rejected-{ask_id}"),
                    code: "ask_rejected".into(),
                    message: message.to_string(),
                    retryable: false,
                    dedupe_key: Some(format!("ask_rejected:{ask_id}")),
                },
                operation_id: None,
            },
        ));
    }

    /// 构造结构化领域错误（error_id = 时间戳，dedupe 可选）。
    #[allow(dead_code)]
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

    /// OperationFailed 的 occurrence_id（时间戳）。
    #[allow(dead_code)]
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

    fn validate_answers(ask: &PendingAsk, answers: &[AskAnswer]) -> Result<Vec<AskAnswer>, String> {
        let mut supplied = HashMap::new();
        for answer in answers {
            if supplied
                .insert(answer.question_id.as_str(), answer.answer.as_str())
                .is_some()
            {
                return Err(format!("duplicate answer for {}", answer.question_id));
            }
        }

        let mut ordered = Vec::with_capacity(ask.questions.len());
        for question in &ask.questions {
            let answer = supplied
                .remove(question.id.as_str())
                .ok_or_else(|| format!("missing answer for {}", question.id))?;
            if answer.trim().is_empty() {
                return Err(format!("empty answer for {}", question.id));
            }
            if !question.options.iter().any(|option| option == answer) && !question.allow_custom {
                return Err(format!("invalid answer for {}", question.id));
            }
            ordered.push(AskAnswer {
                question_id: question.id.clone(),
                answer: answer.to_string(),
            });
        }
        if !supplied.is_empty() {
            return Err("response contains unknown question ids".into());
        }
        Ok(ordered)
    }

    // ── Internal lap execution ──

    // ── terminal/stream helpers moved to turn_lap::gate ──

    fn emit_compact_failure(ctx: &RingContext, message: String) {
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::OperationFailed {
                occurrence_id: format!(
                    "occ-auto-compact-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis())
                        .unwrap_or(0),
                ),
                scope: qaqh_domain::ErrorScope::Conversation,
                error: qaqh_domain::DomainError {
                    error_id: format!(
                        "err-auto-compact-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|duration| duration.as_millis())
                            .unwrap_or(0),
                    ),
                    code: "compact_failed".into(),
                    message,
                    retryable: true,
                    dedupe_key: Some("compact_failed".into()),
                },
                operation_id: None,
            },
        ));
    }

    /// Run compact inline during a gate lap boundary.
    /// Uses CompactEngine to build prompt, calls LLM inline (blocking),
    /// applies result, and streams CompactDelta events to the frontend.
    /// After compact, the current turn continues normally.
    fn run_auto_compact(ctx: &mut RingContext) -> bool {
        let compact_eng = super::engine_compact::CompactEngine::new();
        let (prompt, kept, head, provider, compact_id) =
            match compact_eng.build_prompt_and_meta(ctx) {
                Some(v) => v,
                None => return false,
            };
        let context_revision = ctx.agent.msg.context_revision();
        let turns_removed = ctx.agent.msg.turns().len().saturating_sub(kept);

        let emitter = ctx.emitter;
        let mut summary = String::new();
        let mut on_event = |ev: qaqh_gate::StreamEvent| match ev {
            qaqh_gate::StreamEvent::ContentDelta(d) => {
                summary.push_str(&d);
                // Ringing 双发：CompactProgress（replaceable 流式摘要）
                emitter.emit_domain(qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactProgress {
                        compact_id: compact_id.clone(),
                        delta: d,
                    },
                ));
            }
            qaqh_gate::StreamEvent::ReasoningDelta(d) => {
                // legacy CompactDelta reasoning 透传已退役（§4.2 登记：由 CompactProgress 覆盖）
                let _ = d;
            }
            _ => {}
        };

        let msgs = vec![qaqh_types::Message::user(&prompt)];
        let result = qaqh_gate::chat_stream(
            &provider,
            msgs,
            None,
            20480,
            None,
            None,
            None,
            &mut on_event,
        );

        match result {
            Ok(()) if !summary.trim().is_empty() => {
                if ctx.agent.msg.context_revision() != context_revision {
                    log::warn!(
                        "[TURN] auto-compact result became stale: source revision {}, current {}",
                        context_revision,
                        ctx.agent.msg.context_revision()
                    );
                    ctx.emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::CompactFinished {
                                compact_id,
                                status: qaqh_domain::CompactStatus::Cancelled,
                                summary_chars: Some(0),
                                turns_compacted: Some(0),
                                turns_removed: Some(0),
                            },
                        ));
                    return false;
                }
                let before = {
                    let (c, t, tc, tr, ts, sp, _, _) = ctx.agent.msg.compute_context_stats(None);
                    c + t + tc + tr + ts + sp
                };
                ctx.agent.msg.apply_compact(&summary, kept);
                ctx.agent
                    .msg
                    .snapshot_full(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                let after = {
                    let (c, t, tc, tr, ts, sp, _, _) = ctx.agent.msg.compute_context_stats(None);
                    c + t + tc + tr + ts + sp
                };
                // Ringing 双发：CompactFinished（成功终态）
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: qaqh_domain::CompactStatus::Completed,
                            summary_chars: Some(summary.chars().count()),
                            turns_compacted: Some(head as u32),
                            turns_removed: Some(turns_removed as u32),
                        },
                    ));
                log::info!("[TURN] auto-compact done: {before} → {after} tokens");
                true
            }
            Ok(()) => {
                let message = "Compact failed: model returned an empty response.".to_string();
                Self::emit_compact_failure(ctx, message.clone());
                // Ringing 双发：CompactFinished（空摘要 → 失败终态）
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: qaqh_domain::CompactStatus::Failed,
                            summary_chars: Some(0),
                            turns_compacted: Some(0),
                            turns_removed: Some(0),
                        },
                    ));
                log::error!("[TURN] auto-compact failed: {message}");
                false
            }
            Err(e) => {
                let message = format!("Compact failed: {e}");
                Self::emit_compact_failure(ctx, message);
                // Ringing 双发：CompactFinished（失败终态）
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: qaqh_domain::CompactStatus::Failed,
                            summary_chars: Some(0),
                            turns_compacted: Some(0),
                            turns_removed: Some(0),
                        },
                    ));
                log::error!("[TURN] auto-compact failed: {e}");
                false
            }
        }
    }

    // ── gate_request moved to turn_lap::gate (A2 step3) ──

    /// 构造本轮 gate 的传输快照并执行 auto-compact 预检（A2 step7 瘦身）。
    ///
    /// 覆盖原 `run_lap` 中“Build and measure”整段（verbatim 搬运）：
    /// `build_context` → `dump_request_log` → `estimate_prepared_request`
    /// → `auto_compact` 阈值判定与执行（含重建与重估计）→ `take_cache_diagnostics`
    /// → `token_calibration_fingerprint`/`prepared_request_key`。
    /// 返回 `(messages, request_estimate, fingerprint, key)` 供 `gate_request` 与
    /// 事后 `observe_prepared_request` 消费。
    fn prepare_gate_snapshot(
        ctx: &mut RingContext,
    ) -> (
        Vec<qaqh_types::Message>,
        crate::state::token_calibration::RequestTokenEstimate,
        String,
        String,
    ) {
        let mut messages = ctx.agent.build_context();
        dump_request_log(
            &ctx.agent.session.seed,
            ctx.agent.msg.context_revision(),
            ctx.agent.msg.turn_count(),
            ctx.agent.msg.trailing_messages().len(),
            &messages,
        );
        let mut request_estimate = ctx
            .agent
            .estimate_prepared_request(&messages, Some(&ctx.agent.tool_defs));
        let threshold = ctx.agent.config.auto_compact_threshold;
        if threshold > 0.0 {
            let limit = ctx.agent.config.context_limit as u64;
            let api_context_tokens = request_estimate.api_context_tokens;
            let decision_tokens = ctx.agent.auto_compact_decision_tokens(&request_estimate);
            if decision_tokens as f64 > limit as f64 * threshold {
                if !ctx.agent.auto_compact_allowed() {
                    log::debug!(
                        "[TURN] auto-compact skipped for unchanged failed candidate at revision {}",
                        ctx.agent.msg.context_revision()
                    );
                } else {
                    log::info!(
                        "[TURN] auto-compact preflight: source={}, decision={}, raw={}, predicted={}, upper={}/{limit} tokens ({} samples, {:.0}% threshold)",
                        if api_context_tokens.is_some() { "api" } else { "estimate" },
                        decision_tokens,
                        request_estimate.raw_tokens,
                        request_estimate.predicted_tokens,
                        request_estimate.upper_bound_tokens,
                        request_estimate.sample_count,
                        threshold * 100.0
                    );
                    let compacted = Self::run_auto_compact(ctx);
                    ctx.agent.record_auto_compact_result(compacted);
                    if compacted {
                        messages = ctx.agent.build_context();
                        request_estimate = ctx
                            .agent
                            .estimate_prepared_request(&messages, Some(&ctx.agent.tool_defs));
                        let post_compact_tokens = request_estimate
                            .api_context_tokens
                            .unwrap_or(request_estimate.upper_bound_tokens);
                        if post_compact_tokens as f64 > limit as f64 * threshold {
                            log::warn!(
                                "[TURN] post-compact preflight remains above threshold: source={}, decision={}/{limit}, upper={}",
                                if request_estimate.api_context_tokens.is_some() {
                                    "api"
                                } else {
                                    "estimate"
                                },
                                post_compact_tokens,
                                request_estimate.upper_bound_tokens
                            );
                        }
                    }
                }
            }
        }
        if let Some((hash, reasons)) = ctx.agent.take_cache_diagnostics() {
            let _ = (hash, reasons);
        }
        let request_fingerprint = ctx.agent.token_calibration_fingerprint();
        let request_key = ctx
            .agent
            .prepared_request_key(&messages, Some(&ctx.agent.tool_defs));
        (messages, request_estimate, request_fingerprint, request_key)
    }

    fn run_lap(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        turn_id: String,
        round_num: u32,
        last_usage: Option<UsageInfo>,
    ) -> Outcome {
        log::info!("[TURN] run_lap turn_id={} round_num={}", turn_id, round_num);
        // Rebuild provider from current config（gate_lap 准备逻辑，见 turn_lap::gate）
        let provider = provider_for(ctx);

        loop {
            // ── Interrupt check ──
            if ctx.cancel.is_set()
                || qaqh_workspace::is_cancel()
            {
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
                        turn_id: turn_id.clone(),
                        state: qaqh_domain::TimelineTurnState::Cancelled,
                        failure: None,
                    });
                return abort_running_turn(ctx, turn_id, last_usage);
            }
            if !ctx.pending.is_empty() {
                ctx.agent.msg.remove_last_step_if_incomplete();
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                return Outcome::Handled;
            }

            // ── 传输快照 + 估计 + auto-compact 预检（已抽至 prepare_gate_snapshot） ──
            let (messages, request_estimate, request_fingerprint, request_key) =
                Self::prepare_gate_snapshot(ctx);

            let GateRequestResult {
                content,
                reasoning,
                tool_calls_raw,
                response_output_items,
                mut active_stream_block,
                mut timeline_tools_open,
                had_error,
                done_seen,
                gate_error,
                current_request_usage,
                request_error,
                last_usage,
            } = gate_request(
                ctx,
                &provider,
                messages,
                Some(ctx.agent.tool_defs.clone()),
                &turn_id,
                round_num,
                last_usage,
            );


            if ctx.cancel.is_set() {
                seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    qaqh_domain::TimelineTurnState::Cancelled,
                    None,
                );
                return abort_running_turn(ctx, turn_id, last_usage);
            }

            if (had_error || request_error.is_some()) && !done_seen {
                log::info!(
                    "[TURN] run_lap turn_id={} round_num={} gate error or had_error={}",
                    turn_id,
                    round_num,
                    had_error
                );
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                let message = gate_error
                    .or_else(|| request_error.clone())
                    .unwrap_or_else(|| "Model request failed".into());
                seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    qaqh_domain::TimelineTurnState::Failed,
                    Some(qaqh_domain::TimelineFailure {
                        code: "model_request_failed".into(),
                        message: message.clone(),
                    }),
                );
                return Outcome::TurnFailed {
                    turn_id,
                    usage: last_usage,
                    message,
                };
            }
            if done_seen && (had_error || request_error.is_some()) {
                // 内容已完整流式输出后才报错（[DONE] 后断连/杂散错误）：
                // 不否定已完成的作答——"作答完成却 TurnFailed"会造成前端
                // 状态残留（markdown 不集中渲染、下一条消息带上一条）。
                log::warn!(
                    "[TURN] run_lap turn_id={} round_num={} gate error after Done (ignored): {}",
                    turn_id,
                    round_num,
                    gate_error
                        .clone()
                        .or(request_error.clone())
                        .unwrap_or_default()
                );
            }

            if let Some(usage) = current_request_usage {
                let accepted = ctx.agent.observe_prepared_request(
                    &request_fingerprint,
                    &request_key,
                    request_estimate.raw_tokens,
                    u64::from(usage.prompt_tokens),
                );
                log::debug!(
                    "[TURN] token calibration sample: raw={}, observed={}, accepted={accepted}",
                    request_estimate.raw_tokens,
                    usage.prompt_tokens
                );
            }

            log::info!(
                "[TURN] run_lap turn_id={} round_num={} gate succeeded, parsing response",
                turn_id,
                round_num
            );

            // ── Parse + push assistant message (turn_lap::parse) ──
            let crate::ringing_v1::turn_lap::parse::ParseOutput {
                parsed,
                assistant_msg,
                effect,
            } = crate::ringing_v1::turn_lap::parse::parse_and_ingest(
                ctx,
                &turn_id,
                round_num,
                &content,
                &reasoning,
                &tool_calls_raw,
                response_output_items,
                &mut active_stream_block,
                &mut timeline_tools_open,
            );
            // BUG-015 run_lap 级断言：parse 后的块级隔离在 parse::debug_assert 已验证；
            // 此处额外确保 lap 级“text↔tool 交替不重复回放” — timeline_tools_open 与 parsed 必须一致
            debug_assert!(
                timeline_tools_open.len() >= parsed.len(),
                "BUG-015 lap: timeline_tools_open {} < parsed {}",
                timeline_tools_open.len(),
                parsed.len()
            );
            let _ = (&parsed, &assistant_msg);

            match effect {
                Effect::None => {
                    // ── Admit/dispatch 段已迁入 turn_lap::admit (knife-7 A2 step5) ──
                    if let Some(outcome) = turn_admit::admit_and_dispatch(
                        ctx,
                        tool,
                        &turn_id,
                        round_num,
                        last_usage.clone(),
                        &active_stream_block,
                        &timeline_tools_open,
                        &mut self.suspended,
                    ) {
                        return outcome;
                    }

                    // All tools from this round are now resolved → backfill/skills/ContinueTurn (knife-7 A2 step6)
                    return turn_backfill::handle_tools_done(ctx, turn_id, round_num, last_usage);
                }
                Effect::TurnComplete => {}
                _ => {}
            }

            // TurnComplete / fall-through → backfill/skills/ContinueTurn (knife-7 A2 step6)
            return turn_backfill::handle_turn_complete(ctx, turn_id, round_num, last_usage);
        }
    }

    fn emit_completed_tool_round(
        &self,
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
    ) -> Vec<(String, String, String, bool, Option<String>)> {
        // Delegates to turn_lap::backfill (knife-7 A2 step6) — keeps handle_* sites thin
        // while sharing one verbatim implementation.
        turn_backfill::emit_completed_tool_round(ctx, turn_id, round_num)
    }

    /// Reset all turn state (called on Cancel / new session).
    pub fn reset(&mut self) {
        self.suspended = None;
    }

    pub fn take_suspended_for_abort(&mut self) -> Option<(String, Option<UsageInfo>)> {
        self.suspended
            .take()
            .map(|state| (state.turn_id, state.usage))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingEmitter {
        conversation: RefCell<Vec<(qaqh_domain::RoundDeltaKind, String)>>,
        timeline: RefCell<Vec<(String, String)>>,
    }

    impl crate::ringing_v1::types::Emitter for RecordingEmitter {
        fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
            if let qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::BlockCheckpoint { kind, text, .. },
            ) = event
            {
                self.conversation.borrow_mut().push((kind, text));
            }
        }

        fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
            if let qaqh_domain::TimelineIntent::BlockCheckpoint { block_id, text, .. } = intent {
                self.timeline.borrow_mut().push((block_id, text));
            }
        }
    }

    fn assert_block_scope_after_tool(
        kind: qaqh_domain::RoundDeltaKind,
        first_block_id: &str,
        second_block_id: &str,
        first_delta: &str,
        second_delta: &str,
    ) {
        use crate::ringing_v1::turn_lap::gate::{
            CHECKPOINT_TOKEN_INTERVAL, emit_stream_block_checkpoint, reset_stream_block_checkpoint,
        };
        let emitter = RecordingEmitter::default();
        let mut stream_block_id = None;
        let mut stream_block_text = String::new();
        let mut checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let mut last_checkpoint_at = std::time::Instant::now();

        let first_round = first_delta.to_string();
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            first_block_id,
            kind,
            &first_round,
            first_delta,
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        // The tool block seals the first stream block before the next one opens.
        reset_stream_block_checkpoint(
            &mut stream_block_id,
            &mut stream_block_text,
        );

        checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let second_round = format!("{first_delta}{second_delta}");
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            second_block_id,
            kind,
            &second_round,
            second_delta,
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let final_round = format!("{second_round}-tail");
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            second_block_id,
            kind,
            &final_round,
            "-tail",
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        assert_eq!(
            emitter.conversation.into_inner(),
            vec![
                (kind, first_round),
                (kind, second_round),
                (kind, final_round),
            ]
        );

        assert_eq!(
            emitter.timeline.into_inner(),
            vec![
                (first_block_id.to_string(), first_delta.to_string()),
                (second_block_id.to_string(), second_delta.to_string()),
                (second_block_id.to_string(), format!("{second_delta}-tail")),
            ]
        );
    }

    #[test]
    fn checkpoints_keep_round_text_but_replace_each_stream_block_locally() {
        assert_block_scope_after_tool(
            qaqh_domain::RoundDeltaKind::Answering,
            "round-0:text:0",
            "round-0:text:1",
            "answer-before-tool",
            "answer-after-tool",
        );
        assert_block_scope_after_tool(
            qaqh_domain::RoundDeltaKind::Thinking,
            "round-0:reasoning:0",
            "round-0:reasoning:1",
            "reasoning-before-tool",
            "reasoning-after-tool",
        );
    }
}
