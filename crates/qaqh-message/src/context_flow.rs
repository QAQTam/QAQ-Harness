//! ContextFlow — unified context-ingestion orchestration.
//!
//! Most messages entering the model context (user input, assistant output,
//! skills envelopes, subagent reports, goal-mode prompts)
//! flow through [`ContextFlow::ingest`]. Tool execution results are the
//! deliberate exception: they bypass the flow via
//! `MessageStore::push_tool_result_direct_with_attachments` (the admit layer
//! attaches attachments + permission metadata the flow never sees).
//!
//! # Roles
//!
//! Four open roles: `system`, `user`, `assistant`, `tool` — plus the
//! Responses-protocol-only `developer` role for runtime-injected
//! instructions. On Chat Completions providers `developer` downgrades to
//! `system` at the gate conversion layer (storage keeps the explicit role).
//!
//! # Extensibility
//!
//! New message sources (ACP/MCP loops, future subsystems) register a
//! [`ContextSource`] declaring role + sink + timing + visibility +
//! lifecycle, then call `ingest`/`submit`. No loop-core changes required.
//!
//! # Lifecycle
//!
//! [`LifecyclePolicy`] makes undo/compact behavior a property of the source
//! instead of hard-coded branch logic: trailing injections are `Preserved`
//! under compaction, turn messages are `Compressable`, and undo removes the
//! last turn only for sources that declare `RemoveLast`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use qaqh_types::{ContentBlock, Message};

use crate::store::MessageStore;

// ═══════════════════════════════════════════════════════
// Role — the open role set of the context flow
// ═══════════════════════════════════════════════════════

/// Message role in the context flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowRole {
    System,
    User,
    Assistant,
    Tool,
    /// Responses-protocol-only runtime injection role. Downgrades to
    /// `system` on Chat Completions providers (gate conversion layer).
    Developer,
}

impl FlowRole {
    pub fn as_str(self) -> &'static str {
        match self {
            FlowRole::System => "system",
            FlowRole::User => "user",
            FlowRole::Assistant => "assistant",
            FlowRole::Tool => "tool",
            FlowRole::Developer => "developer",
        }
    }

    /// Parse a stored message role string into the flow role set.
    pub fn from_message(msg: &Message) -> Option<Self> {
        match msg.role.as_str() {
            "system" => Some(FlowRole::System),
            "user" => Some(FlowRole::User),
            "assistant" => Some(FlowRole::Assistant),
            "tool" => Some(FlowRole::Tool),
            "developer" => Some(FlowRole::Developer),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════
// Sink / Timing / Visibility / Lifecycle
// ═══════════════════════════════════════════════════════

/// Where an ingested message lands in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Open a new turn (user / system / developer messages).
    Turn,
    /// Append an assistant step to the current turn.
    Step,
    /// Append to `trailing_messages` — position never moves, prefix cache stable.
    Trailing,
}

impl Sink {
    pub fn as_str(self) -> &'static str {
        match self {
            Sink::Turn => "turn",
            Sink::Step => "step",
            Sink::Trailing => "trailing",
        }
    }
}

/// When an ingested message is committed to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    /// Synchronous commit inside `ingest` (main-line user/model traffic).
    Immediate,
    /// Held in the pending queue until `drain_turn_boundary` runs at a lap
    /// boundary (runtime injections such as subagent reports).
    TurnBoundary,
}

/// Ingest visibility of a message: whether the flow accepts it into the
/// store at all. (Former `timeline`/`persist` flags were write-only — no
/// reader ever consulted them — and were removed in Phase 3-2.)
#[derive(Debug, Clone, Copy, Default)]
pub struct Visibility {
    /// Include in `build_context_for_gate` output.
    pub context: bool,
}

/// Undo behavior for messages from this source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoBehavior {
    /// The message (or its turn) is removed by undo.
    RemoveLast,
    /// The message survives undo.
    Keep,
}

/// Compaction behavior for messages from this source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactBehavior {
    /// Replaced by the compaction summary.
    Compressable,
    /// Preserved verbatim across compaction (trailing injections).
    Preserved,
}

