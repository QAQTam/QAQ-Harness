//! agent::loop::injection — 注入通道（as_system 注入/injection_bus/compact 协同）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::injection::{
    EnqueueResult, Injection, InjectionPriority, InjectionSemantics, SUBAGENT_SOURCE,
};
use super::loop_core::Loop;
use super::loop_core::parse_subagent_status_tag;
use super::types::*;

impl Loop {
    // ═══════════════════════════════════════════════════
    // Interrupt polling (called by engines during long ops)
    // ═══════════════════════════════════════════════════

    /// 见缝插针注入消费者（回合 lap 边界调用）：从 cmd_rx 吸收排队的
    /// `as_system` 注入（子代理报告等）到 InjectionBus，由调用方随后在
    /// lap 边界交给 ContextFlow 落盘 trailing。
    ///
    /// 时机保证（PLAN-FIX-INJECTION-CACHE ②）：只在工具回合完成后的
    /// lap 边界被调用——此时本轮 tool_call 与其 tool_result 均已提交
    /// （工具执行是同步阻塞的），注入取号必然排在本轮全部结果之后，绝不
    /// 夹在 assistant(toolcall) 与其 tool_result 之间。注入以 user +
    /// name=subagent 角色落盘（chat/responses 两协议的对话流主体，可见性
    /// 保证）。
    pub(super) fn is_injection_command(cmd: &super::types::WorkerCommand) -> bool {
        matches!(
            &cmd.frame,
            env
                if matches!(
                    &env.command,
                    qaqh_ringing::RingingCommand::Conversation(
                        qaqh_domain::ConversationCommand::ConversationSendMessage {
                            as_system: true,
                            ..
                        }
                    )
                )
        )
    }

    /// Drop all pending injections when the active session is replaced.
    /// Compact's legacy deferred queue is filtered here as well; otherwise a
    /// report accepted before the switch could be dispatched into the new
    /// session after compact finishes.
    pub(super) fn clear_injections(&mut self) {
        self.injection_bus.clear();
        let before = self.deferred_ringing.len();
        self.deferred_ringing
            .retain(|cmd| !Self::is_injection_command(cmd));
        let dropped = before.saturating_sub(self.deferred_ringing.len());
        if dropped > 0 {
            log::info!("[INJECT] dropped {dropped} deferred injection(s) on session switch");
        }
    }

    pub(super) fn injection_session_matches(&self, command_session_id: &str) -> bool {
        let current_session_id = &self.session.agent.session.seed;
        current_session_id.is_empty()
            || command_session_id.is_empty()
            || command_session_id == current_session_id
    }

