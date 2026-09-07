//! agent::loop::dispatch_control — Control 命令分派（会话/技能/交互/模式）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::loop_core::Loop;
use super::types::*;

use qaqh_domain::{ControlCommand, DomainEvent};

impl Loop {
    pub(super) fn on_control(
        &mut self,
        command: ControlCommand,
        command_id: &str,
        expected_revision: u64,
    ) {
        match command {
            ControlCommand::SessionCreate {
                close_current: _,
                cwd: _,
                tool_mode,
                custom_tools,
            } => {
                // 可选工具模式预置在 create 前应用，保证新会话的
                // system prompt 与工具集首轮就位（daemon 路径已先落盘）。
                if let Some(tool_mode) = tool_mode {
                    self.session
                        .agent
                        .apply_tool_mode(&tool_mode, &custom_tools);
                }
                // H2：无论 close_current 与否都必须走会话切换守卫——
                // 否则旧会话的挂起 turn/tool.pending 会指向被整包替换
                // 后的死 store，迟到 resume 直接打穿新会话。
                self.prepare_session_switch();
                self.session_eng
                    .create(&mut self.session.agent, &self.cancel);
                self.sync_emitter_seed();
                self.paced_emitter.emit_domain(DomainEvent::Control(
                    qaqh_domain::ControlEvent::SessionStateChanged {
                        seed: self.session.agent.session.seed.clone(),
                        state: qaqh_domain::SessionState::Created,
                    },
                ));
                self.misc
                    .emit_dashboard(&self.session.agent, &self.paced_emitter);
            }
            ControlCommand::SessionResume { seed } => {
                self.prepare_session_switch();
                if self
                    .session_eng
                    .resume(&mut self.session.agent, &seed, &self.cancel)
                {
                    self.sync_emitter_seed();
                    self.paced_emitter.emit_domain(DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionStateChanged {
                            seed,
                            state: qaqh_domain::SessionState::Resumed,
                        },
                    ));
                } else {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Control,
                        "session_resume_failed",
                        "session could not be resumed",
                    );
                }
            }
            // daemon 拦截层已处理（仅 lease attach）；到达 actor 即异常路径，
            // 静默完成即可 —— 绝不能走到 resume/重建（会杀死运行中的子代理回合）。
            ControlCommand::SessionAttach { .. } => {
                log::debug!("SessionAttach is handled by the daemon lease layer");
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SessionShutdown => {
                self.pending.shutdown = true;
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::AgentReloadConfig => {
                self.session_eng
                    .reload_config(&mut self.session.agent, &self.cancel);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SetToolMode {
                tool_mode,
                custom_tools,
            } => {
                self.session
                    .agent
                    .apply_tool_mode(&tool_mode, &custom_tools);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SkillsReload => self.emit_ringing_skills_status(),
            ControlCommand::SkillsActivate { name } => {
                let _ = self.session.agent.skills.queue_request(&name, "user");
                self.emit_ringing_skills_status();
            }
            ControlCommand::SkillsOperation {
                operation_id,
                action,
                name,
            } => {
                let (success, _revision, error) = self.session.agent.skills.apply_ui_operation(
                    &operation_id,
                    expected_revision,
                    &action,
                    &name,
                );
                self.emit_ringing_skills_status();
                if !success {
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Control(
                            qaqh_domain::ControlEvent::OperationFailed {
                                occurrence_id: operation_id.clone(),
                                scope: qaqh_domain::ErrorScope::Control,
                                error: qaqh_domain::DomainError {
                                    error_id: operation_id.clone(),
                                    code: "skill_operation_failed".into(),
                                    message: error
                                        .unwrap_or_else(|| "skill operation failed".into()),
                                    retryable: false,
                                    dedupe_key: Some(operation_id.clone()),
                                },
                                operation_id: Some(operation_id),
                            },
                        ));
                }
            }
            ControlCommand::SessionClose { .. } => {
                log::debug!("SessionClose is handled by daemon registry");
            }
            ControlCommand::SessionArchive { .. } => {
                log::debug!("SessionArchive is handled by daemon registry");
            }
            ControlCommand::SessionUnarchive { .. } => {
                log::debug!("SessionUnarchive is handled by daemon registry");
            }
            ControlCommand::SessionDelete { .. } => {
                log::debug!("SessionDelete is handled by daemon registry");
            }
            ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            } => {
                // answers 已是 domain AskAnswer（Ringing 命令直接携带）。
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
                let outcome = self.session.turn.handle_ask_response(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                    &answers,
                );
                let _ = ctx;
                self.apply_outcome(outcome);
            }
            ControlCommand::InteractionAskDismiss { interaction_id } => {
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
                let outcome = self.session.turn.handle_ask_dismiss(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                );
                let _ = ctx;
                self.apply_outcome(outcome);
            }
            ControlCommand::PlanReviewRespond {
                interaction_id,
                approved,
                message,
                autonomous,
            } => {
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
                let outcome = self.session.turn.handle_plan_response(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                    approved,
                    &message.unwrap_or_default(),
                    autonomous,
                );
                let _ = ctx;
                self.apply_outcome(outcome);
            }
        }
    }
}
