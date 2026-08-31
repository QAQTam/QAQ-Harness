//! ToolEngine: permission admission + tool execution.
//!
//! Owns: pending_approvals, trusted_folders.
//! Handles: UI tool calls (via handle_ui_tool_call) and LLM tool calls
//!          (via admit_batch from TurnEngine).
//!
//! Key design: a single admit() entry point for both UI and LLM paths.
//! The old code had two separate code paths; now they converge here.

use std::collections::{HashMap, VecDeque};

use super::dashboard;
use crate::agent::state::agent::PendingApproval;
use qaqh_domain::{AskMode, AskQuestion};

use super::types::*;

fn timeline_tool(
    tool_call_id: &str,
    name: &str,
    state: qaqh_domain::TimelineToolState,
    args_json: Option<String>,
    output: Option<String>,
    diff: Option<String>,
    failure: Option<qaqh_domain::TimelineFailure>,
) -> qaqh_domain::TimelineTool {
    qaqh_domain::TimelineTool {
        tool_call_id: tool_call_id.to_string(),
        name: name.to_string(),
        state,
        summary: output.clone(),
        args_json,
        output,
        diff,
        progress: String::new(),
        failure,
        permission: None,
    }
}

fn emit_timeline_tool_progress(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    tool_call_id: &str,
    chunk: String,
) {
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::ToolProgress {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: format!("tool:{tool_call_id}"),
            chunk,
        });
}

pub struct ToolEngine {
    /// Pending permission approvals (keyed by tool_call_id).
    pub(crate) pending: HashMap<String, PendingApproval>,
}

