//! Loop core — worker 进程内的单会话事件驱动循环（Ringing V1 架构）。
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │  Loop（worker 进程，单会话）                          │
//! │  ├─ I/O: cmd_rx（reader 线程 stdin → JSON-LP）       │
//! │  │        event_tx（writer 线程 → stdout，2ms 批量）  │
//! │  ├─ Signal: cancel, phase, pending, writer_dead      │
//! │  ├─ Session: session (SessionBundle)                 │
//! │  │   ├─ agent: AgentState                            │
//! │  │   ├─ stats: StatsCollector                        │
//! │  │   ├─ turn: TurnEngine                             │
//! │  │   └─ tool: ToolEngine                             │
//! │  ├─ Engines: session_eng, input, misc（compact 已去壳为自由函数）│
//! │  ├─ flow: ContextFlow（消息落盘/注入融合）            │
//! │  ├─ injection_bus: 注入总线（idle 直派 / busy 入队）  │
//! │  └─ paced_emitter: 事件节拍 + causation 作用域        │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Loop 是**单会话**的：一次进程只承载一个 `SessionBundle`（会话隔离的
//! 单位）。会话切换时整包落盘并替换。进程级状态（I/O 通道、cancel token）
//! 不受影响。命令经 `dispatch_ringing_one` 直接路由到各引擎方法（无独立
//! `Engine` trait）；中断类命令由 reader 线程直接置 cancel 以便立即生效。
//!
//! # Panic recovery
//!
//! 每次派发都包在 `safe_dispatch()` 里。若引擎 panic：
//! 1. 所有引擎重置到干净 idle 状态
//! 2. cancel token 清空
//! 3. 向 daemon 发射 `ControlEvent::OperationFailed`（legacy Agent2Ui 已拆除）
//! 4. Loop 继续处理后续命令
//!
//! # 新增命令
//!
//! 1. 若命令跨 wire：在 `qaqh-domain` / `qaqh-ringing` 增加对应变体
//! 2. 在 `dispatch_ringing_one` 路由到对应引擎方法
//! 3. 需要复位语义的在 `reset_all_engines()` 中登记
//!
//! # Ring flow
//!
//! ```text
//! UserInput → InputEngine.handle() → Outcome::ContinueTurn
//!   → TurnEngine.run()
//!     → Gate SSE → parse → admit_batch → execute → ContinueTurn
//!     → (loop until YieldToUser or TurnComplete)
//!   → Outcome::TurnComplete → TurnEnd + Done → Idle
//! ```

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use super::engine_compact::CompactMeta;
use super::engine_input::InputEngine;
use super::engine_misc::MiscEngine;
use super::engine_session::SessionEngine;
use super::injection::InjectionBus;
use super::paced_emitter::PacedEmitter;
use super::types::*;
use crate::agent::state::agent::AgentState;

pub fn ringing_command_is_interrupt(env: &qaqh_ringing::RingingWorkerCommandEnvelope) -> bool {
    matches!(
        &env.command,
        qaqh_ringing::RingingCommand::Control(
            qaqh_domain::ControlCommand::SessionResume { .. }
                | qaqh_domain::ControlCommand::SessionShutdown
                | qaqh_domain::ControlCommand::SessionCreate { .. }
        ) | qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { .. }
        )
    )
}

/// 解析注入文本首行的 `[SUBAGENT 'name' STATE]` 标签 → (name, state)。
/// 标签规范见 `crates/qaqh-subagent/src/lib.rs` collect 收尾（COMPLETED /
/// ERROR / TIMEOUT / CANCELLED 变体）。与前端 `parse_subagent_injection`
/// 保持同一格式约定；解析失败返回 None（静默，不阻断注入本身）。
pub(super) fn parse_subagent_status_tag(text: &str) -> Option<(String, String)> {
    let first = text.lines().next()?.trim_start();
    let rest = first.strip_prefix("[SUBAGENT '")?;
    let (name, rest) = rest.split_once("' ")?;
    let state = if rest.starts_with("COMPLETED]") {
        "COMPLETED"
    } else if rest.starts_with("ERROR") {
        "ERROR"
    } else if rest.starts_with("TIMEOUT") {
        "TIMEOUT"
    } else if rest.starts_with("CANCELLED]") {
        "CANCELLED"
    } else {
        return None;
    };
    Some((name.to_string(), state.to_string()))
}

// ═══════════════════════════════════════════════════════
// Loop — the dispatcher
// ═══════════════════════════════════════════════════════