/// Lifecycle semantics declared per source.
#[derive(Debug, Clone, Copy)]
pub struct LifecyclePolicy {
    pub undo: UndoBehavior,
    pub compact: CompactBehavior,
}

// ═══════════════════════════════════════════════════════
// ContextSource — the pluggable contract
// ═══════════════════════════════════════════════════════

/// A registered message source. Declarative: role, sink, timing, visibility,
/// lifecycle. New sources register here and never touch loop-core code.
pub trait ContextSource: Send + Sync {
    /// Stable source id, e.g. `"skills"`, `"subagent"`.
    fn id(&self) -> &'static str;
    fn role(&self) -> FlowRole;
    fn sink(&self) -> Sink;
    fn timing(&self) -> Timing;
    fn visibility(&self) -> Visibility;
    fn lifecycle(&self) -> LifecyclePolicy;
    /// Per-source idempotency key. The flow refuses to ingest a message whose
    /// key equals the last-ingested key for this source. `None` falls back to
    /// the store layer's text dedup (system/trailing sinks only).
    fn dedupe_key(&self, _msg: &Message) -> Option<String> {
        None
    }
}

// ═══════════════════════════════════════════════════════
// Pending / Trace / Receipt / Error
// ═══════════════════════════════════════════════════════

/// A submitted-but-not-yet-committed injection (TurnBoundary queue element).
pub struct PendingIngest {
    pub source_id: &'static str,
    pub msg: Message,
    /// 注入命令的 command_id（daemon 注入日志去重键）。drain 后由调用方
    /// 标记 journal committed；None 表示非注入来源（无 journal 语义）。
    pub command_id: Option<String>,
}

/// One diagnostic entry per ingest attempt. `ContextFlow::trace` keeps the
/// last N entries — a single log line closes the "was the injection seen by
/// the model" investigation loop end-to-end.
#[derive(Debug, Clone)]
pub struct IngestTraceEntry {
    pub at: Instant,
    pub source: &'static str,
    pub role: &'static str,
    pub sink: &'static str,
    /// `"pending" | "stored" | "deduped" | "skipped" | "rejected"`
    pub outcome: &'static str,
}

/// Outcome of a single `ingest` call.
#[derive(Debug, Clone, Default)]
pub struct IngestReceipt {
    pub stored: bool,
    pub deduped: bool,
    /// Store-layer decision for `Sink::Step` (assistant messages):
    /// `true` ends the turn, `false` means tools may run. Always `false`
    /// for every other sink.
    pub turn_completed: bool,
}

#[derive(Debug)]
pub enum FlowError {
    UnknownSource(&'static str),
    TimingMismatch(&'static str),
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowError::UnknownSource(id) => write!(f, "unknown context source: {id}"),
            FlowError::TimingMismatch(id) => write!(f, "source {id} is not TurnBoundary-timed"),
        }
    }
}

impl std::error::Error for FlowError {}

// ═══════════════════════════════════════════════════════
// ContextFlow — registry + ingest pipeline
// ═══════════════════════════════════════════════════════

const TRACE_CAP: usize = 64;

pub struct ContextFlow {
    sources: HashMap<&'static str, Arc<dyn ContextSource>>,
    pending: VecDeque<PendingIngest>,
    last_keys: HashMap<&'static str, String>,
    trace: VecDeque<IngestTraceEntry>,
}

