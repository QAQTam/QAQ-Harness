//! agent::loop::outcome — 收尾判定（compact 簇 + emit_operation_* + apply_outcome）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use super::engine_compact::CompactMeta;
use super::loop_core::Loop;
use super::types::*;

impl Loop {
    pub(super) fn finish_pending_compact(&mut self, status: qaqh_domain::CompactStatus) {
        self.session.agent.finish_manual_compact();
        self.pending_compact_rx = None;
        let Some(compact_id) = self.pending_compact_id.take() else {
            self.pending_compact_causation = None;
            return;
        };
        let causation = self.pending_compact_causation.take();
        let _scope = self.paced_emitter.enter_causation(causation.as_deref());
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::CompactFinished {
                    compact_id,
                    status,
                    summary_chars: Some(0),
                    turns_compacted: Some(0),
                    turns_removed: Some(0),
                },
            ));
        self.dispatch_injections_after_compact();
    }

    /// Check if a background compact has completed and apply the result.
    pub(super) fn check_pending_compact(&mut self) {
        if let Some(ref rx) = self.pending_compact_rx {
            // G4：挂起/运行中不消费压缩结果——应用会折叠悬空 tool_use。
            // 结果留在 channel 里，安全点（Idle 且无 suspension）再取。
            if self.session.turn.is_suspended() || self.phase != LoopPhase::Idle {
                return;
            }
            match rx.try_recv() {
                Ok(meta) => {
                    self.session.agent.finish_manual_compact();
                    self.pending_compact_rx = None;
                    let compact_id = self.pending_compact_id.take();
                    let causation = self.pending_compact_causation.take();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                    if compact_id.as_deref() != Some(meta.compact_id.as_str()) {
                        log::warn!(
                            "[COMPACT] pending/result id mismatch: pending={compact_id:?}, result={}",
                            meta.compact_id
                        );
                    }
                    {
                        let mut ctx = RingContext {
                            agent: &mut self.session.agent,
                            emitter: &self.paced_emitter,
                            cancel: &self.cancel,
                            phase: &mut self.phase,
                            pending: &mut self.pending,
                            writer_dead: &self.writer_dead,
                            stats: &mut self.session.stats,
                            flow: &mut self.flow,
                        };
                        super::engine_compact::apply_result(&mut ctx, &meta);
                    }
                    // compact 完成且回到 idle：把 compact 期间入队的注入
                    // 逐条开新 turn（替代旧 compact-defer 特判的派发点）。
                    self.dispatch_injections_after_compact();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread died without sending result.
                    // Clear pending state and report error so frontend
                    // doesn't stay stuck at the "compacting" animation.
                    log::error!("[COMPACT] worker thread disconnected without result");
                    self.pending_compact_rx = None;
                    self.finish_pending_compact(qaqh_domain::CompactStatus::Failed);
                    // 失败同时由 OperationFailed 暴露具体原因。
                    self.emit_operation_failed(
                        "compact-worker-crashed",
                        qaqh_domain::ErrorScope::Conversation,
                        "compact_worker_crashed",
                        "Context compaction failed: worker thread crashed.",
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still running — check again next loop iteration.
                }
            }
        }
    }

    /// Compact 完成且 Loop 回到 idle 时，把 compact 期间入队（Deferred）的
    /// 注入逐条经 handle_system_input 开新 turn 派发（替代旧的
    /// compact-defer 特判：dispatch_ringing_one 不再推 deferred_ringing）。
    ///
    /// turn 仍运行中 / writer 已死时不动总线——turn 中的注入由下一个 lap
    /// 边界 `drain_injections` 落盘进当前回合（见缝插针语义不变），
    /// writer 死亡则进程即将退出。
    pub(super) fn dispatch_injections_after_compact(&mut self) {
        if self.writer_dead.load(Ordering::SeqCst)
            || self.phase != LoopPhase::Idle
            || self.injection_bus.pending_len() == 0
        {
            return;
        }
        let session_id = self.session.agent.session.seed.clone();
        self.injection_bus.switch_session(&session_id);
        let records = self.injection_bus.drain();
        for record in records {
            if record.session_id != session_id {
                log::warn!(
                    "[INJECT] skipped injection from stale session (record={}, current={})",
                    record.session_id,
                    session_id
                );
                continue;
            }
            let text = record.text;
            let command_id = record.command_id;
            let mut ctx = RingContext {
                agent: &mut self.session.agent,
                emitter: &self.paced_emitter,
                cancel: &self.cancel,
                phase: &mut self.phase,
                pending: &mut self.pending,
                writer_dead: &self.writer_dead,
                stats: &mut self.session.stats,
                flow: &mut self.flow,
            };
            // 与 `inject()` idle 路径一致：注入开 turn 期间的事件归属到
            // 注入者的 command_id（causation 作用域）。
            let _scope = self
                .paced_emitter
                .enter_causation(Some(command_id.as_str()));
            let outcome =
                self.input
                    .handle_system_input(&mut ctx, &text, Some(command_id.as_str()));
            let _ = ctx;
            self.apply_outcome(outcome);
        }
    }

    pub(super) fn emit_ringing_skills_status(&mut self) {
        let workspace = qaqh_workspace::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = self.session.agent.build_skills_status(&workspace);
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::SkillsUpdated {
                    available: status
                        .available
                        .iter()
                        .map(|s| qaqh_domain::SkillInfo {
                            name: s.name.clone(),
                            description: s.description.clone(),
                            scope: s.scope.clone(),
                            source: s.source.clone(),
                        })
                        .collect(),
                    active: status.active.clone(),
                    catalog_revision: Some(status.catalog_revision.clone()),
                    operation_revision: Some(status.operation_revision),
                    context_epoch: status.context_epoch as usize,
                    token_budget: status.token_budget,
                    token_usage: status.token_usage,
                    runtime: status
                        .runtime
                        .iter()
                        .map(|item| qaqh_domain::SkillRuntimeInfo {
                            name: item.name.clone(),
                            description: item.description.clone(),
                            state: item.state.clone(),
                            source: item.source.clone(),
                            token_count: item.token_count,
                            error: item.error.clone(),
                        })
                        .collect(),
                    diagnostics: status.diagnostics.clone(),
                },
            ));
    }

    // ═══════════════════════════════════════════════════
    // Single-command dispatch
    // ═══════════════════════════════════════════════════

    pub(super) fn start_compact(&mut self, causation: Option<String>) -> Outcome {
        // G4：挂起（权限/ask/plan 未决）或工具运行中禁止启动压缩——
        // 压缩会折叠悬空 tool_use，迟到 grant 将执行出孤儿结果。
        if self.session.turn.is_suspended() || self.phase != LoopPhase::Idle {
            return Outcome::Error(
                "Context compaction is not allowed while a turn is running or suspended.".into(),
            );
        }
        if self.pending_compact_rx.is_some() || self.session.agent.manual_compact_running() {
            return Outcome::Error("Context compaction is already running.".into());
        }
        let compact = {
            let mut ctx = RingContext {
                agent: &mut self.session.agent,
                emitter: &self.paced_emitter,
                cancel: &self.cancel,
                phase: &mut self.phase,
                pending: &mut self.pending,
                writer_dead: &self.writer_dead,
                stats: &mut self.session.stats,
                flow: &mut self.flow,
            };
            super::engine_compact::build_prompt_and_meta(&mut ctx)
        };
        if let Some((prompt, kept, head, provider, compact_id)) = compact {
            self.session.agent.begin_manual_compact();
            let context_revision = self.session.agent.msg.context_revision();
            let pending_compact_id = compact_id.clone();
            let (tx, rx) = mpsc::channel();
            let event_tx = self.event_tx.clone();
            let compact_seed = self.session.agent.session.seed.clone();
            let worker_causation = causation.clone();
            match std::thread::Builder::new()
                .name("compact-worker".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        super::engine_compact::run_compact_worker(
                            compact_seed,
                            compact_id.clone(),
                            prompt,
                            provider,
                            kept,
                            head,
                            context_revision,
                            event_tx,
                            worker_causation,
                        )
                    }));
                    let meta = match result {
                        Ok(meta) => meta,
                        Err(error) => CompactMeta {
                            compact_id,
                            summary: String::new(),
                            kept_user_count: kept,
                            head_user_count: head,
                            context_revision,
                            error: Some(format!(
                                "Compact worker panicked: {}",
                                Self::panic_msg_from_err(error)
                            )),
                        },
                    };
                    let _ = tx.send(meta);
                }) {
                Ok(_) => {
                    self.pending_compact_rx = Some(rx);
                    self.pending_compact_id = Some(pending_compact_id);
                    self.pending_compact_causation = causation;
                }
                Err(error) => {
                    log::error!("[COMPACT] failed to spawn worker: {error}");
                    self.session.agent.finish_manual_compact();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Control(
                            qaqh_domain::ControlEvent::OperationFailed {
                                occurrence_id: pending_compact_id.clone(),
                                scope: qaqh_domain::ErrorScope::Conversation,
                                error: qaqh_domain::DomainError {
                                    error_id: pending_compact_id.clone(),
                                    code: "compact_failed".into(),
                                    message: "Context compaction could not start.".into(),
                                    retryable: true,
                                    dedupe_key: Some("compact_failed".into()),
                                },
                                operation_id: Some(pending_compact_id.clone()),
                            },
                        ));
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::CompactFinished {
                                compact_id: pending_compact_id,
                                status: qaqh_domain::CompactStatus::Failed,
                                summary_chars: Some(0),
                                turns_compacted: Some(0),
                                turns_removed: Some(0),
                            },
                        ));
                }
            }
        } else {
            // L-msgloop③（降级后残余）：领域终态 CompactFinished{Skipped}
            // 已存在，此处仅补命令级 receipt，让发起 compact 的 command_id
            // 也能得到 ack（加法兼容，不改 envelope 形状）。
            if let Some(cid) = causation.as_deref() {
                self.emit_operation_failed(
                    cid,
                    qaqh_domain::ErrorScope::Conversation,
                    "compact_noop",
                    "Nothing to compact: history already fits the retention budget",
                );
            }
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactFinished {
                        compact_id: format!("compact-skipped-{}", self.session.agent.session.seed),
                        status: qaqh_domain::CompactStatus::Skipped,
                        summary_chars: Some(0),
                        turns_compacted: Some(0),
                        turns_removed: Some(0),
                    },
                ));
        }
        Outcome::Handled
    }

    /// Dispatch an already typed Ringing command without constructing a
    /// `Ui2Agent` frame. Legacy and Ringing ingress therefore remain separate
    /// at the worker boundary; both may share the domain engines underneath.
    pub(super) fn emit_operation_completed(
        &self,
        command_id: &str,
        scope: qaqh_domain::ErrorScope,
    ) {
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationCompleted {
                    occurrence_id: command_id.to_string(),
                    scope,
                    operation_id: Some(command_id.to_string()),
                },
            ));
    }

    pub(super) fn emit_operation_failed(
        &self,
        command_id: &str,
        scope: qaqh_domain::ErrorScope,
        code: &str,
        message: &str,
    ) {
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: command_id.to_string(),
                    scope,
                    error: qaqh_domain::DomainError {
                        error_id: command_id.to_string(),
                        code: code.to_string(),
                        message: message.to_string(),
                        retryable: false,
                        dedupe_key: Some(command_id.to_string()),
                    },
                    operation_id: Some(command_id.to_string()),
                },
            ));
    }

    // ═══════════════════════════════════════════════════
    // Outcome handler — the Ring's decision point
    // ═══════════════════════════════════════════════════

    /// Apply the outcome returned by an engine.
    ///
    /// This is the central decision point of the Ringing V1 architecture.
    /// Each Outcome variant maps to a specific Loop action:
    ///
    /// - `TurnComplete` → flush, emit TurnEnd + Done, return to Idle
    /// - `ContinueTurn` → re-enter TurnEngine for another gate lap (recursive)
    /// - `YieldToUser` → do nothing, wait for PermissionResponse or UserInput
    /// - `Handled` / `Error` / `Shutdown` → straightforward
    pub(super) fn apply_outcome(&mut self, outcome: Outcome) {
        // G5：孤儿 tool_result 升级为领域事件——store 只记录，此处统一
        // 上报（覆盖所有执行路径），前端不再对"已授权执行却消失"零感知。
        let orphans = self.session.agent.msg.take_orphan_tool_results();
        if !orphans.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::OperationFailed {
                        occurrence_id: format!("orphan-tool-result-{now_ms}"),
                        scope: qaqh_domain::ErrorScope::Tool,
                        error: qaqh_domain::DomainError {
                            error_id: format!("orphan-tool-result-err-{now_ms}"),
                            code: "orphan_tool_result".into(),
                            message: format!(
                                "{} tool result(s) could not be attached to their tool_use (turn superseded or aborted): {}",
                                orphans.len(),
                                orphans.join(", ")
                            ),
                            retryable: false,
                            dedupe_key: Some("orphan_tool_result".into()),
                        },
                        operation_id: None,
                    },
                ));
        }
        match outcome {
            Outcome::TurnComplete { turn_id, usage } => {
                self.session.agent.skills.complete_user_turn();
                // Persist session state
                self.session.flush();
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: None,
                            usage,
                        },
                    ));

                // Goal mode auto-advance: if the LLM completed a step
                // (via todo(action=set, status=completed)), inject the next step.
                if let Ok(store) = qaqh_workspace::todo::load_todo()
                    && store.mode == qaqh_workspace::todo::TodoMode::Goal
                    && let Some(ref current_id) = store.current_id
                    && let Some(item) = store.items.iter().find(|i| &i.id == current_id)
                    && item.status == qaqh_workspace::todo::TodoStatus::InProgress
                {
                    let prompt = format!(
                        "[自动执行计划 / 目标模式]\n\n\
                                         T{}: {}\n{}\n\n\
                                         完成此步骤后，调用 todo(action=\"set\", id=\"{}\", status=\"completed\", evidence=\"...\").",
                        item.id, item.title, item.description, item.id
                    );
                    let mut ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        flow: &mut self.flow,
                    };
                    let next_outcome = self.input.handle_user_input(
                        &mut ctx,
                        qaqh_message::builtin::GOAL,
                        &prompt,
                        vec![],
                    );
                    let _ = ctx;
                    self.apply_outcome(next_outcome);
                    return;
                }

                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnAborted { turn_id, usage } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                self.reset_all_engines();
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: Some("cancelled".into()),
                            usage,
                        },
                    ));
                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnFailed {
                turn_id,
                usage: _,
                message,
            } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnFailed {
                            turn_id,
                            error: qaqh_domain::DomainError {
                                error_id: format!(
                                    "turn-failed-{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis())
                                        .unwrap_or(0),
                                ),
                                code: "turn_failed".into(),
                                message,
                                retryable: false,
                                dedupe_key: None,
                            },
                        },
                    ));
                self.phase = LoopPhase::Idle;
            }
            Outcome::ContinueTurn {
                turn_id,
                round_num,
                usage,
            } => {
                // 见缝插针注入：工具调用回合结束 → 下一轮 gate 前，消费
                // cmd_rx 中排队的 as_system 注入（子代理报告等）进入总线，
                // 再由 ContextFlow 在 lap 边界落盘，使下一轮 LLM 请求立即可见。
                // 非注入命令移入 deferred 队列兜底。
                self.drain_pending_injections();
                self.drain_injections();
                // Another lap: re-enter TurnEngine.
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    flow: &mut self.flow,
                };
                let next_outcome = self.session.turn.run(
                    &mut ctx,
                    &mut self.session.tool,
                    turn_id,
                    round_num,
                    usage,
                );
                let _ = ctx;

                // Poll compact result after each turn lap — the background
                // compact thread may have completed while we were blocked
                // on SSE streaming. Without this, CompactEnd is delayed
                // until the entire turn finishes.
                self.check_pending_compact();

                self.apply_outcome(next_outcome);
            }
            Outcome::YieldToUser { .. } => {
                // Turn suspended. Loop returns to Idle. The next
                // PermissionResponse or a typed ask command will trigger resume.
            }
            Outcome::Handled => {}
            Outcome::Error(msg) => {
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Control(
                        qaqh_domain::ControlEvent::OperationFailed {
                            occurrence_id: format!(
                                "occ-outcome-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0),
                            ),
                            scope: qaqh_domain::ErrorScope::System,
                            error: qaqh_domain::DomainError {
                                error_id: format!(
                                    "outcome-error-{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis())
                                        .unwrap_or(0),
                                ),
                                code: "outcome_error".into(),
                                message: msg,
                                retryable: false,
                                dedupe_key: None,
                            },
                            operation_id: None,
                        },
                    ));
                self.phase = LoopPhase::Idle;
            }
            Outcome::Shutdown => {
                self.pending.shutdown = true;
            }
        }
    }
}