/// Pre-created in-process worker channel ends.
///
/// An in-process host (daemon actor, tests) can create them before the loop,
/// keep the producer/consumer sides, and construct the loop later with
/// [`Loop::from_channels`].
pub struct LoopChannels {
    pub cmd_tx: mpsc::SyncSender<WorkerCommand>,
    pub cmd_rx: mpsc::Receiver<WorkerCommand>,
    pub event_tx: mpsc::SyncSender<WriterEvent>,
    pub event_rx: mpsc::Receiver<WriterEvent>,
    pub cancel: CancelToken,
    pub writer_dead: Arc<AtomicBool>,
}

impl Default for LoopChannels {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopChannels {
    /// Create the bounded channels used by a Ringing V1 loop.
    pub fn new() -> Self {
        // std 的 sync_channel 在构造时预分配 (capacity + 1) 个 slot 的环形缓冲，
        // 每个 slot 是 size_of::<WriterEvent>() = 512 字节（枚举按最大变体对齐）。
        // 旧值 655360 × 512B ≈ 320MB —— 每个 worker 进程启动即常驻，这正是
        // 单 session 内存 300MB+ 的根因。writer 线程逐事件即时写 stdout，突发
        // 事件由 PacedEmitter 以 ≤50ms 节流合并，16384 个 slot（8MB）在保留
        // 背压语义的同时把固定开销降到合理范围。
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WorkerCommand>(4096);
        let (event_tx, event_rx) = mpsc::sync_channel::<WriterEvent>(16384);
        Self {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel: CancelToken::new(),
            writer_dead: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub struct Loop {
    // ── Process-level I/O ──
    /// Incoming command channel (fed by reader thread).
    pub(super) cmd_rx: mpsc::Receiver<super::types::WorkerCommand>,
    /// Outgoing event channel (consumed by writer thread).
    pub(super) event_tx: mpsc::SyncSender<super::types::WriterEvent>,

    // ── Process-level signals ──
    /// Cancellation token shared across engines.
    pub(super) cancel: CancelToken,
    /// Current phase (Idle / GateRunning / ToolsRunning).
    pub(super) phase: LoopPhase,
    /// Deferred interrupt commands received while busy.
    pub(super) pending: PendingState,
    /// Ringing commands already acknowledged by the daemon while a legacy
    /// session switch is pending. An accepted command must execute exactly
    /// once after the switch; it must never be silently discarded.
    pub(super) deferred_ringing: VecDeque<super::types::WorkerCommand>,
    /// Set to true when the writer thread exits (stdout pipe broken).
    pub(super) writer_dead: Arc<AtomicBool>,
    /// Whether a `Ready` event has already been emitted for the current
    /// idle period. Prevents the 1 Hz `Ready` storm that flooded the
    /// daemon's Critical lane (each Ready is EventLane::Critical and was
    /// sent every loop iteration, saturating priority queues and tripping
    /// the connection-death cascade).
    pub(super) ready_emitted: bool,

    // ── Session-scoped state (flushed/swapped on session change) ──
    /// The active session's data and engines. Loop 单会话：进程只承载一个
    /// bundle，切换时整包落盘并替换。
    pub(super) session: SessionBundle,

    // ── Session-agnostic engines (process lifetime, no session state) ──
    /// Session lifecycle: create, resume, reload config.
    pub(super) session_eng: SessionEngine,
    /// User input handler: compliance guard, auto-create session.
    pub(super) input: InputEngine,
    /// Miscellaneous: undo, dashboard, mode.
    pub(super) misc: MiscEngine,
    /// Unified context-ingestion pipeline — the single door into the message
    /// store for every message source (user/model/tool/skills/subagent/goal).
    /// Registered with the built-in sources at construction; new sources
    /// (ACP/MCP loops) register here without touching the dispatcher.
    pub(super) flow: qaqh_message::ContextFlow,
    /// Busy-turn injections waiting for the next lap boundary.
    pub(super) injection_bus: InjectionBus,
    /// Pending compact result (set when compact is running in background).
    pub(super) pending_compact_rx: Option<mpsc::Receiver<CompactMeta>>,
    pub(super) pending_compact_id: Option<String>,
    pub(super) pending_compact_causation: Option<String>,

    /// Direct output emitter. The renderer performs frame-level coalescing.
    pub(super) paced_emitter: PacedEmitter,

    /// Idle-unload liveness signal shared with the daemon registry. The Loop
    /// is the producer (busy/activity/suspend), the registry is the consumer.
    pub(super) liveness: std::sync::Arc<super::liveness::WorkerLiveness>,
}

impl Loop {
    pub fn from_channels(
        agent: AgentState,
        cmd_rx: mpsc::Receiver<WorkerCommand>,
        event_tx: mpsc::SyncSender<WriterEvent>,
        cancel: CancelToken,
        writer_dead: Arc<AtomicBool>,
        liveness: std::sync::Arc<super::liveness::WorkerLiveness>,
    ) -> Self {
        // resume 模式下 `--resume-seed` 只写入 resume_seed 字段，seed 此时
        // 仍为空；用 resume_seed 兜底，避免 PacedEmitter 以空 seed 构造
        // （Ringing 事件信封会被 daemon 按 seed 过滤丢弃）。init_session
        // 完成后还会经 sync_emitter_seed 再次同步权威值。
        let seed = if !agent.session.seed.is_empty() {
            agent.session.seed.clone()
        } else {
            agent.session.resume_seed.clone().unwrap_or_default()
        };
        let paced_emitter = PacedEmitter::new(seed, event_tx.clone(), writer_dead.clone());

        let mut flow = qaqh_message::ContextFlow::new();
        qaqh_message::builtin::register_all(&mut flow);

        Loop {
            cmd_rx,
            event_tx,
            cancel,
            phase: LoopPhase::Idle,
            pending: PendingState::default(),
            deferred_ringing: VecDeque::new(),
            writer_dead,
            ready_emitted: false,
            session: SessionBundle::new(agent),
            session_eng: SessionEngine::new(),
            input: InputEngine::new(),
            misc: MiscEngine::new(),
            flow,
            injection_bus: InjectionBus::new(),
            pending_compact_rx: None,
            pending_compact_id: None,
            pending_compact_causation: None,
            paced_emitter,
            liveness,
        }
    }

    // ── Convenience accessors ──

    // ═══════════════════════════════════════════════════
    // Panic recovery
    // ═══════════════════════════════════════════════════

    /// Execute a closure with panic recovery.
    ///
    /// If `f` panics:
    /// 1. All engines are reset to clean idle state
    /// 2. Cancel token is cleared
    /// 3. Phase is reset to Idle
    /// 4. A `ControlEvent::OperationFailed` is emitted to the daemon
    ///
    /// The Loop continues processing commands after recovery.
    fn safe_dispatch<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Self) + std::panic::UnwindSafe,
    {
        // Idle-unload liveness: this dispatch counts as activity; the registry
        // must never unload while it is running.
        self.liveness.set_busy(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(self);
        }));

        if let Err(e) = result {
            let msg = Self::panic_msg_from_err(e);
            log::error!("[AGENT] engine panic during dispatch: {msg}");
            eprintln!("[qaqh AGENT] engine panic during dispatch: {msg}");

            self.reset_all_engines();
            self.phase = LoopPhase::Idle;
            self.cancel.clear();
            qaqh_workspace::clear_cancel();
            // L1: a panic must not widen the persistence loss window. Ops
            // enqueued before the panic are complete PersistOps — applying
            // them here keeps the archive at the last coherent round boundary.
            self.session.agent.drain_persist_ops();

            // panic 恢复：Ringing 侧以 OperationFailed 暴露（legacy Error/Done 已拆除）。
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::OperationFailed {
                        occurrence_id: format!(
                            "occ-panic-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0)
                        ),
                        scope: qaqh_domain::ErrorScope::System,
                        error: qaqh_domain::DomainError {
                            error_id: format!(
                                "panic-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0)
                            ),
                            code: "engine_panic_recovered".into(),
                            message: format!("Internal error (recovered): {msg}"),
                            retryable: false,
                            dedupe_key: None,
                        },
                        operation_id: None,
                    },
                ));
        }

