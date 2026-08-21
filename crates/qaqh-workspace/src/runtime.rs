//! Tool runtime state and ToolManager lifecycle.
//!
//! Knife-1 step 2: per-actor state moved from process-wide `static`s into
//! **thread-local** slots. Each in-process actor runs its Loop on its own daemon
//! thread and tool execution is synchronous on that actor thread, so
//! [`RUNTIME_CTX`], [`ACTOR_TOOL_MANAGER`], [`AGENT_MODE`] and the sandbox flag
//! live per-thread and give concurrent actors real isolation without
//! `ACTOR_SERIAL`. The process-level [`TOOL_MANAGER`] stays as the stable
//! fallback for non-actor threads (daemon `skills.list_tools`, serve, CLI).

use qaqh_types::ToolDef;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Unified runtime security context used for session binding and admission.
#[derive(Clone)]
pub struct RuntimeContext {
    pub active_session: String,
    pub permission_level: u8,
}

thread_local! {
    static RUNTIME_CTX: std::cell::RefCell<Option<RuntimeContext>> = const { std::cell::RefCell::new(None) };
}

static TOOL_MANAGER: OnceLock<Mutex<crate::ToolManager>> = OnceLock::new();

// Optional in-process actor manager, **per actor thread**.
//
// When installed it shadows the process manager for every `with_manager`
// caller on that thread — including tool threads spawned by that actor's turn
// while they resolve tools on the actor thread. Each actor installs its own
// before its Loop and clears on exit, so concurrent actors do not share a
// tool allowlist or mutate each other's stats.
thread_local! {
    static ACTOR_TOOL_MANAGER: std::cell::RefCell<Option<Arc<Mutex<crate::ToolManager>>>> = const { std::cell::RefCell::new(None) };
}

// Agent operating mode: 0=Code(默认), 1=Plan, 2=Code(旧编码兼容). Per-actor.
thread_local! {
    static AGENT_MODE: Cell<u8> = const { Cell::new(0) };
}

pub fn set_context(session: &str, permission_level: u8) {
    RUNTIME_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RuntimeContext {
            active_session: session.to_string(),
            permission_level,
        });
    });
}

pub fn clear_context() {
    RUNTIME_CTX.with(|ctx| *ctx.borrow_mut() = None);
}

pub fn context() -> Option<RuntimeContext> {
    RUNTIME_CTX.with(|ctx| ctx.borrow().clone())
}

/// Fail closed if the proof is missing a session or no longer matches the runtime.
pub(crate) fn verify_active_session(authorized_session: &str) -> Result<(), String> {
    if authorized_session.is_empty() {
        return Err("missing session in authorization".to_string());
    }
    RUNTIME_CTX.with(|runtime| {
        let context = runtime.borrow();
        let context = context
            .as_ref()
            .ok_or_else(|| "no active session".to_string())?;
        if authorized_session != context.active_session {
            return Err("session mismatch".to_string());
        }
        Ok(())
    })
}

pub fn set_mode(mode: u8) {
    AGENT_MODE.with(|slot| slot.set(mode));
}

/// 运行时重设工具白名单（工具模式 Standard/Minimal/Custom 的入口）。
/// 空列表 = 全量（标准模式）；未知名自动剔除并 warn（复用 apply_init 语义）。
pub fn set_allowed_tools(tools: Vec<String>) {
    with_manager(|manager| manager.set_allowed(tools));
}

pub(crate) fn is_plan_mode() -> bool {
    AGENT_MODE.with(|slot| slot.get() == 1)
}

/// Snapshot of the per-actor tool runtime state on the actor thread.
///
/// Tool execution runs on **spawned OS threads** (see engine_turn/engine_tool),
/// which never inherit the actor thread's thread-locals. The actor captures its
/// scope with [`ActorToolScope::capture`] before spawning a tool worker, and the
/// worker reinstalls it with [`ActorToolScope::install`], so concurrent actors
/// each run their tools under their own context/manager/mode/sandbox.
#[derive(Clone, Default)]
pub struct ActorToolScope {
    runtime: Option<RuntimeContext>,
    manager: Option<Arc<Mutex<crate::ToolManager>>>,
    mode: u8,
    sandbox: bool,
}

impl ActorToolScope {
    /// Capture the current (actor) thread's per-actor tool state.
    pub fn capture() -> Self {
        Self {
            runtime: context(),
            manager: ACTOR_TOOL_MANAGER.with(|slot| slot.borrow().clone()),
            mode: AGENT_MODE.with(|slot| slot.get()),
            sandbox: crate::authorization::is_subagent_sandbox(),
        }
    }

    /// Install this scope onto the current thread (a spawned tool worker),
    /// restoring the caller's previous thread-local state when the guard drops.
    pub fn install(&self) -> ActorToolScopeGuard {
        let previous = Self::capture();
        RUNTIME_CTX.with(|slot| *slot.borrow_mut() = self.runtime.clone());
        ACTOR_TOOL_MANAGER.with(|slot| *slot.borrow_mut() = self.manager.clone());
        AGENT_MODE.with(|slot| slot.set(self.mode));
        crate::authorization::set_subagent_sandbox(self.sandbox);
        ActorToolScopeGuard { previous }
    }
}