impl ToolEngine {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Native lifecycle update for a model-originated tool block. The block is
    /// opened by TurnEngine while parsing the assistant response; execution
    /// only changes its mutable state.
    pub fn emit_timeline_tool_running(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        tool_call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_call_id}"),
                tool: timeline_tool(
                    tool_call_id,
                    name,
                    qaqh_domain::TimelineToolState::Running,
                    Some(args.to_string()),
                    None,
                    None,
                    None,
                ),
            });
    }

    pub fn emit_timeline_tool_result(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        tool_call_id: &str,
        name: &str,
        args: &str,
        output: &str,
        success: bool,
        diff: Option<String>,
    ) {
        let failure = (!success).then(|| qaqh_domain::TimelineFailure {
            code: "TOOL_EXECUTION_FAILED".into(),
            message: output.to_string(),
        });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_call_id}"),
                tool: timeline_tool(
                    tool_call_id,
                    name,
                    if success {
                        qaqh_domain::TimelineToolState::Succeeded
                    } else {
                        qaqh_domain::TimelineToolState::Failed
                    },
                    Some(args.to_string()),
                    Some(output.to_string()),
                    diff,
                    failure,
                ),
            });
    }

    // ═══════════════════════════════════════════════════
    // UI-initiated tool call
    // ═══════════════════════════════════════════════════

    pub fn handle_ui_tool_call(
        &mut self,
        ctx: &mut RingContext,
        id: &str,
        name: &str,
        action: &str,
        args: &serde_json::Value,
    ) {
        let effective_name = crate::agent::util::resolve_effective_name(name, action, args);

        qaqh_workspace::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        match qaqh_workspace::authorize_call(
            &ctx.agent.session.seed,
            id,
            &effective_name,
            args,
            ctx.agent.config.permission_level,
        ) {
            qaqh_workspace::Admission::Authorized(authorized) => {
                self.execute_and_emit(ctx, id, &effective_name, args, authorized, false);
            }
            qaqh_workspace::Admission::ApprovalRequired(challenge) => {
                let cat_str = challenge.category().as_str().to_string();
                let cat_domain = match challenge.category() {
                    qaqh_workspace::ToolCategory::Read => {
                        qaqh_domain::PermissionCategory::Read
                    }
                    qaqh_workspace::ToolCategory::Write => {
                        qaqh_domain::PermissionCategory::Write
                    }
                    qaqh_workspace::ToolCategory::Exec => {
                        qaqh_domain::PermissionCategory::Exec
                    }
                    qaqh_workspace::ToolCategory::Net => {
                        qaqh_domain::PermissionCategory::Net
                    }
                };
                let risk_domain = match challenge.risk() {
                    qaqh_workspace::PermissionRisk::Low => {
                        qaqh_domain::PermissionRisk::Low
                    }
                    qaqh_workspace::PermissionRisk::Medium => {
                        qaqh_domain::PermissionRisk::Medium
                    }
                    qaqh_workspace::PermissionRisk::High => {
                        qaqh_domain::PermissionRisk::High
                    }
                };
                let turn_id = format!("tc_{}", challenge.call_id());
                let permission = qaqh_domain::TimelineToolPermission {
                    reason: challenge.reason().to_string(),
                    paths: challenge
                        .resources()
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                    category: cat_str.clone(),
                    level: ctx.agent.config.permission_level,
                    risk: match risk_domain {
                        qaqh_domain::PermissionRisk::Low => "low",
                        qaqh_domain::PermissionRisk::Medium => "medium",
                        qaqh_domain::PermissionRisk::High => "high",
                    }
                    .to_string(),
                    consequence: challenge.consequence().to_string(),
                };
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TurnOpened {
                        turn_id: turn_id.clone(),
                        user_text: format!("tool: {name}"),
                    });
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                        turn_id,
                        round_num: 0,
                        block_id: format!("tool:{}", challenge.call_id()),
                        kind: qaqh_domain::TimelineBlockKind::Tool,
                        tool: Some(qaqh_domain::TimelineTool {
                            tool_call_id: challenge.call_id().to_string(),
                            name: challenge.tool_name().to_string(),
                            state: qaqh_domain::TimelineToolState::Prepared,
                            summary: None,
                            args_json: Some(args.to_string()),
                            output: None,
                            diff: None,
                            progress: String::new(),
                            failure: None,
                            permission: Some(permission),
                        }),
                    });
                // Ringing 双发：ToolPermissionRequested（权限请求归 Tool 频道）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                    qaqh_domain::ToolEvent::ToolPermissionRequested {
                        tool_call_id: challenge.call_id().to_string(),
                        turn_id: format!("tc_{}", challenge.call_id()),
                        round_num: 0,
                        tool_name: challenge.tool_name().to_string(),
                        reason: challenge.reason().to_string(),
                        paths: challenge
                            .resources()
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect(),
                        category: cat_domain,
                        level: ctx.agent.config.permission_level,
                        risk: risk_domain,
                        consequence: challenge.consequence().to_string(),
                    },
                ));
                self.pending.insert(
                    challenge.call_id().to_string(),
                    PendingApproval {
                        challenge,
                        is_llm_tool: false,
                    },
                );
            }
            qaqh_workspace::Admission::Denied(reason) => {
                let turn_id = format!("tc_{id}");
                Self::emit_timeline_denied(ctx, id, name, &args.to_string(), &reason, false);
                // Ringing 终态统一由 ToolFinished 承载，失败只由 result.status 表达。
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                    qaqh_domain::ToolEvent::ToolFinished {
                        tool_call_id: id.to_string(),
                        turn_id: turn_id.clone(),
                        round_num: 0,
                        result: qaqh_types::ToolResult::error_with(
                            "TOOL_DENIED",
                            reason.to_string(),
                            false,
                            None,
                        ),
                    },
                ));
            }
        }
    }

    // ═══════════════════════════════════════════════════
    // Permission response handler (called from Loop::dispatch)
    // ═══════════════════════════════════════════════════

    pub fn handle_permission_response(
        &mut self,
        ctx: &mut RingContext,
        tool_call_id: &str,
        approved: bool,
        trust_folder: bool,
    ) -> PermissionDisposition {
        let pending = match self.pending.remove(tool_call_id) {
            Some(p) => p,
            None => {
                log::warn!("[TOOL] unknown permission response: {tool_call_id}");
                return PermissionDisposition::Ignored;
            }
        };

        let call_id = pending.challenge.call_id().to_string();
        let tool_name = pending.challenge.tool_name().to_string();
        let is_llm = pending.is_llm_tool;
        let resources = pending.challenge.resources().to_vec();

        match pending.challenge.approve(approved) {
            Ok(authorized) => {
                if trust_folder {
                    for path in &resources {
                        qaqh_workspace::trust_folder(path.parent().unwrap_or(path));
                    }
                }
                if is_llm {
                    return PermissionDisposition::LlmResolved {
                        call_id: call_id.clone(),
                        admitted: Some(AdmittedTool {
                            call_id,
                            auth: Box::new(authorized),
                        }),
                    };
                } else {
                    // UI tool: emit full result flow
                    let args = authorized.args().clone();
                    self.execute_and_emit(ctx, &call_id, &tool_name, &args, authorized, true);
                }
            }
            Err(qaqh_workspace::ApprovalError::Rejected) => {
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!("[DENIED] '{tool_name}' (user denied permission)"),
                        false,
                    );
                } else {
                    self.emit_denied(ctx, &call_id, &tool_name, "user denied permission");
                }
            }
            Err(qaqh_workspace::ApprovalError::Expired) => {
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!("[EXPIRED] Permission expired for '{tool_name}'."),
                        false,
                    );
                } else {
                    self.emit_denied(ctx, &call_id, &tool_name, "permission expired");
                }
            }
            Err(qaqh_workspace::ApprovalError::MissingOrReplayed) => {
                log::warn!("[TOOL] replayed permission response: {call_id}");
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!(
                            "[EXPIRED] Permission response is no longer valid for '{tool_name}'."
                        ),
                        false,
                    );
                }
            }
        }

        if is_llm {
            PermissionDisposition::LlmResolved {
                call_id,
                admitted: None,
            }
        } else {
            PermissionDisposition::UiHandled
        }
    }

    // ═══════════════════════════════════════════════════
    // Batch admit for LLM tools (called from TurnEngine)
    // ═══════════════════════════════════════════════════

    /// Admit a batch of LLM tool calls.
    /// Denied tools are pushed directly into the message store.
    pub fn admit_batch(
        &mut self,
        ctx: &mut RingContext,
        tools: &[qaqh_message::PendingTool],
        turn_id: &str,
        round_num: u32,
    ) -> BatchAdmission {
        let mut authorized = Vec::new();
        let mut pending_permission_ids = Vec::new();
        let mut pending_asks = VecDeque::new();
        // L-msgloop②：pending_plans / pending_todo_activation 恒为空——
        // plan 评审与 todo 激活当前由 UI ToolInvoke / todo 工具自身闭环，
        // 不再走 lap 内挂起路径。若未来恢复挂起式评审，务必同步处理
        // handle_plan_response 中 co-pending asks 的排空陷阱：plan resolve
        // 后若 suspended.pending_asks 非空，必须重新 YieldToUser 而非直接
        // run_lap，否则 ask 的悬空 tool_use 会随下一轮被折叠。
        let pending_plans = VecDeque::new();
        let pending_todo_activation = None;

        for tool in tools {
            // 已移除 minimal:dsh PTY（bash_v2 下线），当前恒等。
            // 让权限准入 / prepare_req / handler 全部走内部 key，原生 bash 不参与。
            let effective_name = crate::agent::state::agent::AgentState::normalize_tool_name_for_mode(
                &ctx.agent.session.tool_mode,
                &tool.name,
            );
            match qaqh_workspace::authorize_call(
                &ctx.agent.session.seed,
                &tool.id,
                &effective_name,
                &tool.args,
                ctx.agent.config.permission_level,
            ) {
                qaqh_workspace::Admission::Authorized(auth) => {
                    if auth.tool_name() == "ask" {
                        match qaqh_workspace::ask_user::normalize_ask_user(auth.args()) {
                            Ok(normalized) => pending_asks.push_back(PendingAsk {
                                call_id: auth.call_id().to_string(),
                                mode: match normalized.mode {
                                    qaqh_workspace::ask_user::NormalizedAskMode::Single => {
                                        AskMode::Single
                                    }
                                    qaqh_workspace::ask_user::NormalizedAskMode::Batch => {
                                        AskMode::Batch
                                    }
                                },
                                questions: normalized
                                    .questions
                                    .into_iter()
                                    .map(|question| AskQuestion {
                                        id: question.id,
                                        question: question.question,
                                        options: question.options,
                                        allow_custom: question.allow_custom,
                                    })
                                    .collect(),
                            }),
                            Err(error) => ctx.agent.msg.push_tool_result_direct(
                                auth.call_id(),
                                &serde_json::json!({
                                    "status": "error",
                                    "code": error.code,
                                    "message": error.message,
                                })
                                .to_string(),
                                false,
                            ),
                        }
                    } else {
                        authorized.push(AdmittedTool {
                            call_id: tool.id.clone(),
                            auth: Box::new(auth), // Box to reduce enum size
                        });
                    }
                }
                qaqh_workspace::Admission::ApprovalRequired(challenge) => {
                    let cat_str = challenge.category().as_str().to_string();
                    let call_id = challenge.call_id().to_string();
                    let risk = match challenge.risk() {
                        qaqh_workspace::PermissionRisk::Low => "low",
                        qaqh_workspace::PermissionRisk::Medium => "medium",
                        qaqh_workspace::PermissionRisk::High => "high",
                    }
                    .to_string();
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::ToolUpdated {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: format!("tool:{call_id}"),
                            tool: qaqh_domain::TimelineTool {
                                tool_call_id: call_id.clone(),
                                name: challenge.tool_name().to_string(),
                                state: qaqh_domain::TimelineToolState::Prepared,
                                summary: None,
                                args_json: Some(tool.args.to_string()),
                                output: None,
                                diff: None,
                                progress: String::new(),
                                failure: None,
                                permission: Some(qaqh_domain::TimelineToolPermission {
                                    reason: challenge.reason().to_string(),
                                    paths: challenge
                                        .resources()
                                        .iter()
                                        .map(|path| path.to_string_lossy().to_string())
                                        .collect(),
                                    category: cat_str.clone(),
                                    level: ctx.agent.config.permission_level,
                                    risk,
                                    consequence: challenge.consequence().to_string(),
                                }),
                            },
                        });
                    // Ringing：LLM 工具轮权限请求（legacy PermissionRequest 的替代，
                    // 与 handle_ui_tool_call 路径一致）。
                    let cat_domain = match challenge.category() {
                        qaqh_workspace::ToolCategory::Read => {
                            qaqh_domain::PermissionCategory::Read
                        }
                        qaqh_workspace::ToolCategory::Write => {
                            qaqh_domain::PermissionCategory::Write
                        }
                        qaqh_workspace::ToolCategory::Exec => {
                            qaqh_domain::PermissionCategory::Exec
                        }
                        qaqh_workspace::ToolCategory::Net => {
                            qaqh_domain::PermissionCategory::Net
                        }
                    };
                    let risk_domain = match challenge.risk() {
                        qaqh_workspace::PermissionRisk::Low => {
                            qaqh_domain::PermissionRisk::Low
                        }
                        qaqh_workspace::PermissionRisk::Medium => {
                            qaqh_domain::PermissionRisk::Medium
                        }
                        qaqh_workspace::PermissionRisk::High => {
                            qaqh_domain::PermissionRisk::High
                        }
                    };
                    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                        qaqh_domain::ToolEvent::ToolPermissionRequested {
                            tool_call_id: call_id.clone(),
                            turn_id: turn_id.to_string(),
                            round_num,
                            tool_name: challenge.tool_name().to_string(),
                            reason: challenge.reason().to_string(),
                            paths: challenge
                                .resources()
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect(),
                            category: cat_domain,
                            level: ctx.agent.config.permission_level,
                            risk: risk_domain,
                            consequence: challenge.consequence().to_string(),
                        },
                    ));
                    pending_permission_ids.push(call_id.clone());
                    self.pending.insert(
                        call_id,
                        PendingApproval {
                            challenge,
                            is_llm_tool: true,
                        },
                    );
                }
                qaqh_workspace::Admission::Denied(reason) => {
                    ctx.agent.msg.push_tool_result_direct(
                        &tool.id,
                        &format!(
                            "[timeis: {}]\n[DENIED] {}",
                            crate::agent::util::chrono_local_datetime(),
                            reason
                        ),
                        false,
                    );
                }
            }
        }
        BatchAdmission {
            authorized,
            pending_permission_ids,
            pending_asks,
            pending_plans,
            pending_todo_activation,
        }
    }

    // ═══════════════════════════════════════════════════
    // Tool execution (shared by UI and LLM paths)
    // ═══════════════════════════════════════════════════

    /// Execute an authorized tool call and emit full result flow.
    fn execute_and_emit(
        &mut self,
        ctx: &mut RingContext,
        id: &str,
        name: &str,
        args: &serde_json::Value,
        authorized: qaqh_workspace::AuthorizedToolCall,
        approved: bool,
    ) {
        let turn_id = format!("tc_{id}");

        // A newly authorized UI tool owns a complete native turn. A
        // permission-approved tool resumes its stable pending block.
        if !approved {
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: turn_id.clone(),
                    user_text: format!("tool: {name}"),
                });
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    block_id: format!("tool:{id}"),
                    kind: qaqh_domain::TimelineBlockKind::Tool,
                    tool: Some(timeline_tool(
                        id,
                        name,
                        qaqh_domain::TimelineToolState::Prepared,
                        Some(args.to_string()),
                        None,
                        None,
                        None,
                    )),
                });
        }
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
                tool: timeline_tool(
                    id,
                    name,
                    qaqh_domain::TimelineToolState::Running,
                    Some(args.to_string()),
                    None,
                    None,
                    None,
                ),
            });

        // Ringing 双发：权限已通过 = 执行真正开始（决策记录 Q1）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: id.to_string(),
                turn_id: turn_id.clone(),
                round_num: 0,
                name: name.to_string(),
            },
        ));
        // Ringing 双发：RoundCompleted（工具回合的 initial round 终态）
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::RoundCompleted {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    thinking: None,
                    answer: None,
                    output_ref: None,
                    is_final: false,
                },
            ));

        // Spawn tool thread
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let tool_id = id.to_string();
        // Tool workers run on spawned threads: carry the actor's per-actor tool
        // scope so concurrent actors stay isolated.
        let actor_scope = qaqh_workspace::runtime::ActorToolScope::capture();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let _scope = actor_scope.install();
                let result =
                    qaqh_workspace::execution::execute_authorized(authorized, Some(progress_tx));
                (
                    tool_id,
                    result.result,
                    result.code_delta,
                    result.skill_effects,
                )
            })
            .expect("failed to spawn tool thread");

        // Drain progress
        self.drain_progress(ctx, progress_rx, &turn_id, 0);

        let (tid, result, code_delta, skill_effects) = handle.join().unwrap_or_else(|_| {
            (
                id.to_string(),
                qaqh_types::ToolResult::error("[ERROR] tool thread panicked"),
                None,
                Vec::new(),
            )
        });
        let output = result.model_text().to_string();
        let success = result.is_success();

        ctx.agent.apply_tool_effects(skill_effects, ctx.flow);

        // Instant refresh for todo tools
        if matches!(name, "todo") {
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

        if let Some(ref delta) = code_delta {
            ctx.stats.push_delta(delta.clone());
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                qaqh_domain::ToolEvent::CodeChanged {
                    tool_call_id: tid.clone(),
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    lines_added: delta.lines_added,
                    lines_removed: delta.lines_removed,
                    files_created: delta.files_created,
                    files_deleted: delta.files_deleted,
                    file: delta.file.clone(),
                },
            ));
        }

        // 展示平面 diff：先取出（ToolFinished 会 move 整个 result）。
        let display_diff = result.diff.clone();

        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::ToolFinished {
                tool_call_id: tid,
                turn_id: turn_id.clone(),
                round_num: 0,
                result,
            },
        ));
        let terminal_state = if success {
            qaqh_domain::TimelineToolState::Succeeded
        } else {
            qaqh_domain::TimelineToolState::Failed
        };
        let failure = (!success).then(|| qaqh_domain::TimelineFailure {
            code: "TOOL_EXECUTION_FAILED".into(),
            message: output.clone(),
        });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
                tool: timeline_tool(
                    id,
                    name,
                    terminal_state,
                    Some(args.to_string()),
                    Some(output.clone()),
                    display_diff,
                    failure,
                ),
            });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
            });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                is_final: true,
            });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
                turn_id: turn_id.clone(),
                state: if success {
                    qaqh_domain::TimelineTurnState::Completed
                } else {
                    qaqh_domain::TimelineTurnState::Failed
                },
                failure: (!success).then(|| qaqh_domain::TimelineFailure {
                    code: "TOOL_EXECUTION_FAILED".into(),
                    message: output.clone(),
                }),
            });
    }

    // ═══════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════

    /// Drain tool progress from external caller (TurnEngine).
    /// Unlike the internal drain_progress, this takes RingContext directly.
    pub fn drain_progress_external(
        &self,
        ctx: &mut RingContext,
        rx: std::sync::mpsc::Receiver<qaqh_workspace::ExecProgressEvent>,
        turn_id: &str,
        round_num: u32,
    ) {
        // A2：渲染尾部协议——尾部状态按 (tool_call_id, stream) 维护，跨事件累积。
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        Self::emit_progress_tail(ctx, turn_id, round_num, &event);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn drain_progress(
        &self,
        ctx: &mut RingContext,
        rx: std::sync::mpsc::Receiver<qaqh_workspace::ExecProgressEvent>,
        turn_id: &str,
        round_num: u32,
    ) {
        // A2：渲染尾部协议（与 drain_progress_external 共用发射 helper）。
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        Self::emit_progress_tail(ctx, turn_id, round_num, &event);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// A2：渲染尾部协议——每 (tool_call_id, stream) 只保留最后 4KB 尾部，
    /// 事件携带完整尾部（`seq_start` = 尾部覆盖的起始位置，`chunk` = 尾部全文），
    /// 前端**替换**而非拼接；不连续/丢 chunk 由下一次尾部自愈。
    fn emit_progress_tail(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        event: &qaqh_workspace::ExecProgressEvent,
    ) {
        emit_timeline_tool_progress(
            ctx,
            turn_id,
            round_num,
            &event.tool_call_id,
            event.chunk.clone(),
        );
    }

    fn emit_timeline_denied(
        ctx: &mut RingContext,
        call_id: &str,
        tool_name: &str,
        args_json: &str,
        reason: &str,
        already_open: bool,
    ) {
        let turn_id = format!("tc_{call_id}");
        let output = format!("[DENIED] '{tool_name}' ({reason})");
        if !already_open {
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: turn_id.clone(),
                    user_text: format!("tool: {tool_name}"),
                });
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    block_id: format!("tool:{call_id}"),
                    kind: qaqh_domain::TimelineBlockKind::Tool,
                    tool: Some(timeline_tool(
                        call_id,
                        tool_name,
                        qaqh_domain::TimelineToolState::Prepared,
                        Some(args_json.to_string()),
                        None,
                        None,
                        None,
                    )),
                });
        }
        Self::emit_timeline_tool_result(
            ctx, &turn_id, 0, call_id, tool_name, args_json, &output, false, None,
        );
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{call_id}"),
            });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                is_final: true,
            });
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
                turn_id,
                state: qaqh_domain::TimelineTurnState::Failed,
                failure: Some(qaqh_domain::TimelineFailure {
                    code: "tool_denied".into(),
                    message: reason.to_string(),
                }),
            });
    }

    fn emit_denied(&self, ctx: &mut RingContext, call_id: &str, tool_name: &str, reason: &str) {
        let turn_id = format!("tc_{call_id}");
        Self::emit_timeline_denied(ctx, call_id, tool_name, "{}", reason, true);
        // Ringing 终态统一由 ToolFinished 承载，失败只由 result.status 表达。
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::ToolFinished {
                tool_call_id: call_id.to_string(),
                turn_id: turn_id.clone(),
                round_num: 0,
                result: qaqh_types::ToolResult::error_with(
                    "TOOL_DENIED",
                    reason.to_string(),
                    false,
                    None,
                ),
            },
        ));
    }

    pub fn cancel_current(&self) {
        qaqh_workspace::runtime::cancel_current_tool();
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
        qaqh_workspace::runtime::clear_context();
    }
}

// ═══════════════════════════════════════════════════════
// Batch admission and permission response contracts
// ═══════════════════════════════════════════════════════

pub struct BatchAdmission {
    pub authorized: Vec<AdmittedTool>,
    pub pending_permission_ids: Vec<String>,
    pub pending_asks: VecDeque<PendingAsk>,
    pub pending_plans: VecDeque<PendingPlan>,
    pub pending_todo_activation: Option<PendingTodoActivation>,
}

pub enum PermissionDisposition {
    Ignored,
    UiHandled,
    LlmResolved {
        call_id: String,
        admitted: Option<AdmittedTool>,
    },
}