        // Liveness bookkeeping runs on both the success and panic-recovery
        // paths: a completed dispatch is activity, and a suspended turn
        // (unresolved ask / permission / plan) blocks idle unload.
        self.liveness.set_busy(false);
        self.liveness.touch();
        self.liveness
            .set_suspend_pending(self.session.turn.is_suspended());
    }

    /// Reset all engines to clean idle state.
    ///
    /// Called after a panic or on Cancel.
    /// Session-level engines are reset (turn, tool) to clear any
    /// suspended state or pending approvals. Stateless engines are
    /// no-ops. Stats accumulator is replaced with a fresh one.
    pub(super) fn reset_all_engines(&mut self) {
        // Session-level engines (hold mutable state)
        self.session.turn.reset();
        self.session.tool.clear_pending();
        self.session.stats = StatsCollector::new();

        // Session-agnostic engines：无状态（M3 后无 Engine trait reset）
        self.misc.reset();
        self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);

        self.pending.clear();
    }

    /// Close any suspended transaction before replacing the active session.
    /// An unanswered ask/tool round must never be persisted into, or resumed
    /// against, the next session.
    pub(super) fn prepare_session_switch(&mut self) {
        self.clear_injections();
        self.session.agent.reset_compaction_coordination();
        if self.session.turn.is_suspended() {
            self.session.agent.msg.remove_last_step_if_incomplete();
        }
        self.session.flush();
        self.reset_all_engines();
        self.cancel.clear();
        qaqh_workspace::clear_cancel();
    }

    /// 将会话 seed 同步到 PacedEmitter（Ringing 事件信封路由键）。
    /// 必须在任何会话创建/恢复（含 auto-create）之后、后续 emit_domain
    /// 之前调用；否则事件携带旧/空 seed，被 daemon SSE 的 owns_seed
    /// 过滤丢弃，前端收不到流式输出。
    pub(super) fn sync_emitter_seed(&mut self) {
        let seed = self.session.agent.session.seed.clone();
        self.paced_emitter.set_seed(&seed);
        self.injection_bus.switch_session(&seed);
    }

    /// Extract a human-readable message from a panic payload.
    pub(super) fn panic_msg_from_err(e: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = e.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".into()
        }
    }

    // ═══════════════════════════════════════════════════
    // Main event loop
    // ═══════════════════════════════════════════════════

    /// Run the main event loop. Blocks until shutdown or pipe break.
    ///
    /// # Lifecycle
    ///
    /// 1. **Init**: auto-create or resume session from CLI seed
    /// 2. **Loop**: drain pending → block for command → dispatch → repeat
    /// 3. **Exit**: flush session, shutdown tools
    ///
    /// # Cancellation
    ///
    /// The reader thread sets `cancel` on interrupt-type commands BEFORE
    /// they reach the channel. This means long-running operations (Gate
    /// SSE, tool execution) see the cancellation immediately via
    /// `cancel.is_set()` polling.
    pub fn run(&mut self) {
        // ── Init: handle pre-set seed from CLI ──
        self.init_session();

        log::info!("[AGENT] entering main event loop");
        loop {
            // ── Process queued interrupts ──
            self.drain_pending();

            if self.pending.shutdown {
                break;
            }

            if self.writer_dead.load(Ordering::SeqCst) {
                self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                log::error!("[AGENT] writer thread died — exiting");
                eprintln!("[qaqh AGENT] writer thread died — stdout pipe broken. Exiting.");
                break;
            }

            // ── Check background compact completion ──
            self.check_pending_compact();

            // Signal readiness at most once per truly idle period. A manual
            // compact runs in a background worker, but it still owns the
            // active context transaction until CompactEnd is applied.
            if self.pending_compact_rx.is_none() && !self.ready_emitted {
                self.ready_emitted = true;
            }

            // ── Block for next command (with timeout to poll compact) ──
            let cmd = match self.cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(f) => {
                    log::info!(
                        "[AGENT] received worker command frame: seed={} cmd={}",
                        f.frame.seed,
                        f.frame.command_id
                    );
                    f
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Compact polling path can also enqueue persist ops
                    // (finish_manual_compact → flush_meta); drain before idling.
                    self.session.agent.drain_persist_ops();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                    log::error!("[AGENT] cmd_rx closed — stdin pipe broken. Exiting.");
                    eprintln!("[qaqh AGENT] stdin pipe broken — exiting.");
                    break;
                }
            };

            // ── Dispatch with panic safety ──
            let causation = cmd.causation.clone();
            self.safe_dispatch(|this| {
                let _scope = this.paced_emitter.enter_causation(causation.as_deref());
                let env = cmd.frame;
                this.dispatch_ringing_one(env);
                // PR-1-6: flush queued persistence ops after the command's
                // dispatch completes (write order = enqueue order, Z5).
                this.session.agent.drain_persist_ops();
            });
        }

        // ── Cleanup ──
        qaqh_workspace::runtime::shutdown_tools();
        self.session.flush();
        // Final drain: SessionBundle::flush enqueues a flush_meta op; the old
        // synchronous path wrote it before exiting (PR-1-6).
        self.session.agent.drain_persist_ops();
    }

    /// Initialize session state from pre-set seed (CLI args --seed / --resume-seed).
    fn init_session(&mut self) {
        let resume_seed = self.session.agent.session.resume_seed.take();
        let has_seed = !self.session.agent.session.seed.is_empty();

        if let Some(seed) = resume_seed {
            if self
                .session_eng
                .resume(&mut self.session.agent, &seed, &self.cancel)
            {
                // init_session 已把 agent.session.seed 设为权威值（恢复成功
                // 为原 seed，fallback 为新 seed）；此后 Ringing 事件必须携带它。
                self.sync_emitter_seed();
                // legacy SessionRestored 已退役：Ringing 恢复由 daemon bootstrap 快照承担。
            }
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
        } else if has_seed && !self.session.agent.session.from_resume {
            self.session_eng
                .create_with_seed(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            let seed = self.session.agent.session.seed.clone();
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::SessionStateChanged {
                        seed: seed.clone(),
                        state: qaqh_domain::SessionState::Created,
                    },
                ));
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
        } else {
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
        }

        // 崩溃恢复不再重放注入日志（PLAN B1）：注入一旦落盘到 messages.jsonl
        // 即成 history，由 from_messages 按原写入位置恢复；未落盘的崩溃窗口
        // 注入静默丢弃（从未进入任何请求，无事实损失）。
    }

    // ═══════════════════════════════════════════════════
    // Pending queue drain
    // ═══════════════════════════════════════════════════

    /// Process all queued commands from the channel.
    ///
    /// Interrupt-type commands (Cancel, ResumeSession, NewSession, Shutdown)
    /// set the cancel token and queue a pending action. Ringing commands have
    /// already been acknowledged by the daemon, so commands received during a
    /// session switch are retained and dispatched once the switch completes.
    fn drain_pending(&mut self) {
        self.dispatch_deferred_ringing();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let env = cmd.frame;
            if self.pending.is_empty() {
                let causation = cmd.causation.clone();
                let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                self.dispatch_ringing_one(env);
            } else {
                self.deferred_ringing
                    .push_back(super::types::WorkerCommand {
                        frame: env,
                        causation: cmd.causation,
                    });
            }
        }

        self.dispatch_deferred_ringing();
    }

    /// Dispatch accepted Ringing commands in FIFO order once no session switch
    /// is pending. Stop as soon as a deferred command schedules another switch;
    /// later commands remain queued for the next drain.
    fn dispatch_deferred_ringing(&mut self) {
        while self.pending.is_empty() {
            let Some(cmd) = self.deferred_ringing.pop_front() else {
                break;
            };
            let env = cmd.frame;
            let _scope = self.paced_emitter.enter_causation(cmd.causation.as_deref());
            self.dispatch_ringing_one(env);
            // PR-1-6: per-command drain, same as the safe_dispatch path.
            self.session.agent.drain_persist_ops();
        }
    }

    // ═══════════════════════════════════════════════════
    // Command router — arms live in loop_dispatch_* (Phase 2-5)
    // ═══════════════════════════════════════════════════

    fn dispatch_ringing_one(&mut self, env: qaqh_ringing::RingingWorkerCommandEnvelope) {
        use qaqh_ringing::RingingCommand;

        self.ready_emitted = false;
        let expected_revision = env.expected_revision.unwrap_or_default();
        let command_id = env.command_id.clone();
        let command_session_id = env.seed.clone();

        match env.command {
            RingingCommand::Control(command) => {
                self.on_control(command, &command_id, expected_revision);
            }
            RingingCommand::Conversation(command) => {
                self.on_conversation(command, &command_id, &command_session_id);
            }
            RingingCommand::Tool(command) => {
                self.on_tool(command, &command_id);
            }
        }
    }
}

#[cfg(test)]
mod parse_subagent_status_tag_tests {
    use super::parse_subagent_status_tag;

    #[test]
    fn parses_all_terminal_tags() {
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'explore' COMPLETED]\n\nfinal answer"),
            Some(("explore".to_string(), "COMPLETED".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' ERROR exit=1]"),
            Some(("x".to_string(), "ERROR".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' TIMEOUT after 120s]"),
            Some(("x".to_string(), "TIMEOUT".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' CANCELLED]"),
            Some(("x".to_string(), "CANCELLED".to_string()))
        );
    }

    #[test]
    fn rejects_non_injection_text() {
        assert_eq!(parse_subagent_status_tag("normal user message"), None);
        assert_eq!(parse_subagent_status_tag("[SUBAGENT 'x' RUNNING]"), None);
        assert_eq!(parse_subagent_status_tag(""), None);
        // 标签不在首行（注入文本规范要求首行即标签）→ 不匹配。
        assert_eq!(
            parse_subagent_status_tag("some text\n[SUBAGENT 'x' COMPLETED]"),
            None
        );
    }
}