/// Restores the pre-install thread-local state on drop.
pub struct ActorToolScopeGuard {
    previous: ActorToolScope,
}

impl Drop for ActorToolScopeGuard {
    fn drop(&mut self) {
        RUNTIME_CTX.with(|slot| *slot.borrow_mut() = self.previous.runtime.clone());
        ACTOR_TOOL_MANAGER.with(|slot| *slot.borrow_mut() = self.previous.manager.clone());
        AGENT_MODE.with(|slot| slot.set(self.previous.mode));
        crate::authorization::set_subagent_sandbox(self.previous.sandbox);
    }
}

/// Initialize the process-global tool manager.
pub fn init_tools(
    session_seed: &str,
    extra_registrars: &[crate::registration::ToolRegistrar],
    allowed_tools: Vec<String>,
) {
    let mut manager = crate::registration::build_tool_manager(extra_registrars);
    manager.apply_init(allowed_tools, session_seed);
    let _ = TOOL_MANAGER.set(Mutex::new(manager));
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: tool manager inited ({} tools)", all_tools().len());
}

/// Install a private manager for one in-process actor (per-actor thread-local).
///
/// Unlike [`init_tools`], this does not mutate the daemon/worker process
/// manager. The caller is responsible for clearing it with
/// [`clear_actor_tool_manager`] when the actor exits. Because it is
/// thread-local, concurrent actors each get their own manager.
pub fn install_actor_tool_manager(manager: crate::ToolManager) {
    ACTOR_TOOL_MANAGER.with(|slot| {
        *slot.borrow_mut() = Some(Arc::new(Mutex::new(manager)));
    });
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: in-process actor tool manager installed");
}

/// Remove the in-process actor manager, falling back to the process manager.
/// Call on the same actor thread as [`install_actor_tool_manager`].
pub fn clear_actor_tool_manager() {
    ACTOR_TOOL_MANAGER.with(|slot| {
        *slot.borrow_mut() = None;
    });
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: in-process actor tool manager cleared");
}

pub(crate) fn with_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut crate::ToolManager) -> R,
{
    let actor_mgr = ACTOR_TOOL_MANAGER.with(|slot| slot.borrow().clone());
    if let Some(actor_mgr) = actor_mgr {
        let mut guard = lock_manager(&actor_mgr);
        return Some(f(&mut guard));
    }
    let mgr = TOOL_MANAGER.get()?;
    let mut guard = lock_manager(mgr);
    Some(f(&mut guard))
}

fn lock_manager(
    manager: &Mutex<crate::ToolManager>,
) -> std::sync::MutexGuard<'_, crate::ToolManager> {
    match manager.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("[TOOLS] ToolManager Mutex poisoned — recovering with into_inner()");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
pub(crate) fn register_test_handler(handler: crate::ToolHandler) {
    with_manager(|manager| manager.register(handler));
}

/// Return the canonical workspace root used for authorization and execution.
pub(crate) fn active_workspace_root() -> PathBuf {
    let workspace = crate::current_workspace();
    let root = if workspace.is_empty() || workspace == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(workspace)
    };
    crate::permission::resolve_target_path(root)
}

#[cfg(test)]
pub(crate) fn register_test_handler_with_placement(
    handler: crate::ToolHandler,
    placement: crate::ToolPlacement,
) {
    with_manager(|manager| manager.register_with_placement(handler, placement));
}

pub fn all_tools() -> Vec<ToolDef> {
    with_manager(|manager| manager.filtered_defs()).unwrap_or_default()
}

/// 查询 handler 声明的能力类别（权限决策单一事实源）。
/// 未注册/未初始化返回 None——调用方回退保守默认（Write）。
pub fn lookup_category(name: &str) -> Option<crate::permission::ToolCategory> {
    with_manager(|manager| manager.lookup(name).map(|handler| handler.category)).flatten()
}

/// Tool names from the **process** manager, ignoring any installed actor
/// manager. Used by daemon-side snapshots (e.g. `skills.list_tools`) that must
/// stay stable while an in-process subagent actor temporarily shadows the
/// manager for its own tool execution.
pub fn process_all_tool_names() -> Vec<String> {
    let Some(manager) = TOOL_MANAGER.get() else {
        return Vec::new();
    };
    let guard = lock_manager(manager);
    guard
        .all_defs()
        .iter()
        .map(|definition| definition.function.name.clone())
        .collect()
}

pub fn global_stats() -> crate::ToolStats {
    with_manager(|manager| manager.stats()).unwrap_or_default()
}

pub fn files_read() -> Vec<String> {
    global_stats().files_read
}

pub fn files_written() -> Vec<String> {
    global_stats().files_written
}

pub fn cancel_current_tool() {
    with_manager(|manager| manager.cancel_tool(None));
}

pub fn shutdown_tools() {
    log::info!("qaqh: tool manager shut down");
}
