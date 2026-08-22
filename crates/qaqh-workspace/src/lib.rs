//! ToolManager framework — tool registration, execution, and lifecycle.
//!
//! Submodules register handlers via `pub fn register(mgr: &mut ToolManager)`.

pub mod confirm_apply;
pub mod copy_range;
pub mod exec;
pub mod grep_tool;
pub mod pending;

pub mod apply_patch;
pub mod apply_patch_engine;

pub mod authorization;
pub mod backend;
mod code_delta;
pub mod edit;
pub mod execution;
pub mod file_cache;
pub mod file_edit_v2;
pub mod file_glob;
pub mod file_mutate;
pub mod file_query;
pub mod file_shared;
pub mod file_state;
pub mod git;
pub mod image_query;
pub mod runtime;
mod safety;
pub mod skill;
pub mod tool_side_fold;
mod web;

pub mod ask_user;

pub mod todo;

pub mod process_inspect;
pub mod process_registry;
pub mod workspace;
pub mod wsl_path;

pub mod registration;

pub mod manager;
/// Permission engine: tool categories, levels, trusted folders.
pub mod permission;
pub mod serve;

pub mod audit;

pub mod journal;

pub use backend::{
    BackendRequest, HttpToolExecutionBackend, LocalToolExecutionBackend, ToolExecutionBackend,
    ToolPlacement, install_workspace_backend, use_local_workspace_backend,
};
pub use manager::{ToolExecMeta, ToolExecReport, ToolManager, ToolStats};
pub use safety::SafetyVerdict;

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM" (matching the [timeis:] prefix convention).
pub fn now_utc8() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

/// Build a JSON success response for tools that only need a status message.
/// Extra fields can be added via `extra`.
pub fn json_ok(extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({"timeis": now_utc8(), "status": "ok"});
    if let Some(obj) = v.as_object_mut() {
        if let Some(ext) = extra.as_object() {
            for (k, val) in ext {
                obj.insert(k.clone(), val.clone());
            }
        } else if !extra.is_null() {
            obj.insert("content".to_string(), extra);
        }
    }
    v.to_string()
}

/// Build a JSON error response.
pub fn json_err(
    code: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> String {
    serde_json::json!({
        "timeis": now_utc8(),
        "status": "error",
        "code": code.into(),
        "message": message.into(),
        "hint": hint.into(),
    })
    .to_string()
}

/// Risk level for tool operations, replacing per-handler safety functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    Destructive,
    Administrative,
}

// ── Macro: handler ──
///
/// The inner `$exec` function returns the canonical structured result directly.
#[macro_export]
macro_rules! handler {
    ($name:ident, $exec:ident) => {
        fn $name(ctx: ToolCallCtx) -> ToolResult {
            $exec(&ctx.args)
        }
    };
}

/// String 结果兼容宏：旧式 exec 返回 `String`（[ERROR]/[PARTIAL] 前缀视为失败），
/// 包装为结构化 `ToolResult`。file_mutate 系列工具使用（write/edit/edit_block/delete）。
#[macro_export]
macro_rules! handler_from_string {
    ($name:ident, $exec:ident) => {
        fn $name(ctx: ToolCallCtx) -> ToolResult {
            let s: String = $exec(&ctx.args);
            if s.trim_start().starts_with("[ERROR") || s.trim_start().starts_with("[PARTIAL") {
                ToolResult::error(s)
            } else {
                ToolResult::ok(s)
            }
        }
    };
}

// ── JsonArgs trait: typed access to tool arguments ──

pub trait JsonArgs {
    fn s(&self, key: &str) -> String;
    fn s_or(&self, key: &str, default: &str) -> String;
    fn opt_bool(&self, key: &str) -> Option<bool>;
}

impl JsonArgs for serde_json::Value {
    fn s(&self, key: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default()
    }
    fn s_or(&self, key: &str, default: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| default.to_string())
    }
    fn opt_bool(&self, key: &str) -> Option<bool> {
        let val = self.get(key)?;
        val.as_bool()
            .or_else(|| val.as_str().and_then(|s| s.parse::<bool>().ok()))
    }
}