    /// Emit the subagent terminal-state tag event after a successful enqueue.
    /// Only `SUBAGENT_SOURCE` injections carry the tag contract; future
    /// sources may reuse this hook with their own event vocabulary.
    pub(super) fn emit_subagent_status(&self, session_id: &str, source: &str, text: &str) {
        if source != SUBAGENT_SOURCE {
            return;
        }
        if let Some((name, state)) = parse_subagent_status_tag(text) {
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::SubagentStatus {
                        seed: session_id.to_string(),
                        name,
                        state,
                    },
                ));
        }
    }

    /// 吸收注入到总线（busy 路径）：session 作用域校验 + command_id 幂等。
    /// 入队成功后立即发射 subagent 状态标签事件（保持现有时机）。
    pub(super) fn absorb_injection(&mut self, mut injection: Injection) {
        if !self.injection_session_matches(&injection.session_id) {
            log::warn!(
                "[INJECT] rejected injection for stale session (command={}, current={})",
                injection.session_id,
                self.session.agent.session.seed
            );
            return;
        }
        let session_id = self.session.agent.session.seed.clone();
        if session_id.is_empty() {
            log::warn!(
                "[INJECT] rejected injection without an active session (command_id={})",
                injection.command_id
            );
            return;
        }
        let text_len = injection.text.len();
        let command_id = injection.command_id.clone();
        let text = injection.text.clone();
        let source = injection.source;
        // 总线以当前 session 为作用域（command 携带的 session 仅用于陈旧性校验）。
        injection.session_id = session_id.clone();
        self.injection_bus.switch_session(&session_id);
        match self.injection_bus.enqueue(injection) {
            EnqueueResult::Queued => {
                log::info!(
                    "[INJECT] injection queued via InjectionBus (seed={}, text_len={}, pending={})",
                    session_id,
                    text_len,
                    self.injection_bus.pending_len()
                );
                // Absorbed injections do not create a turn of their own, so
                // keep the existing lightweight tracker convergence signal.
                self.emit_subagent_status(&session_id, source, &text);
            }
            EnqueueResult::DuplicateCommandId => {
                log::info!(
                    "[INJECT] duplicate injection command ignored (seed={}, command_id={})",
                    session_id,
                    command_id
                );
            }
            EnqueueResult::StaleSession => {
                log::warn!(
                    "[INJECT] rejected injection for stale/empty session (seed={}, command_id={})",
                    session_id,
                    command_id
                );
            }
        }
    }

    /// Keep the idle path's existing immediate turn semantics while using the
    /// bus to claim the command id exactly once for this session.
    pub(super) fn claim_injection(&mut self, injection: &Injection, command_id: &str) -> bool {
        if self.session.agent.session.seed.is_empty() {
            return true;
        }
        if !self.injection_session_matches(&injection.session_id) {
            log::warn!(
                "[INJECT] ignored idle injection for stale session (command={}, current={})",
                injection.session_id,
                self.session.agent.session.seed
            );
            return false;
        }
        let session_id = self.session.agent.session.seed.clone();
        self.injection_bus.switch_session(&session_id);
        let mut claimed = injection.clone();
        claimed.session_id = session_id.clone();
        match self.injection_bus.enqueue(claimed) {
            EnqueueResult::Queued => {
                self.emit_subagent_status(&session_id, injection.source, &injection.text);
                let _ = self.injection_bus.drain();
                true
            }
            EnqueueResult::DuplicateCommandId => {
                log::info!("[INJECT] duplicate idle injection command ignored: {command_id}");
                false
            }
            EnqueueResult::StaleSession => false,
        }
    }

    /// 统一注入入口（刀 7 第一阶段）：所有非用户命令的消息注入
    /// （subagent 报告，未来 system/MCP）都经此进入 Loop。
    ///
    /// 时机决策：
    /// - compact 进行中 → 入总线（priority 记为 Deferred），compact 完成后
    ///   idle 再逐条开新 turn（`dispatch_injections_after_compact`）；
    /// - turn 运行中（phase != Idle）→ 入总线，lap 边界由 `drain_injections`
    ///   落盘进当前 turn；
    /// - idle → 占 command_id 后立即经 `handle_system_input` 开新 turn。
    ///
    /// 会话作用域校验（stale/空 session → 拒绝 + 日志）与 command_id 幂等
    /// 均由总线承担。返回 Some(outcome) 表示已开 turn（由调用方
    /// apply_outcome），None 表示入队等待或被拒绝。
    pub fn inject(&mut self, injection: Injection) -> Option<Outcome> {
        let command_id = injection.command_id.clone();
        let text = injection.text.clone();

        if self.session.agent.manual_compact_running() {
            let mut deferred = injection;
            deferred.priority = InjectionPriority::Deferred;
            self.absorb_injection(deferred);
            return None;
        }

        match self.phase {
            LoopPhase::Idle => {
                if !self.claim_injection(&injection, &command_id) {
                    return None;
                }
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
                // 进入该注入命令的 causation 作用域：开 turn 期间发射的事件
                // 必须归属到注入者的 command_id（与 dispatch_deferred_ringing /
                // 其它单命令派发路径一致）。
                let _scope = self
                    .paced_emitter
                    .enter_causation(Some(command_id.as_str()));
                let outcome =
                    self.input
                        .handle_system_input(&mut ctx, &text, Some(command_id.as_str()));
                let _ = ctx;
                Some(outcome)
            }
            _ => {
                self.absorb_injection(injection);
                None
            }
        }
    }

    /// Hand bus records to ContextFlow only at a lap boundary. ContextFlow
    /// remains responsible for the actual store write and write ordering.
    pub(super) fn drain_injections(&mut self) {
        let session_id = self.session.agent.session.seed.clone();
        self.injection_bus.switch_session(&session_id);
        let records = self.injection_bus.drain();
        if records.is_empty() {
            return;
        }

        let mut submitted = 0;
        for record in records {
            if record.session_id != session_id {
                log::warn!(
                    "[INJECT] skipped injection from stale session (record={}, current={})",
                    record.session_id,
                    session_id
                );
                continue;
            }
            let message = record.message();
            let command_id = record.command_id;
            match self
                .flow
                .submit(qaqh_message::builtin::SUBAGENT, message, Some(command_id))
            {
                Ok(()) => submitted += 1,
                Err(e) => log::error!("[INJECT] ContextFlow submit failed: {e}"),
            }
        }
        if submitted == 0 {
            return;
        }

        let model = self.session.agent.config.model.clone();
        let effort = self.session.agent.config.reasoning_effort.clone();
        let (drained, _) =
            self.flow
                .drain_turn_boundary(&mut self.session.agent.msg, &model, &effort);
        if drained > 0 {
            log::info!("[INJECT] lap boundary drained {drained} injection(s) via ContextFlow");
        }
    }

    pub fn drain_pending_injections(&mut self) {
        use qaqh_domain::ConversationCommand;
        use qaqh_ringing::RingingCommand;
        self.injection_bus
            .switch_session(&self.session.agent.session.seed);
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let env = cmd.frame;
            match &env.command {
                // ── 注入命令：as_system 消息（子代理报告等）──────────
                // 统一走 Loop::inject()（turn 运行中 → 入总线，lap 边界
                // 再由 drain_injections 交给 ContextFlow）。
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text,
                    as_system: true,
                    ..
                }) => {
                    let injection = Injection {
                        session_id: env.seed.clone(),
                        command_id: env.command_id.clone(),
                        source: SUBAGENT_SOURCE,
                        role: qaqh_types::Message::ROLE_USER,
                        text: text.clone(),
                        priority: InjectionPriority::Normal,
                        semantics: InjectionSemantics::NextTurn,
                    };
                    let _ = self.inject(injection);
                }
                // ── 其它命令（用户消息/非注入）：绝不丢弃 ────────────────
                // daemon 侧已 ACK（accepted），静默丢弃会让调用方永久悬挂。
                // 放入 deferred_ringing，主循环 idle 时按 FIFO 派发（复用
                // 既有 session 切换保留队列，语义一致）。
                _ => {
                    self.deferred_ringing
                        .push_back(super::types::WorkerCommand {
                            frame: env,
                            causation: cmd.causation,
                        });
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════
}
