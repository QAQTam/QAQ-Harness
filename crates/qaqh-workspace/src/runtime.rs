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

pub fn set_mode(mode: u8) {
    AGENT_MODE.with(|slot| slot.set(mode));
}

/// Explicit tool-execution context (PR-3-2 / D5-G2): the caller (agent tool
/// dispatch, workspace serve, CLI) assembles one per execution instead of
/// mutating process/thread state first. Threaded through the execute path;
/// the workspace installs it for the duration of the call and restores the
/// previous ambient state on drop.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub session_id: String,
    pub permission_level: u8,
    /// Agent operating mode (0=Code, 1=Plan) recorded at dispatch time.
    pub mode: u8,
    /// Workspace root for this execution. `None` = keep the current process
    /// workspace (agent in-process path; serve sets it separately until
    /// PR-3-3 moves cwd injection fully host-side).
    pub workspace_root: Option<String>,
}

impl ToolCtx {
    /// Context for an already-admitted caller (serve / CLI): full permission,
    /// Code mode, current process workspace.
    pub fn admitted(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            permission_level: 4,
            mode: 0,
            workspace_root: None,
        }
    }
}

/// RAII guard restoring the thread's previous ambient context.
pub struct ToolCtxGuard {
    previous: Option<RuntimeContext>,
    restore_mode: u8,
}

impl Drop for ToolCtxGuard {
    fn drop(&mut self) {
        RUNTIME_CTX.with(|ctx| *ctx.borrow_mut() = self.previous.take());
        AGENT_MODE.with(|slot| slot.set(self.restore_mode));
    }
}

/// Install `ctx` as the ambient runtime context for this thread until the
/// returned guard drops. The execute path calls this so tool handlers can
/// keep reading ambient state while the *caller* stays explicit.
pub fn install_tool_ctx(ctx: &ToolCtx) -> ToolCtxGuard {
    let previous = RUNTIME_CTX.with(|slot| {
        let previous = slot.borrow().clone();
        *slot.borrow_mut() = Some(RuntimeContext {
            active_session: ctx.session_id.clone(),
            permission_level: ctx.permission_level,
        });
        previous
    });
    let restore_mode = AGENT_MODE.with(|slot| {
        let previous = slot.get();
        slot.set(ctx.mode);
        previous
    });
    ToolCtxGuard {
        previous,
        restore_mode,
    }
}

/// Bind only the active session (permission stays unknown post-admission).
/// Used by `execute_authorized` where the [`AuthorizedToolCall`](crate::AuthorizedToolCall)
/// itself is the execution context.
pub fn bind_session(session_id: &str) -> ToolCtxGuard {
    install_tool_ctx(&ToolCtx {
        session_id: session_id.to_string(),
        permission_level: 0,
        mode: AGENT_MODE.with(|slot| slot.get()),
        workspace_root: None,
    })
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
    let defs = with_manager(|manager| manager.filtered_defs()).unwrap_or_default();
    if image_tool_enabled() {
        defs
    } else {
        // 端点不支持视觉输入（未声明 supports_image_tool）时，
        // read_image 不进入模型工具清单。
        defs.into_iter()
            .filter(|def| def.function.name != "read_image")
            .collect()
    }
}

/// 当前配置的 provider endpoint 是否接受图片输入（read_image 工具开关）。
///
/// PR-1-10 / D2：能力快照由宿主注入（[`set_image_capability`]：daemon
/// 装配 / config reload / serve 启动），工具调用路径零磁盘读。
/// 未注入时（单元测试 / 未装配进程）默认放行——工具可见性交给注册方，
/// 执行路径的自然错误兜底真实不支持的场景。
pub fn image_tool_enabled() -> bool {
    image_caps().map(|c| c.endpoint).unwrap_or(true)
}

/// 当前 (provider, endpoint, model) 组合是否接受图片输入。
///
/// 比端点级 [`image_tool_enabled`] 更精确：路由器端点（如 OpenRouter）的
/// 模型异构，文本-only 模型需要在此处被拒绝，而不是让带图请求打到上游
/// 换回一个不透明的 400。快照语义同上（PR-1-10）。
pub fn image_model_supported() -> bool {
    image_caps().map(|c| c.model).unwrap_or(true)
}

#[derive(Clone, Copy)]
struct ImageCaps {
    endpoint: bool,
    model: bool,
}

static IMAGE_CAPS: Mutex<Option<ImageCaps>> = Mutex::new(None);

/// 注入图片能力快照（PR-1-10 / D2）。宿主在装配 / reload / serve 启动时
/// 以当前配置计算后调用；快照存活期内工具调用路径不再触碰磁盘。
pub fn set_image_capability(endpoint_enabled: bool, model_supported: bool) {
    *IMAGE_CAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ImageCaps {
        endpoint: endpoint_enabled,
        model: model_supported,
    });
}

fn image_caps() -> Option<ImageCaps> {
    *IMAGE_CAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