impl ContextFlow {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            pending: VecDeque::new(),
            last_keys: HashMap::new(),
            trace: VecDeque::new(),
        }
    }

    /// Register a message source. Duplicate ids replace the previous entry.
    pub fn register(&mut self, source: Arc<dyn ContextSource>) {
        self.sources.insert(source.id(), source);
    }

    pub fn source(&self, id: &str) -> Option<&Arc<dyn ContextSource>> {
        self.sources.get(id)
    }

    /// Submit a TurnBoundary-timed injection while a turn is running. The
    /// message sits in the pending queue until `drain_turn_boundary` commits
    /// it at the next lap boundary — no mid-round mutation, no dropped
    /// command (non-injection commands must be handled by the caller).
    ///
    /// `command_id` is the durable-queue key: it is returned by
    /// [`Self::drain_turn_boundary`] once the message is committed to the
    /// store, so the caller can mark the injection journal entry as
    /// committed. `None` skips journal bookkeeping.
    pub fn submit(
        &mut self,
        source_id: &'static str,
        msg: Message,
        command_id: Option<String>,
    ) -> Result<(), FlowError> {
        let src = self
            .sources
            .get(source_id)
            .cloned()
            .ok_or(FlowError::UnknownSource(source_id))?;
        if src.timing() != Timing::TurnBoundary {
            return Err(FlowError::TimingMismatch(source_id));
        }
        self.trace_push(
            source_id,
            src.role().as_str(),
            src.sink().as_str(),
            "pending",
        );
        self.pending.push_back(PendingIngest {
            source_id,
            msg,
            command_id,
        });
        Ok(())
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Commit all pending injections at a lap boundary. Returns the number of
    /// messages drained plus the command_ids of the committed injections (in
    /// submit order) so the caller can mark the durable injection journal.
    /// Flushes the store meta afterwards.
    pub fn drain_turn_boundary(
        &mut self,
        store: &mut MessageStore,
        model: &str,
        effort: &str,
    ) -> (usize, Vec<String>) {
        let pending = std::mem::take(&mut self.pending);
        let n = pending.len();
        let mut command_ids = Vec::new();
        for p in pending {
            let receipt = self.ingest(store, p.source_id, p.msg);
            // G1：只有 stored/deduped 才回执 committed——被拒/跳过的注入若
            // 也标 committed，重试与崩溃重放都会被吞，注入即静默丢失。
            if (receipt.stored || receipt.deduped)
                && let Some(command_id) = p.command_id
            {
                command_ids.push(command_id);
            }
        }
        if n > 0 {
            store.flush_meta(model, effort);
        }
        (n, command_ids)
    }

    /// The single door into the message store. Routes the message by the
    /// source's declared role + sink; records a trace entry regardless of
    /// outcome.
    pub fn ingest(
        &mut self,
        store: &mut MessageStore,
        source_id: &'static str,
        msg: Message,
    ) -> IngestReceipt {
        let Some(src) = self.sources.get(source_id).cloned() else {
            self.trace_push(source_id, "?", "?", "rejected");
            return IngestReceipt::default();
        };
        let role = src.role();
        let sink = src.sink();
        let vis = src.visibility();

        if !role_sink_compatible(role, sink) {
            self.trace_push(source_id, role.as_str(), sink.as_str(), "rejected");
            return IngestReceipt::default();
        }
        if !vis.context {
            self.trace_push(source_id, role.as_str(), sink.as_str(), "skipped");
            return IngestReceipt::default();
        }
        // dedup 双层语义（M-doc+M-low② 统一后）：flow 层 last-key 仅阻断
        // 相邻重复；store 层 trailing 文本去重亦已收窄为仅相邻（原全历史
        // 扫描会吞掉间隔数小时重现的相同报告）。A,B,A 全链路放行，
        // 由 aba_pattern_passes_by_design 测试锁定。
        let pending_key = src.dedupe_key(&msg);
        if let Some(key) = &pending_key
            && self.last_keys.get(source_id) == Some(key)
        {
            self.trace_push(source_id, role.as_str(), sink.as_str(), "deduped");
            return IngestReceipt {
                stored: false,
                deduped: true,
                turn_completed: false,
            };
        }

        // (stored, turn_completed) — the flag carries the store-layer decision
        // for Sink::Step (assistant messages: false = tools may run, true = turn done).
        let (stored, turn_completed) = match sink {
            Sink::Turn => {
                let text = extract_text(&msg);
                match role {
                    FlowRole::User => {
                        store.push_user(&text);
                        (true, false)
                    }
                    FlowRole::System | FlowRole::Developer => {
                        store.push_system_input(&text);
                        (true, false)
                    }
                    _ => (false, false), // unreachable via role_sink_compatible
                }
            }
            Sink::Step => {
                let turn_completed = store.push_assistant(msg.clone());
                (true, turn_completed)
            }
            Sink::Trailing => (store.push_trailing_system(msg.clone()), false),
        };

        // G1：last_keys 在 store 确定性接受之后落账——原先先记账后落盘，
        // 失败路径会让合法重试被永久误判 dedup。
        if stored && let Some(key) = pending_key {
            self.last_keys.insert(source_id, key);
        }

        self.trace_push(
            source_id,
            role.as_str(),
            sink.as_str(),
            if stored { "stored" } else { "skipped" },
        );
        IngestReceipt {
            stored,
            deduped: false,
            turn_completed,
        }
    }

    /// Recent ingest diagnostics (oldest → newest).
    pub fn trace(&self) -> &VecDeque<IngestTraceEntry> {
        &self.trace
    }

    fn trace_push(
        &mut self,
        source: &'static str,
        role: &'static str,
        sink: &'static str,
        outcome: &'static str,
    ) {
        if self.trace.len() >= TRACE_CAP {
            self.trace.pop_front();
        }
        self.trace.push_back(IngestTraceEntry {
            at: Instant::now(),
            source,
            role,
            sink,
            outcome,
        });
    }
}