use std::cell::{Cell, RefCell};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use qaqh_types::ToolDef;
pub use qaqh_types::{
    ContentRef, ToolContinuation, ToolError as CanonicalToolError, ToolModelPayload, ToolResult,
    ToolStatus,
};

// ── Global state ──

/// Global cancel flag for tool execution.
///
/// Set at the start of every interrupt (Cancel, session switch, shutdown)
/// from the reader thread and the main loop. Checked by bridge before
/// executing each tool. Reset at the top of [`crate::Loop::handle_user_input`]
/// so it is per-turn, not cross-session.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static CURRENT_SESSION: Mutex<Option<String>> = Mutex::new(None);

pub fn set_current_session(seed: &str) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_SESSION.with(|slot| *slot.borrow_mut() = Some(seed.to_string()));
        return;
    }
    let mut guard = CURRENT_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(seed.to_string());
}

pub static CURRENT_WORKSPACE: RwLock<String> = RwLock::new(String::new());

thread_local! {
    static ACTOR_WORKSPACE: RefCell<Option<String>> = const { RefCell::new(None) };
    static ACTOR_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
    static ACTOR_CANCEL: Cell<bool> = const { Cell::new(false) };
}

/// Enter an actor-scoped workspace/session context.
///
/// In-process session and subagent actors call this before running their Loop
/// so workspace/session/cancel state is isolated per actor thread. Once set,
/// `set_workspace` / `set_current_session` write to the thread-local slot;
/// reads prefer the thread-local slot and fall back to the process-wide globals.
pub fn set_actor_context(workspace: &str, session: &str) {
    ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = Some(workspace.to_string()));
    ACTOR_SESSION.with(|slot| *slot.borrow_mut() = Some(session.to_string()));
    ACTOR_CANCEL.with(|slot| slot.set(false));
}

/// Leave actor-scoped state and restore process-wide behavior.
pub fn clear_actor_context() {
    ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = None);
    ACTOR_SESSION.with(|slot| *slot.borrow_mut() = None);
    ACTOR_CANCEL.with(|slot| slot.set(false));
}

