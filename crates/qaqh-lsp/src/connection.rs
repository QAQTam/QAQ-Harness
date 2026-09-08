//! per-server 连接生命周期（mcp connection.rs 同款状态机，LSP 化）。
//!
//! ```text
//! Disconnected --ensure_connected--> Connected
//! Disconnected(冷却期) --ensure_connected--> Err(ConnectFailed)   // 不重启
//! Connected --crash(mainloop 退出/IO 错)--> Disconnected           // 下一次调用单次重启
//!   重启失败 --> 冷却 30s（期间调用直接报错；LSP server 重，冷却比 mcp 长）
//! Connected --inflight==0 连续 idle_shutdown_secs--> Disconnected // watchdog 回收
//! 任意状态 --shutting_down 闸--> 拒绝 lazy connect/重连/idle 重启（Shutdown）
//! ```
//!
//! 锁模型（mcp 同款）：`state: StdMutex<ConnState>` 短临界段禁跨 await；
//! service 句柄包 `Arc<tokio::sync::Mutex<ServerSession>>`；watchdog 持
//! `Weak<ServerConnection>` 防环。
//!
//! LSP 会话语义（与 mcp 的差异）：
//! - `initialize` 参数带 `workspace_folders=[root]` + `root_uri`（root 钉死在
//!   连接键里，同一 server 不同 root 是不同连接）；
//! - `initialized` 通知后等索引门（rust-analyzer `rustAnalyzer/Indexing` 或
//!   `rustAnalyzer/cachePriming` 的 `WorkDone::End`；通用回退：收到 initialize
//!   回复后等 `startup_timeout` 内首个 `Progress::End`，超时则放行——非 RA
//!   server 无此 token，不能硬等）；
//! - 文档同步 M1 薄版：每次查询前 `didOpen`（磁盘文本，version 自增；已 open
//!   则先 `didClose` 再重开——保证查的是落盘态）；常驻 open 不做增量。

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::tracing::TracingLayer;
use async_lsp::{MainLoop, ServerSocket};
use futures::channel::oneshot;
use lsp_types::notification::{Progress, PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, InitializeParams, InitializedParams, NumberOrString, ProgressParamsValue,
    Url, WindowClientCapabilities, WorkDoneProgress, WorkspaceFolder,
};
use tokio::task::JoinHandle;
use tower::ServiceBuilder;

use qaqh_config::config::LspServerConfig;
use qaqh_config::secrets::SecretStore;

use crate::adapter;
use crate::error::{LspError, LspErrorKind};

/// 连接状态快照（管理面只读视图；mcp ConnStatus 同款）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    Connected { inflight: u64 },
    Disconnected,
    Cooling { remaining_ms: u64 },
    ShuttingDown,
}

/// 生命周期参数（生产默认见 [`LifecycleSettings::default`]；测试可调小）。
#[derive(Debug, Clone)]
pub struct LifecycleSettings {
    /// lazy connect 总超时（LSP 含索引门，默认 30s；mcp 为 10s）。
    pub connect_timeout: Duration,
    /// 重连冷却（默认 30s；LSP server 重，冷却比 mcp 的 5s 长）。
    pub reconnect_cooldown: Duration,
    /// 单连接优雅关闭兜底。
    pub close_timeout: Duration,
    /// idle watchdog 巡检间隔。
    pub idle_tick: Duration,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            reconnect_cooldown: Duration::from_secs(30),
            close_timeout: Duration::from_secs(2),
            idle_tick: Duration::from_millis(200),
        }
    }
}

/// 已连接的 LSP 会话句柄（mainloop 驱动任务 + ServerSocket 请求口 +
/// 子进程持有者）。字段 crate 内可见（tool.rs 的 didOpen 薄版同步用）。
pub(crate) struct ServerSession {
    pub(crate) socket: ServerSocket,
    /// mainloop 驱动任务（abort 即断连；crash 判定看此 handle 是否结束）。
    pub(crate) _driver: JoinHandle<()>,
    /// 子进程持有者（drop 即组杀；connection 释放/关闭时连带退出）。
    pub(crate) _child: Box<dyn process_wrap::tokio::ChildWrapper>,
    /// 已 didOpen 的文档版本（uri → version；M1 每次查询重开即自增）。
    pub(crate) opened: BTreeMap<String, i32>,
}

struct ConnState {
    session: Option<Arc<tokio::sync::Mutex<ServerSession>>>,
    connected: bool,
    cooling_until: Option<Instant>,
    inflight: u64,
    idle_since: Option<Instant>,
}