impl Default for ContextFlow {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════
// Role/sink compatibility
// ═══════════════════════════════════════════════════════

fn role_sink_compatible(role: FlowRole, sink: Sink) -> bool {
    match sink {
        Sink::Turn => matches!(
            role,
            FlowRole::User | FlowRole::System | FlowRole::Developer
        ),
        Sink::Step => role == FlowRole::Assistant,
        Sink::Trailing => matches!(
            role,
            FlowRole::System | FlowRole::Developer | FlowRole::User
        ),
    }
}

fn extract_text(msg: &Message) -> String {
    msg.content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════
// Built-in sources
// ═══════════════════════════════════════════════════════

pub mod builtin {
    use super::*;

    /// Stable ids of the built-in sources.
    pub const USER: &str = "user";
    pub const MODEL: &str = "model";
    pub const SKILLS: &str = "skills";
    pub const SUBAGENT: &str = "subagent";
    pub const GOAL: &str = "goal";

    fn base(
        id: &'static str,
        role: FlowRole,
        sink: Sink,
        timing: Timing,
        visibility: Visibility,
        lifecycle: LifecyclePolicy,
    ) -> Arc<dyn ContextSource> {
        struct S {
            id: &'static str,
            role: FlowRole,
            sink: Sink,
            timing: Timing,
            visibility: Visibility,
            lifecycle: LifecyclePolicy,
            dedupe: Option<&'static str>,
        }
        impl ContextSource for S {
            fn id(&self) -> &'static str {
                self.id
            }
            fn role(&self) -> FlowRole {
                self.role
            }
            fn sink(&self) -> Sink {
                self.sink
            }
            fn timing(&self) -> Timing {
                self.timing
            }
            fn visibility(&self) -> Visibility {
                self.visibility
            }
            fn lifecycle(&self) -> LifecyclePolicy {
                self.lifecycle
            }
            fn dedupe_key(&self, _msg: &Message) -> Option<String> {
                self.dedupe.map(str::to_string)
            }
        }
        Arc::new(S {
            id,
            role,
            sink,
            timing,
            visibility,
            lifecycle,
            dedupe: None,
        })
    }

    /// Human user input (wire `ConversationSendMessage`, `as_system=false`).
    pub fn user_source() -> Arc<dyn ContextSource> {
        base(
            USER,
            FlowRole::User,
            Sink::Turn,
            Timing::Immediate,
            Visibility { context: true },
            LifecyclePolicy {
                undo: UndoBehavior::RemoveLast,
                compact: CompactBehavior::Compressable,
            },
        )
    }

    /// Main-model output (gate response → assistant step).
    pub fn model_source() -> Arc<dyn ContextSource> {
        base(
            MODEL,
            FlowRole::Assistant,
            Sink::Step,
            Timing::Immediate,
            Visibility { context: true },
            LifecyclePolicy {
                undo: UndoBehavior::RemoveLast,
                compact: CompactBehavior::Compressable,
            },
        )
    }

    /// Skills activation-set envelope (runtime injection). Developer role:
    /// a `developer` item on Responses providers, downgraded to `system` on
    /// Chat Completions.
    pub fn skills_source() -> Arc<dyn ContextSource> {
        base(
            SKILLS,
            FlowRole::Developer,
            Sink::Trailing,
            Timing::Immediate,
            Visibility { context: true },
            LifecyclePolicy {
                undo: UndoBehavior::Keep,
                compact: CompactBehavior::Preserved,
            },
        )
    }

    /// Subagent report injection (`[SUBAGENT …]` via `as_system=true`).
    /// TurnBoundary timing: committed at the next lap boundary so the next
    /// model request sees it without interrupting the running round.
    /// User role (was Developer): chat 协议下 developer 降级为中段 system
    /// 感知弱化，user 是两种协议的对话流主体；name 标记区分真实用户输入。
    pub fn subagent_source() -> Arc<dyn ContextSource> {
        base(
            SUBAGENT,
            FlowRole::User,
            Sink::Trailing,
            Timing::TurnBoundary,
            Visibility { context: true },
            LifecyclePolicy {
                undo: UndoBehavior::Keep,
                compact: CompactBehavior::Preserved,
            },
        )
    }

    /// Goal-mode auto-advance prompt (user surrogate turn).
    pub fn goal_source() -> Arc<dyn ContextSource> {
        base(
            GOAL,
            FlowRole::User,
            Sink::Turn,
            Timing::Immediate,
            Visibility { context: true },
            LifecyclePolicy {
                undo: UndoBehavior::RemoveLast,
                compact: CompactBehavior::Compressable,
            },
        )
    }

    /// Register all built-in sources on a flow (idempotent per flow).
    pub fn register_all(flow: &mut ContextFlow) {
        flow.register(user_source());
        flow.register(model_source());
        flow.register(skills_source());
        flow.register(subagent_source());
        flow.register(goal_source());
    }
}

// ═══════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn flow_with(ids: &[&'static str]) -> (ContextFlow, Vec<Arc<dyn ContextSource>>) {
        let mut flow = ContextFlow::new();
        let all: Vec<Arc<dyn ContextSource>> = vec![
            builtin::user_source(),
            builtin::model_source(),
            builtin::skills_source(),
            builtin::subagent_source(),
            builtin::goal_source(),
        ];
        for s in &all {
            if ids.contains(&s.id()) {
                flow.register(s.clone());
            }
        }
        (flow, all)
    }

    #[test]
    fn unknown_source_rejected_with_trace() {
        let (mut flow, _) = flow_with(&[]);
        let mut store = MessageStore::new("seed-x");
        let receipt = flow.ingest(&mut store, "ghost", Message::user("hi"));
        assert!(!receipt.stored);
        let last = flow.trace().back().unwrap();
        assert_eq!(last.outcome, "rejected");
    }

    #[test]
    fn role_sink_compatibility_matrix() {
        // user → Turn ✓, Step ✗
        assert!(role_sink_compatible(FlowRole::User, Sink::Turn));
        assert!(!role_sink_compatible(FlowRole::User, Sink::Step));
        // developer → Trailing ✓, Turn ✓
        assert!(role_sink_compatible(FlowRole::Developer, Sink::Trailing));
        assert!(role_sink_compatible(FlowRole::Developer, Sink::Turn));
        // assistant → Step ✓ only
        assert!(role_sink_compatible(FlowRole::Assistant, Sink::Step));
        assert!(!role_sink_compatible(FlowRole::Assistant, Sink::Turn));
        // tool role parses (stored-message compat) but has no flow sink:
        // tool results bypass the flow via push_tool_result_direct*.
        assert!(!role_sink_compatible(FlowRole::Tool, Sink::Turn));
        assert!(!role_sink_compatible(FlowRole::Tool, Sink::Step));
        assert!(!role_sink_compatible(FlowRole::Tool, Sink::Trailing));
    }

    #[test]
    fn turn_boundary_submit_drains_at_boundary_in_order() {
        let (mut flow, _) = flow_with(&[builtin::SUBAGENT]);
        let mut store = MessageStore::new_ephemeral("seed-x");
        flow.submit(
            builtin::SUBAGENT,
            Message::developer("[SUBAGENT one]\nok"),
            Some("cmd-1".into()),
        )
        .unwrap();
        flow.submit(
            builtin::SUBAGENT,
            Message::developer("[SUBAGENT two]\nok"),
            Some("cmd-2".into()),
        )
        .unwrap();
        assert_eq!(flow.pending_len(), 2);
        // Nothing committed before the boundary.
        assert!(store.trailing_messages().is_empty());
        let (drained, command_ids) = flow.drain_turn_boundary(&mut store, "m", "e");
        assert_eq!(drained, 2);
        // command_id 按 submit 顺序透传（journal committed 标记用）。
        assert_eq!(command_ids, vec!["cmd-1".to_string(), "cmd-2".to_string()]);
        assert_eq!(flow.pending_len(), 0);
        assert_eq!(store.trailing_messages().len(), 2);
        assert!(
            flow.trace()
                .iter()
                .any(|t| t.outcome == "stored" && t.sink == "trailing")
        );
    }

    #[test]
    fn immediate_source_rejects_submit() {
        let (mut flow, _) = flow_with(&[builtin::USER]);
        let err = flow
            .submit(builtin::USER, Message::user("hi"), None)
            .unwrap_err();
        assert!(matches!(err, FlowError::TimingMismatch(builtin::USER)));
    }

    #[test]
    fn source_dedupe_key_blocks_repeat() {
        struct Keyed;
        impl ContextSource for Keyed {
            fn id(&self) -> &'static str {
                "keyed"
            }
            fn role(&self) -> FlowRole {
                FlowRole::Developer
            }
            fn sink(&self) -> Sink {
                Sink::Trailing
            }
            fn timing(&self) -> Timing {
                Timing::Immediate
            }
            fn visibility(&self) -> Visibility {
                Visibility { context: true }
            }
            fn lifecycle(&self) -> LifecyclePolicy {
                LifecyclePolicy {
                    undo: UndoBehavior::Keep,
                    compact: CompactBehavior::Preserved,
                }
            }
            fn dedupe_key(&self, msg: &Message) -> Option<String> {
                Some(extract_text(msg))
            }
        }
        let mut flow = ContextFlow::new();
        flow.register(Arc::new(Keyed));
        let mut store = MessageStore::new_ephemeral("seed-x");
        let r1 = flow.ingest(&mut store, "keyed", Message::developer("epoch-1"));
        let r2 = flow.ingest(&mut store, "keyed", Message::developer("epoch-1"));
        assert!(r1.stored);
        assert!(!r2.stored && r2.deduped);
        assert_eq!(store.trailing_messages().len(), 1);
    }

    /// M-doc/M-low②：A,B,A 锁定测试——两层去重均只阻断相邻重复；间隔重现
    /// 的相同文本（第三个 A）视为新事件正常放行。文档化设计，勿当吞错修。
    #[test]
    fn aba_pattern_passes_by_design() {
        struct Keyed;
        impl ContextSource for Keyed {
            fn id(&self) -> &'static str {
                "keyed"
            }
            fn role(&self) -> FlowRole {
                FlowRole::Developer
            }
            fn sink(&self) -> Sink {
                Sink::Trailing
            }
            fn timing(&self) -> Timing {
                Timing::TurnBoundary
            }
            fn visibility(&self) -> Visibility {
                Visibility { context: true }
            }
            fn lifecycle(&self) -> LifecyclePolicy {
                LifecyclePolicy {
                    undo: UndoBehavior::Keep,
                    compact: CompactBehavior::Preserved,
                }
            }
            fn dedupe_key(&self, msg: &Message) -> Option<String> {
                Some(extract_text(msg))
            }
        }
        let mut flow = ContextFlow::new();
        flow.register(Arc::new(Keyed));
        let mut store = MessageStore::new_ephemeral("seed-aba");
        let r1 = flow.ingest(&mut store, "keyed", Message::developer("A"));
        let r2 = flow.ingest(&mut store, "keyed", Message::developer("B"));
        let r3 = flow.ingest(&mut store, "keyed", Message::developer("A"));
        assert!(r1.stored);
        assert!(r2.stored);
        // 第三个 A：last_keys 已被 B 覆盖 → 放行而非 dedup。
        assert!(r3.stored, "A,B,A 的第三个 A 应放行（非相邻重复）");
        assert_eq!(store.trailing_messages().len(), 3);
    }

    #[test]
    fn developer_ingest_lands_in_trailing_via_subagent_source() {
        let (mut flow, _) = flow_with(&[builtin::SUBAGENT]);
        let mut store = MessageStore::new_ephemeral("seed-x");
        flow.submit(
            builtin::SUBAGENT,
            Message::developer("[SUBAGENT 'x' COMPLETED]\nok"),
            None,
        )
        .unwrap();
        flow.drain_turn_boundary(&mut store, "m", "e");
        let msgs = store.trailing_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "developer");
        assert_eq!(FlowRole::from_message(&msgs[0]), Some(FlowRole::Developer));
    }

    #[test]
    fn user_and_assistant_ingest_via_flow() {
        let (mut flow, _) = flow_with(&[builtin::USER, builtin::MODEL]);
        let mut store = MessageStore::new_ephemeral("seed-x");
        let r1 = flow.ingest(&mut store, builtin::USER, Message::user("hello"));
        assert!(r1.stored);
        let r2 = flow.ingest(
            &mut store,
            builtin::MODEL,
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::text("world")],
            },
        );
        assert!(r2.stored);
        let ctx = store.build_context_for_gate(&[]);
        let roles: Vec<&str> = ctx.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
    }

    #[test]
    fn developer_injection_survives_store_roundtrip_as_trailing() {
        let (mut flow, _) = flow_with(&[builtin::SUBAGENT]);
        let mut store = MessageStore::new_ephemeral("seed-x");
        flow.submit(
            builtin::SUBAGENT,
            Message::developer("[SUBAGENT 'x' COMPLETED]\nok"),
            None,
        )
        .unwrap();
        flow.drain_turn_boundary(&mut store, "m", "e");
        let persisted = store.to_vec();
        let (restored, repairs) = MessageStore::from_messages("seed-x", &persisted, 0);
        assert!(repairs.is_empty(), "unexpected repairs: {repairs:?}");
        let trailing = restored.trailing_messages();
        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].role, "developer");
    }

    #[test]
    fn trailing_injections_survive_undo_and_compact() {
        // LifecyclePolicy 声明与 store 行为的一致性固化：
        // trailing（skills/subagent 注入）undo=Keep、compact=Preserved。
        let (mut flow, _) = flow_with(&[builtin::USER, builtin::MODEL, builtin::SUBAGENT]);
        let mut store = MessageStore::new_ephemeral("seed-x");
        flow.ingest(&mut store, builtin::USER, Message::user("hello"));
        flow.ingest(
            &mut store,
            builtin::MODEL,
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::text("hi there")],
            },
        );
        flow.submit(
            builtin::SUBAGENT,
            Message::developer("[SUBAGENT 'x' COMPLETED]\nok"),
            None,
        )
        .unwrap();
        flow.drain_turn_boundary(&mut store, "m", "e");
        assert_eq!(store.trailing_messages().len(), 1);

        // undo（truncate_before_turn 删除整个 user turn）不动 trailing。
        let removed = store.truncate_before_turn("t1");
        assert!(removed);
        assert_eq!(
            store.trailing_messages().len(),
            1,
            "undo must keep trailing injections"
        );

        // compact（apply_compact 替换 turns 为摘要）不动 trailing。
        store.apply_compact("[COMPACT SUMMARY]", 0);
        assert_eq!(
            store.trailing_messages().len(),
            1,
            "compact must preserve trailing injections"
        );
        let ctx = store.build_context_for_gate(&[]);
        assert!(
            ctx.iter()
                .any(|m| m.role == "developer" && extract_text(m).starts_with("[SUBAGENT "))
        );
    }
}