/// Returns the current session seed, preferring actor-local state.
pub fn current_session() -> Option<String> {
    let local = ACTOR_SESSION.with(|slot| slot.borrow().clone());
    if local.is_some() {
        return local;
    }
    CURRENT_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Returns the current workspace root, preferring actor-local state.
pub fn current_workspace() -> String {
    let local = ACTOR_WORKSPACE.with(|slot| slot.borrow().clone());
    if let Some(workspace) = local {
        return workspace;
    }
    CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Set the actor-local or process-wide cancel flag.
pub fn set_cancel(value: bool) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_CANCEL.with(|slot| slot.set(value));
    } else {
        CANCEL.store(value, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Read the effective cancel flag (actor-local if present).
pub fn is_cancel() -> bool {
    let local = ACTOR_CANCEL.with(|slot| slot.get());
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        local
    } else {
        CANCEL.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Unit tests mutate process-wide runtime state. Keep those mutations
/// deterministic even when the Rust test harness runs modules in parallel.
#[cfg(test)]
pub(crate) static TEST_RUNTIME_SERIAL: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// Tools blocked in PLAN mode. 分类与 handler 声明的 category 对应
/// （Read 类不入表；Write/Exec/Net 按语义列名）。
pub const PLAN_BLOCKED: &[&str] = &["edit", "exec", "process", "todo"];

pub fn set_workspace(path: &str) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = Some(path.to_string()));
        return;
    }
    let mut ws = CURRENT_WORKSPACE.write().unwrap_or_else(|e| e.into_inner());
    *ws = path.to_string();
}

/// Resolve a path against the workspace root.
/// If the path is already absolute, return as-is.
/// If the workspace is empty or ".", return the path as-is (OS cwd resolution).
/// Otherwise, join the workspace root with the relative path.
pub fn resolve_workspace_path(path: &str) -> String {
    use std::path::Path;
    if path.is_empty() {
        return path.to_string();
    }
    // WSL serve 侧：把 worker 下发的 Windows 绝对路径（`F:\...`）归一化为 /mnt。
    let path = crate::wsl_path::platform_workspace_path(path);
    let p = Path::new(&path);
    if p.is_absolute() {
        return path;
    }
    let ws = current_workspace();
    if ws.is_empty() || ws == "." {
        return path.to_string();
    }
    let joined = Path::new(&ws).join(p);
    // Normalise: strip redundant . components via iterator (e.g. D:\foo\./bar → D:\foo\bar)
    let normalized: std::path::PathBuf = joined.components().collect();
    normalized.to_string_lossy().to_string()
}

/// Convert an absolute path into a display-friendly relative path.
///
/// Strips the workspace root prefix and uses `/` separators for
/// cross-platform consistency (like Git, VS Code, etc.).
///
/// # Examples
/// ```ignore
/// // workspace = D:\project\QAQ-Harness
/// display_path("D:\\project\\QAQ-Harness\\crates\\foo\\bar.rs") → "crates/foo/bar.rs"
/// display_path("/home/user/project/src/main.rs")          → "src/main.rs"
/// ```
pub fn display_path(abs_path: &str) -> String {
    // Normalise input: strip redundant . components
    let normalized: std::path::PathBuf = std::path::Path::new(abs_path).components().collect();
    let norm_str = normalized.to_string_lossy();

    let ws = current_workspace();
    let ws = ws.trim_end_matches(['/', '\\']);

    if !ws.is_empty() && ws != "." {
        // Case-insensitive prefix match on Windows。注意不能用 to_lowercase
        // 副本做 starts_with 再按原长切片：Unicode 小写映射可能改变字节长度，
        // 非 ASCII 路径会 panic。NTFS 大小写折叠仅限 ASCII，逐字节等长比较
        // 同时保证切点是 char boundary。
        if norm_str.len() >= ws.len()
            && norm_str.as_bytes()[..ws.len()].eq_ignore_ascii_case(ws.as_bytes())
        {
            let (_, tail) = norm_str.split_at(ws.len());
            let rel = tail.trim_start_matches(['/', '\\']);
            return rel.replace('\\', "/");
        }
    }
    // Not under workspace — return normalised path with forward slashes
    norm_str.replace('\\', "/")
}

// ── ToolCallCtx ──

/// A single non-blocking execution-output update for the frontend.
///
/// `seq` is monotonic per execution and represents the order in which pipe
/// reader threads observed chunks.  Cross-stream ordering is therefore local
/// observation order, while ordering within a stream is exact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecProgressEvent {
    pub tool_call_id: String,
    pub stream: ExecOutputStream,
    pub seq: u64,
    pub chunk: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
}

