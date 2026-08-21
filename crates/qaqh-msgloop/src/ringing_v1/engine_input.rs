//! InputEngine: user input handler.
//!
//! Receives raw user text, handles auto-session-creation, compliance guard,
//! and routes to TurnEngine for LLM processing.
use super::types::*;

pub struct InputEngine;

impl InputEngine {
    pub fn new() -> Self {
        Self
    }

    /// Handle user input. Returns an Outcome telling the Loop whether
    /// to start a turn, yield, or report an error.
    ///
    /// `source_id` selects the registered ContextFlow source (`"user"` for
    /// human input, `"goal"` for goal-mode auto-advance prompts) — every
    /// message enters the store through [`ContextFlow::ingest`], so the
    /// ingest trace covers user input exactly like any other source.
    pub fn handle_user_input(
        &self,
        ctx: &mut RingContext,
        source_id: &'static str,
        text: &str,
        images: Vec<qaqh_domain::ImageBlock>,
    ) -> Outcome {
        log::info!("[INPUT] handle_user_input called, text_len={}", text.len());
        // Auto-create session on first input
        if ctx.agent.session.seed.is_empty() {
            log::info!("[INPUT] auto-creating session on first user input");
            crate::state::lifecycle::create_session(ctx.agent);
            ctx.agent.rebind_store();
            // 新 seed 生成后立即同步，后续 Ringing 事件（TurnStarted 双发、
            // RoundDelta 流式等）才能携带正确路由键。
            ctx.emitter.set_seed(&ctx.agent.session.seed);
        }

        let text = if text == "[QAQ-Harness Goal: resume]" {
            match qaqh_workspace::todo::load_todo() {
                Ok(store) if store.mode == qaqh_workspace::todo::TodoMode::Goal => {
                    if let Some(current_id) = store.current_id {
                        if let Some(item) = store.items.iter().find(|i| i.id == current_id) {
                            format!(
                                "[自动执行计划 / 目标模式]\n\n继续执行 T{}: {}\n{}",
                                item.id, item.title, item.description
                            )
                        } else {
                            "目标模式无法恢复：当前步骤已丢失。".to_string()
                        }
                    } else {
                        "目标模式无法恢复：没有当前步骤。".to_string()
                    }
                }
                Ok(_) => {
                    "目标模式无法恢复：当前没有激活的 goal。使用 todo(action=\"activate\") 开始。"
                        .to_string()
                }
                Err(e) => format!("目标模式恢复失败：{e}"),
            }
        } else {
            if let Ok(mut store) = qaqh_workspace::todo::load_todo() {
                if store.mode == qaqh_workspace::todo::TodoMode::Goal {
                    if let Some(ref current_id) = store.current_id.clone() {
                        if let Some(item) = store.items.iter_mut().find(|i| &i.id == current_id) {
                            if item.status == qaqh_workspace::todo::TodoStatus::InProgress {
                                item.status = qaqh_workspace::todo::TodoStatus::Pending;
                            }
                        }
                    }
                    store.mode = qaqh_workspace::todo::TodoMode::Manual;
                    let _ = qaqh_workspace::todo::save_todo(&store);
                }
            }
            text.to_string()
        };

        ctx.cancel.clear();
        // NOTE: annotations are frozen at the SESSION level (first gate call)
        // and injected into the FIRST user message. They must NOT be reset
        // here — a per-turn rebuild would move the [Environment] block to the
        // newest user message and break the prefix cache at turn-1's message.
        qaqh_workspace::set_cancel(false);

        qaqh_workspace::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        if ctx.agent.config.compliance_enabled {
            if let Err(reason) = qaqh_gate::guard::content_guard(&text) {
                log::info!("[INPUT] compliance blocked: {reason}");
                // Ringing 双发：OperationFailed（Control 频道错误终态）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::OperationFailed {
                        occurrence_id: format!(
                            "op-failed-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0),
                        ),
                        scope: qaqh_domain::ErrorScope::Control,
                        error: qaqh_domain::DomainError {
                            error_id: format!(
                                "compliance-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0),
                            ),
                            code: "compliance_block".into(),
                            message: reason,
                            retryable: false,
                            dedupe_key: Some("compliance_block".into()),
                        },
                        operation_id: None,
                    },
                ));
                return Outcome::Handled;
            }
        }

        ctx.agent.activate_explicit_skills(&text);

        {
            let workspace = qaqh_workspace::CURRENT_WORKSPACE
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let status = ctx.agent.build_skills_status(&workspace);
            // Ringing 双发：SkillsUpdated（skill 目录/激活状态）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
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

        log::info!("[INPUT] pushing user message via ContextFlow (source={source_id})");
        let turn_id = ctx.agent.msg.allocate_turn_id();
        let receipt = ctx.flow.ingest(
            &mut ctx.agent.msg,
            source_id,
            qaqh_types::Message::user(&text),
        );
        if !receipt.stored {
            log::warn!(
                "[INPUT] ContextFlow refused user message (source={source_id}): trace={:?}",
                ctx.flow.trace().back()
            );
        }

        // Add image blocks to the user message and register them globally
        // so image can look them up by index.
        for img in &images {
            ctx.agent
                .msg
                .push_image_to_last_user(&img.mime_type, &img.data);
            qaqh_workspace::image_query::store_image(
                &ctx.agent.session.seed,
                &img.mime_type,
                &img.data,
            );
        }
        log::info!("[INPUT] flushing meta");
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

        log::info!("[INPUT] emitting TurnStart turn_id={} round_num=0", turn_id);
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::TurnOpened {
                turn_id: turn_id.clone(),
                user_text: text.clone(),
            });
        // Ringing 双发：TurnStarted（权威开始事件）
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    user_text: text,
                },
            ));

        Outcome::ContinueTurn {
            turn_id,
            round_num: 0,
            usage: None,
        }
    }

    /// Handle a system-level injection (e.g. sub-agent result). Same turn
    /// lifecycle as user input, but the message is stored with `role=system`
    /// and user-input-only processing is skipped: compliance guard, explicit
    /// skill activation and todo/goal mode switches do not apply to injected
    /// data. Callers must keep the `[SUBAGENT ...]` tag in `text` so the model
    /// can distinguish injected content from system instructions.
    ///
    /// `command_id` is the durable injection-journal key: once the message is
    /// committed to the store (drain below), the journal entry is marked
    /// committed so a crash/replay cannot re-inject it.
    pub fn handle_system_input(
        &self,
        ctx: &mut RingContext,
        text: &str,
        command_id: Option<&str>,
    ) -> Outcome {
        log::info!(
            "[INPUT] handle_system_input called, text_len={}",
            text.len()
        );
        // Auto-create session on first input (same as user path).
        if ctx.agent.session.seed.is_empty() {
            log::info!("[INPUT] auto-creating session on system injection");
            crate::state::lifecycle::create_session(ctx.agent);
            ctx.agent.rebind_store();
            ctx.emitter.set_seed(&ctx.agent.session.seed);
        }

        ctx.cancel.clear();
        qaqh_workspace::set_cancel(false);
        qaqh_workspace::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        log::info!("[INPUT] pushing system injection via ContextFlow (subagent source)");
        let turn_id = ctx.agent.msg.allocate_turn_id();
        // 统一管道：submit → drain（idle 时立即落盘 trailing）
        // ——与回合中的 lap 边界消费同一代码路径，不再分叉。
        // 角色统一为 user + name=subagent（与 busy 见缝插针/崩溃恢复路径一致）：
        // chat 协议下 developer 降级为中段 system 感知弱化，user 是两种协议
        // 的对话流主体，可见性有保证；name 标记供前端/回放区分真实用户输入。
        let msg = qaqh_types::Message {
            msg_id: None,
            role: qaqh_types::Message::ROLE_USER.into(),
            name: Some("subagent".into()),
            content: vec![qaqh_types::ContentBlock::text(text)],
        };
        if let Err(e) = ctx.flow.submit(
            qaqh_message::builtin::SUBAGENT,
            msg,
            command_id.map(String::from),
        ) {
            log::error!("[INPUT] ContextFlow submit failed for system injection: {e}");
        }
        {
            let model = ctx.agent.config.model.clone();
            let effort = ctx.agent.config.reasoning_effort.clone();
            // 注入落盘到 messages.jsonl 即成 history（不随崩溃/重启重放）；
            // 注入日志（injections.jsonl）已退役（PLAN B1），无需 committed
            // 标记。drain_turn_boundary 消费 pending 并落盘 trailing。
            let _ = ctx
                .flow
                .drain_turn_boundary(&mut ctx.agent.msg, &model, &effort);
        }
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

        log::info!("[INPUT] emitting TurnStart turn_id={} round_num=0", turn_id);
        // 注入回合的用户可见文本**只保留 [SUBAGENT ...] 标签行**：
        // 正文仅进模型消息流（push_system_input 已全文落盘，build_context
        // 从消息流取），不再进入前端 timeline/事件——否则注入正文会以
        // user 身份泄露到前端聊天流（前端把 TurnOpened.user_text 渲染为
        // 用户气泡）。前端 parse_subagent_injection 只需标签行即可收敛
        // 子代理状态区，语义不变。
        let label = text.lines().next().unwrap_or(text).to_string();
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::TurnOpened {
                turn_id: turn_id.clone(),
                user_text: label.clone(),
            });
        // Ringing 双发：TurnStarted（权威开始事件）
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    user_text: label,
                },
            ));

        Outcome::ContinueTurn {
            turn_id,
            round_num: 0,
            usage: None,
        }
    }
}