/// 单个 LSP server 在单个 root 下的连接（lazy connect / 冷却 / idle 回收）。
pub struct ServerConnection {
    server: String,
    root: String,
    server_cfg: LspServerConfig,
    idle_secs: u64,
    settings: LifecycleSettings,
    gate: Arc<AtomicBool>,
    secret_store: SecretStore,
    /// 本代连接 spawn 的进程组 id（adapter 登记；组杀兜底清扫用，Unix）。
    pgid: StdMutex<Option<u32>>,
    state: StdMutex<ConnState>,
    connect_serializer: tokio::sync::Mutex<()>,
    watchdog: StdMutex<Option<JoinHandle<()>>>,
}

impl ServerConnection {
    /// 构造连接（不发起任何 IO——lazy 语义由 [`Self::ensure_connected`] 承担）。
    pub fn new(
        server: impl Into<String>,
        root: impl Into<String>,
        server_cfg: LspServerConfig,
        idle_secs: u64,
        settings: LifecycleSettings,
        gate: Arc<AtomicBool>,
        secret_store: SecretStore,
    ) -> Self {
        Self {
            server: server.into(),
            root: root.into(),
            server_cfg,
            idle_secs,
            settings,
            gate,
            secret_store,
            pgid: StdMutex::new(None),
            state: StdMutex::new(ConnState {
                session: None,
                connected: false,
                cooling_until: None,
                inflight: 0,
                idle_since: None,
            }),
            connect_serializer: tokio::sync::Mutex::new(()),
            watchdog: StdMutex::new(None),
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn server_config(&self) -> &LspServerConfig {
        &self.server_cfg
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConnState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前状态快照。
    pub fn status(&self) -> ConnStatus {
        if self.gate.load(Ordering::Relaxed) {
            return ConnStatus::ShuttingDown;
        }
        let state = self.lock_state();
        if state.connected && state.session.is_some() {
            return ConnStatus::Connected {
                inflight: state.inflight,
            };
        }
        if let Some(until) = state.cooling_until {
            let now = Instant::now();
            if now < until {
                return ConnStatus::Cooling {
                    remaining_ms: (until - now).as_millis() as u64,
                };
            }
        }
        ConnStatus::Disconnected
    }

    fn cooling_remaining(&self) -> Option<Duration> {
        let state = self.lock_state();
        state
            .cooling_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    fn arm_cooldown(&self) {
        let mut state = self.lock_state();
        state.cooling_until = Some(Instant::now() + self.settings.reconnect_cooldown);
    }

    /// 调用守卫：inflight +1，drop 时 -1 并刷新 idle_since（mcp CallGuard 同款）。
    pub fn begin_call(&self) -> CallGuard<'_> {
        let mut state = self.lock_state();
        state.inflight += 1;
        state.idle_since = None;
        CallGuard { conn: self }
    }

    /// lazy connect（幂等；已连接直接返回）。
    pub async fn ensure_connected(self: &Arc<Self>) -> Result<(), LspError> {
        {
            let state = self.lock_state();
            if state.connected && state.session.is_some() {
                return Ok(());
            }
        }
        let _serial = self.connect_serializer.lock().await;
        {
            let state = self.lock_state();
            if state.connected && state.session.is_some() {
                return Ok(());
            }
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shutting down; lazy connect rejected",
                    self.server
                ),
            ));
        }
        if let Some(remaining) = self.cooling_remaining() {
            return Err(LspError::new(
                LspErrorKind::ConnectFailed,
                format!(
                    "lsp server {}: reconnect cooldown {:.1}s remaining; retry later",
                    self.server,
                    remaining.as_secs_f32()
                ),
            ));
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shutting down; lazy connect rejected",
                    self.server
                ),
            ));
        }

        let timeout = self
            .server_cfg
            .startup_timeout_secs
            .clamp(1, 600)
            .max(self.settings.connect_timeout.as_secs());
        let timeout = Duration::from_secs(timeout);
        let this = Arc::clone(self);
        let attempt = tokio::time::timeout(timeout, this.connect_once()).await;
        match attempt {
            Err(_elapsed) => {
                self.arm_cooldown();
                Err(LspError::new(
                    LspErrorKind::ConnectTimeout,
                    format!("lsp server {}: connect timed out after {timeout:?}", self.server),
                ))
            }
            Ok(Err(error)) => {
                self.arm_cooldown();
                Err(error)
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    /// 单次连接全流程：解析 secret → spawn → mainloop → initialize → 索引门。
    async fn connect_once(self: Arc<Self>) -> Result<(), LspError> {
        let resolved =
            adapter::resolve_server_secrets(&self.server_cfg, &self.secret_store)?;
        let wrap = adapter::build_stdio_command(&resolved, &self.root);
        log::info!(
            "[lsp] server {}: spawning stdio transport (command={:?}, root={})",
            self.server,
            resolved.command,
            self.root
        );
        let spawned = adapter::spawn_server(wrap)?;
        adapter::record_spawn_pid(&self.server, spawned.pid);
        let mut child = spawned.child;
        let stdin = child.stdin().take().ok_or_else(|| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                "spawned LSP server has no stdin pipe".to_owned(),
            )
        })?;
        let stdout = child.stdout().take().ok_or_else(|| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                "spawned LSP server has no stdout pipe".to_owned(),
            )
        })?;

        // 索引门：RA 系 token 的 WorkDone::End 到达即放行。
        let (indexed_tx, indexed_rx) = oneshot::channel::<()>();
        let (mainloop, socket) = MainLoop::new_client(|_server| {
            let mut router = Router::new(ClientGate {
                indexed_tx: Some(indexed_tx),
            });
            router
                .notification::<Progress>(|this, prog| {
                    // 索引门：RA 系 token 的 WorkDone::End 到达即放行。
                    let is_index_end = matches!(&prog.token, NumberOrString::String(s) if is_index_end_token(s))
                        && matches!(
                            prog.value,
                            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
                        );
                    if is_index_end && let Some(tx) = this.indexed_tx.take() {
                        let _: Result<_, _> = tx.send(());
                    }
                    ControlFlow::Continue(())
                })
                .notification::<PublishDiagnostics>(|_, _| ControlFlow::Continue(()))
                .notification::<ShowMessage>(|_, params| {
                    log::info!("[lsp] server message {:?}: {}", params.typ, params.message);
                    ControlFlow::Continue(())
                })
                .event(|_, _: StopLoop| ControlFlow::Break(Ok(())));
            ServiceBuilder::new()
                .layer(TracingLayer::default())
                .layer(CatchUnwindLayer::default())
                .layer(ConcurrencyLayer::default())
                .service(router)
        });

        let driver = tokio::spawn(async move {
            // tokio IO → futures IO（async-lsp inspector.rs 同款 compat）。
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            let stdout = stdout.compat();
            let mut stdin = stdin.compat_write();
            if let Err(e) = mainloop.run_buffered(stdout, &mut stdin).await {
                log::warn!("[lsp] mainloop exited with error: {e}");
            }
        });

        // initialize（root 钉死：workspace_folders 主用；root_uri 已废弃但
        // 部分老 server 仍读它，双写兼容——deprecated 警告 allow 压住，M2 基线
        // 确认后移除）。
        let root_uri = Url::from_file_path(&self.root).map_err(|_| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                format!("lsp server {}: root {:?} is not a valid file path", self.server, self.root),
            )
        })?;
        let init = socket
            .request::<lsp_types::request::Initialize>(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root_uri.clone(),
                    name: "root".to_owned(),
                }]),
                #[allow(deprecated)]
                root_uri: Some(root_uri),
                capabilities: ClientCapabilities {
                    window: Some(WindowClientCapabilities {
                        work_done_progress: Some(true),
                        ..WindowClientCapabilities::default()
                    }),
                    ..ClientCapabilities::default()
                },
                ..InitializeParams::default()
            })
            .await
            .map_err(|e| {
                LspError::new(
                    LspErrorKind::ConnectFailed,
                    format!("lsp server {}: initialize failed: {e}", self.server),
                )
            })?;
        log::info!("[lsp] server {} initialized: {:?}", self.server, init.capabilities);
        socket.notify::<lsp_types::notification::Initialized>(InitializedParams {})?;

        // 索引门：等 RA 系 End token；超时放行（非 RA server 无此 token）。
        // 等待上限取 startup 超时的剩余量，最多 60s——initialize 已耗一部分。
        let gate_wait = Duration::from_secs(60.min(self.server_cfg.startup_timeout_secs.clamp(1, 600)));
        match tokio::time::timeout(gate_wait, indexed_rx).await {
            Ok(Ok(())) => log::info!("[lsp] server {}: index gate passed", self.server),
            _ => log::info!(
                "[lsp] server {}: index gate skipped (no RA progress token within {gate_wait:?})",
                self.server
            ),
        }

        if self.gate.load(Ordering::Relaxed) {
            socket.emit(StopLoop).ok();
            driver.abort();
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shut down during connect; connection discarded",
                    self.server
                ),
            ));
        }
        self.store_connected(socket, driver, child);
        Ok(())
    }

    fn store_connected(
        self: &Arc<Self>,
        socket: ServerSocket,
        driver: JoinHandle<()>,
        child: Box<dyn process_wrap::tokio::ChildWrapper>,
    ) {
        {
            let mut state = self.lock_state();
            state.session = Some(Arc::new(tokio::sync::Mutex::new(ServerSession {
                socket,
                _driver: driver,
                _child: child,
                opened: BTreeMap::new(),
            })));
            state.connected = true;
            state.cooling_until = None;
            state.idle_since = Some(Instant::now());
        }
        if let Some(pid) = adapter::take_spawn_pid(&self.server) {
            *self
                .pgid
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pid);
        }
        self.ensure_watchdog();
    }

    /// 会话句柄快照（请求路径用；None = 未连接）。
    pub(crate) fn session_snapshot(
        &self,
    ) -> Option<Arc<tokio::sync::Mutex<ServerSession>>> {
        self.lock_state().session.clone()
    }

    /// 标记 crash（mainloop 退出/IO 错时调用）：清会话 + 组杀兜底。
    /// 下次调用单次重启（冷却只在重启失败后设，mcp 同款语义）。
    pub(crate) fn mark_crashed(&self) {
        {
            let mut state = self.lock_state();
            state.session = None;
            state.connected = false;
            state.idle_since = None;
        }
        self.sweep_group();
        log::warn!("[lsp] server {} marked crashed; next call will reconnect", self.server);
    }

    /// 组杀兜底清扫（Unix；mcp sweep_group 同款）。
    fn sweep_group(&self) {
        let pid = self
            .pgid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        #[cfg(unix)]
        if let Some(pgid) = pid {
            // SAFETY：pgid 来自本连接 spawn 的独立进程组，不含 daemon 自身。
            unsafe {
                libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
    }

    fn ensure_watchdog(self: &Arc<Self>) {
        let mut slot = self
            .watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let tick = self.settings.idle_tick;
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(this) = weak.upgrade() else { return };
                if this.gate.load(Ordering::Relaxed) {
                    return;
                }
                let idle_secs = this.idle_secs;
                if idle_secs == 0 {
                    continue;
                }
                let should_drop = {
                    let state = this.lock_state();
                    !state.connected
                        || state.inflight != 0
                        || state.session.is_none()
                        || state.idle_since.is_none_or(|since| {
                            since.elapsed() < Duration::from_secs(idle_secs)
                        })
                };
                if should_drop {
                    continue;
                }
                log::info!(
                    "[lsp] server {} idle for {idle_secs}s; recycling",
                    this.server
                );
                this.shutdown().await;
            }
        }));
    }

    /// 优雅关闭：停 mainloop → 释放会话 → 组杀兜底。
    pub async fn shutdown(&self) {
        let session = {
            let mut state = self.lock_state();
            state.connected = false;
            state.session.take()
        };
        if let Some(session) = session {
            let mut guard = session.lock().await;
            // 先发 shutdown+exit（best-effort），再停 loop。
            let socket = guard.socket.clone();
            socket.request::<lsp_types::request::Shutdown>(()).await.ok();
            socket.notify::<lsp_types::notification::Exit>(()).ok();
            socket.emit(StopLoop).ok();
            guard._driver.abort();
            guard.opened.clear();
        }
        self.sweep_group();
    }
}

/// 调用守卫（mcp CallGuard 同款：drop 时 inflight -1 + idle_since 刷新）。
pub struct CallGuard<'a> {
    conn: &'a ServerConnection,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.conn.lock_state();
        state.inflight = state.inflight.saturating_sub(1);
        if state.inflight == 0 {
            state.idle_since = Some(Instant::now());
        }
    }
}

/// 索引门 token（RA 新老两代命名；async-lsp client_builder.rs 实测）。
fn is_index_end_token(token: &str) -> bool {
    matches!(token, "rustAnalyzer/Indexing" | "rustAnalyzer/cachePriming")
}

struct ClientGate {
    indexed_tx: Option<oneshot::Sender<()>>,
}

struct StopLoop;

impl async_lsp::LanguageClient for ClientGate {
    type Error = async_lsp::ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;
}