impl ExecOutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Bounded, lossy progress sender. Pipe readers must never wait for a slow UI.
#[derive(Clone)]
pub struct ExecProgressSender {
    tx: std::sync::mpsc::SyncSender<ExecProgressEvent>,
    dropped_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ExecProgressSender {
    pub fn try_send(&self, event: ExecProgressEvent) {
        let bytes = event.chunk.len() as u64;
        if self.tx.try_send(event).is_err() {
            self.dropped_bytes
                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub const EXEC_PROGRESS_CHANNEL_CAPACITY: usize = 256;

pub fn bounded_exec_progress_channel() -> (
    ExecProgressSender,
    std::sync::mpsc::Receiver<ExecProgressEvent>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(EXEC_PROGRESS_CHANNEL_CAPACITY);
    let dropped_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    (ExecProgressSender { tx, dropped_bytes }, rx)
}

#[derive(Clone)]
pub struct ToolCallCtx {
    pub id: String,
    pub name: String,
    pub action: String,
    pub args: serde_json::Value,
    pub tx_progress: Option<ExecProgressSender>,
    pub timeout_secs: Option<u64>,
    /// Per-invocation cancellation signal owned by ToolManager.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Private, typed effects returned to the host runtime. Sharing this cell
    /// across context clones avoids parsing trusted effects from tool text.
    pub(crate) skill_effects: std::sync::Arc<std::sync::Mutex<Vec<ToolEffect>>>,
}

/// Trusted, typed state transitions emitted by tool handlers.
///
/// Keeping this wrapper generic lets the runtime add other effect families
/// without widening the textual tool-result protocol.
#[derive(Clone, Debug)]
pub enum ToolEffect {
    Skill(qaqh_skills::SkillEffect),
}

impl ToolCallCtx {
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(|v| v.as_str())
    }
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.args.get(key).and_then(|v| v.as_u64())
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.args.get(key).and_then(|v| v.as_bool())
    }
    pub(crate) fn push_skill_effect(&self, effect: qaqh_skills::SkillEffect) {
        self.skill_effects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(ToolEffect::Skill(effect));
    }
    pub(crate) fn take_skill_effects(&self) -> Vec<ToolEffect> {
        std::mem::take(
            &mut *self
                .skill_effects
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }
}

/// Structured error type for tool operations.
///
/// Replaces `"[ERROR]"` / `"[PARTIAL]"` / `"[CANCELLED]"` prefix conventions
/// with a typed enum so callers can match on error categories instead of
/// string prefixes.
#[derive(Debug, Clone)]
pub enum ToolError {
    /// Tool manager not initialised or Mutex poisoned.
    ManagerUnavailable,
    /// Unknown tool name requested.
    UnknownTool { name: String },
    /// Session mismatch between authorization proof and active runtime.
    SessionMismatch,
    /// Permission denied for the requested operation.
    PermissionDenied { reason: String },
    /// Operation cancelled by user.
    Cancelled,
    /// Blocked by operating mode (e.g. PLAN mode blocks write/exec).
    BlockedByMode { mode: String, tool: String },
    /// Invalid or malformed arguments.
    InvalidArgs { message: String },
    /// IO / filesystem error.
    Io {
        tool: String,
        path: String,
        message: String,
    },
    /// Resource mismatch — invocation targets different resources than authorized.
    ResourceMismatch,
    /// Runtime context not initialised.
    RuntimeNotInitialized,
    /// Tool-specific error with a machine-readable code.
    ToolSpecific {
        tool: String,
        code: String,
        message: String,
    },
    /// Partial failure — operation did not complete.
    Partial { message: String },
    /// Generic internal error.
    Internal { message: String },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManagerUnavailable => {
                write!(
                    f,
                    "[ERROR] tool manager unavailable — poisoned or not initialised"
                )
            }
            Self::UnknownTool { name } => {
                write!(f, "[ERROR] Unknown tool: {name}")
            }
            Self::SessionMismatch => {
                write!(
                    f,
                    "[ERROR] session mismatch — authorization doesn't match active session"
                )
            }
            Self::PermissionDenied { reason } => {
                write!(f, "[PERMISSION_REQUIRED] {reason}")
            }
            Self::Cancelled => {
                write!(f, "[CANCELLED]")
            }
            Self::BlockedByMode { mode, tool } => {
                write!(f, "[BLOCKED] {mode} mode: '{tool}' is not allowed")
            }
            Self::InvalidArgs { message } => {
                write!(f, "[ERROR] invalid arguments: {message}")
            }
            Self::Io {
                tool,
                path,
                message,
            } => {
                write!(f, "[ERROR] {tool}: {path} — {message}")
            }
            Self::ResourceMismatch => {
                write!(
                    f,
                    "[ERROR] Resource mismatch — tool invocation targets different resources than authorized"
                )
            }
            Self::RuntimeNotInitialized => {
                write!(
                    f,
                    "[ERROR] Tool execution requires an initialized runtime context — call set_context() first"
                )
            }
            Self::ToolSpecific {
                tool,
                code,
                message,
            } => {
                write!(f, "[ERROR] {tool}: {code} — {message}")
            }
            Self::Partial { message } => {
                write!(f, "[PARTIAL] {message}")
            }
            Self::Internal { message } => {
                write!(f, "[ERROR] {message}")
            }
        }
    }
}

impl ToolError {
    pub fn into_result(self) -> ToolResult {
        let message = self.to_string();
        let (code, retryable, hint) = match &self {
            Self::ManagerUnavailable => ("MANAGER_UNAVAILABLE", true, None),
            Self::UnknownTool { .. } => ("UNKNOWN_TOOL", false, None),
            Self::SessionMismatch => ("SESSION_MISMATCH", false, None),
            Self::PermissionDenied { .. } => ("PERMISSION_DENIED", false, None),
            Self::Cancelled => ("CANCELLED", false, None),
            Self::BlockedByMode { .. } => ("BLOCKED_BY_MODE", false, None),
            Self::InvalidArgs { .. } => ("INVALID_ARGUMENTS", false, None),
            Self::Io { .. } => ("IO_ERROR", false, None),
            Self::ResourceMismatch => ("RESOURCE_MISMATCH", false, None),
            Self::RuntimeNotInitialized => ("RUNTIME_NOT_INITIALIZED", false, None),
            Self::ToolSpecific { .. } => ("TOOL_ERROR", false, None),
            Self::Partial { .. } => ("PARTIAL", false, None),
            Self::Internal { .. } => ("INTERNAL_ERROR", true, None),
        };
        ToolResult::error_with(code, message, retryable, hint.map(str::to_string))
    }
}

// ── parse helpers ──

pub fn parse_arg(args: &str, key: &str) -> String {
    qaqh_types::arg::parse_arg(args, key).unwrap_or_default()
}

pub fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    qaqh_types::arg::parse_arg_or(args, key, default)
}

pub fn parse_opt(args: &str, key: &str) -> Option<String> {
    qaqh_types::arg::parse_arg(args, key)
}

// ── ToolHandler ──

#[derive(Clone)]
pub struct ToolHandler {
    pub key: String,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub handler: fn(ToolCallCtx) -> ToolResult,
    pub risk: ToolRisk,
    /// 能力类别（权限决策单一事实源）：Read/Write/Exec/Net。
    /// 取代按工具名字表分类（categorize_tool 已删除）——加新工具必须
    /// 显式声明，MCP 等动态工具未来包装为 Capability 时复用同字段。
    pub category: crate::permission::ToolCategory,
    pub default_timeout: Duration,
}

impl ToolHandler {
    pub fn to_tool_def(&self) -> ToolDef {
        ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: self.key.clone(),
                description: self.description.to_string(),
                parameters: self.input_schema.clone(),
            },
        }
    }
}

#[cfg(test)]
mod schema_spot_check;

#[cfg(test)]
mod actor_context_tests {
    use super::*;

    #[test]
    fn actor_context_isolates_workspace_and_session_on_current_thread() {
        // Enter actor context and verify thread-local takes precedence.
        set_actor_context("/tmp/actor-ws", "actor-seed");
        assert_eq!(current_workspace(), "/tmp/actor-ws");
        assert_eq!(current_session().as_deref(), Some("actor-seed"));

        set_workspace("/tmp/actor-ws-2");
        set_current_session("actor-seed-2");
        assert_eq!(current_workspace(), "/tmp/actor-ws-2");
        assert_eq!(current_session().as_deref(), Some("actor-seed-2"));

        set_cancel(true);
        assert!(is_cancel());

        clear_actor_context();
        assert_ne!(current_workspace(), "/tmp/actor-ws-2");
        assert_ne!(current_session().as_deref(), Some("actor-seed-2"));
        assert!(!is_cancel());
    }
}
