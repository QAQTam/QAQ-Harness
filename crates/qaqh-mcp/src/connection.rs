//! per-server 连接生命周期（设计 `docs/mcp-client-design.md` §5.1）。
//!
//! 状态机（语义以设计 §5.1 表格为权威；handover 草图与其冲突处以设计为准）：
//!
//! ```text
//! Disconnected --ensure_connected--> Connected
//! Disconnected(冷却期) --ensure_connected--> Err(ConnectFailed)   // 不重启
//! Connected --crash(T transport closed)--> Disconnected           // 下一次调用单次重启
//!   重启失败 --> 冷却 5s（期间调用直接报错）
//! Connected --inflight==0 连续 idle_shutdown_secs--> Disconnected // watchdog 回收
//! 任意状态 --shutting_down 闸--> 拒绝 lazy connect/重连/idle 重启（Shutdown）
//! ```
//!
//! 锁模型（关键正确性决策，替代 handover 草图的 AtomicU64+Mutex 分离）：
//! - `state: StdMutex<ConnState>`——inflight / idle_since / cooling_until /
//!   service 句柄同锁。理由：[`CallGuard::drop`] 是同步上下文，必须用 std 锁；
//!   单锁同时消除"begin_call 与 idle 判定竞态导致在飞调用被回收"的可能。
//!   所有持锁段不含 await。
//! - service 本体包 `Arc<tokio::sync::Mutex<RunningService>>`——RPC/关闭可跨
//!   await，但先从 state 中 clone/take 句柄再锁，避免 std 锁跨 await。
//! - watchdog 持 `Weak<ServerConnection>`——不阻止连接释放，无 Arc 环。
//!
//! crash 冷却语义（与 handover 草图"下次调用走冷却后重启"的差异）：
//! 设计 §5.1「崩溃恢复：下一次调用触发单次重启；重启失败保持报错，冷却期内
//! 不再尝试」→ 本实现 crash 本身**不**设冷却，重启失败才设。防止 crash-loop
//! 靠"每次调用至多一次重启" + 失败后冷却双重约束。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceError};
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::bridge;
use crate::error::{McpError, McpErrorKind};
use qaqh_config::config::McpServerConfig;

// ═══════ PR-M2-2：server 通知 → 重拉管线（list_changed 订阅）═══════
//
// adapter 的 NotifyBridge（客户端 handler）收到 server 的
// `notifications/tools/list_changed` / `notifications/resources/list_changed`
// 后按 name 查本表触发重拉 + 置脏。沿用 record_spawn_pid 的 crate 内桥接
// 模式（handler 与 connection 分居两文件、factory 闭包拿不到 conn 引用——
// Weak 表解耦，Weak 防环，同 watchdog 决策 #2）。

fn conn_notify_slot() -> &'static StdMutex<BTreeMap<String, Weak<ServerConnection>>> {
    static CONN_NOTIFY: OnceLock<StdMutex<BTreeMap<String, Weak<ServerConnection>>>> =
        OnceLock::new();
    CONN_NOTIFY.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

/// server 通知入口（adapter NotifyBridge 调用；按 name 触发重拉）。
/// 未注册/已释放的 name → 无操作（竞态窗口内连接已死，忽略即可）。
pub(crate) fn notify_lists_changed(server: &str) {
    let weak = conn_notify_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(server)
        .cloned();
    let Some(conn) = weak.and_then(|weak| weak.upgrade()) else {
        return;
    };
    conn.spawn_refresh_lists();
}

/// rmcp 客户端服务句柄（Auto 生命周期 + 单元 service `()`）。
/// 客户端 service 句柄（H = adapter 的 NotifyBridge：server 通知桥接，
/// PR-M2-2。mock factory 同型构造——通知不触发，仅为类型统一）。
pub type ClientService = RunningService<RoleClient, crate::adapter::NotifyBridge>;

/// 一次 connect 的产物（工厂层错误；超时由连接层统一判定）。
pub type ConnectFuture = Pin<
    Box<
        dyn Future<Output = Result<ClientService, Box<dyn std::error::Error + Send + Sync>>> + Send,
    >,
>;

/// connect 工厂：给定 server 名与配置，产出一个已完成 initialize 的客户端。
/// 生产实现见 [`crate::adapter`]；测试注入 in-memory 双工。
pub type ConnectFactory = Arc<dyn Fn(&str, &McpServerConfig) -> ConnectFuture + Send + Sync>;

/// 生命周期参数（生产默认见 [`LifecycleSettings::default`]；测试可调小）。
#[derive(Debug, Clone)]
pub struct LifecycleSettings {
    /// lazy connect 总超时（设计 §5.1：10s）。
    pub connect_timeout: Duration,
    /// 重连冷却（设计 §5.1：5s）。
    pub reconnect_cooldown: Duration,
    /// 单连接优雅关闭兜底（超时后 service 被 cancel → transport drop → 组杀）。
    pub close_timeout: Duration,
    /// idle watchdog 巡检间隔。
    pub idle_tick: Duration,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            reconnect_cooldown: Duration::from_secs(5),
            close_timeout: Duration::from_secs(2),
            idle_tick: Duration::from_millis(200),
        }
    }
}

/// 连接状态快照（管理面只读视图）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    /// 已连接；`inflight` 为在飞调用数。
    Connected { inflight: u64 },
    /// 未连接（初始 / idle 回收后 / 崩溃后）。
    Disconnected,
    /// 重连冷却中，`remaining_ms` 为剩余毫秒。
    Cooling { remaining_ms: u64 },
    /// daemon 关闭闸已落下。
    ShuttingDown,
}

/// 连接内部状态（全部短临界段，禁跨 await 持锁）。
struct ConnState {
    service: Option<Arc<TokioMutex<ClientService>>>,
    connected: bool,
    cooling_until: Option<Instant>,
    inflight: u64,
    /// 最近一次 inflight 归零的时刻；None = 有调用在飞（E-2 语义）。
    idle_since: Option<Instant>,
    /// 连接成功后自动拉取的 `tools/list` 快照（投影/模型面重建的唯一来源）。
    /// crash/关闭即清空（模型面与连接状态一致）。
    tools: Option<Arc<Vec<rmcp::model::Tool>>>,
    /// 连接成功后自动拉取的 `resources/list` 快照（PR-M2-1 聚合工具
    /// `list_resources` 的数据源；idle 回收不清——重连时刷新，与 tools 同款）。
    resources: Option<Arc<Vec<rmcp::model::Resource>>>,
    /// 连接成功后自动拉取的 `resources/templates/list` 快照（uriTemplate
    /// 展开提示的来源；生命周期同上）。
    resource_templates: Option<Arc<Vec<rmcp::model::ResourceTemplate>>>,
}

/// 单个 MCP server 的连接（lazy connect / 冷却 / idle 回收 / 崩溃标记）。
///
/// 由 [`crate::manager::McpManager`] 创建并持有；调用方（M1-5 桥接）流程：
/// `manager.get_or_connect(server)` → [`ServerConnection::begin_call`] → RPC。
pub struct ServerConnection {
    name: String,
    server_cfg: McpServerConfig,
    /// idle 回收阈值（秒；来自 `[mcp].idle_shutdown_secs`，0 = 常驻）。
    idle_secs: u64,
    settings: LifecycleSettings,
    /// manager 共享的 `shutting_down` 闸。
    gate: Arc<AtomicBool>,
    /// manager 共享的“工具缓存已变脏”标记（连接填充/清空缓存时置位；
    /// [`crate::bridge::take_projection_batch`] 消费后复位）。
    dirty: Arc<AtomicBool>,
    /// 本代连接 spawn 的进程组 id（adapter 登记；组杀兜底清扫用，Unix）。
    pgid: StdMutex<Option<u32>>,
    state: StdMutex<ConnState>,
    /// 串行化 connect，防并发双 spawn。
    connect_serializer: TokioMutex<()>,
    connect_factory: ConnectFactory,
    watchdog: StdMutex<Option<JoinHandle<()>>>,
}

impl ServerConnection {
    /// 构造连接（不发起任何 IO——lazy 语义由 [`Self::ensure_connected`] 承担）。
    pub fn new(
        name: impl Into<String>,
        server_cfg: McpServerConfig,
        idle_secs: u64,
        settings: LifecycleSettings,
        gate: Arc<AtomicBool>,
        dirty: Arc<AtomicBool>,
        connect_factory: ConnectFactory,
    ) -> Self {
        Self {
            name: name.into(),
            server_cfg,
            idle_secs,
            settings,
            gate,
            dirty,
            pgid: StdMutex::new(None),
            state: StdMutex::new(ConnState {
                service: None,
                connected: false,
                cooling_until: None,
                inflight: 0,
                idle_since: None,
                tools: None,
                resources: None,
                resource_templates: None,
            }),
            connect_serializer: TokioMutex::new(()),
            connect_factory,
            watchdog: StdMutex::new(None),
        }
    }

    /// server 名（config key）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// server 配置快照。
    pub fn server_config(&self) -> &McpServerConfig {
        &self.server_cfg
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConnState> {
        // 锁中毒只可能来自持锁段 panic；仓库惯例 into_inner 继续（backend.rs:235）。
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
        if state.connected && state.service.is_some() {
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

    /// lazy connect（幂等；已连接直接返回）。
    ///
    /// 超时 → `MCP_CONNECT_TIMEOUT`；失败 → `MCP_CONNECT_FAILED`；两者都进入
    /// 重连冷却（设计 §5.1）。冷却期内调用直接报错不重启。并发调用串行化，
    /// 后到者看到已连接结果即返回。
    pub async fn ensure_connected(self: &Arc<Self>) -> Result<(), McpError> {
        {
            let state = self.lock_state();
            if state.connected && state.service.is_some() {
                return Ok(());
            }
        }
        let _serial = self.connect_serializer.lock().await;
        {
            // 拿到串行锁后重查：等待期间可能已有别的调用者完成连接。
            let state = self.lock_state();
            if state.connected && state.service.is_some() {
                return Ok(());
            }
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(McpError::new(
                McpErrorKind::Shutdown,
                format!(
                    "server {}: daemon shutting down; lazy connect rejected",
                    self.name
                ),
            ));
        }
        if let Some(remaining) = self.cooling_remaining() {
            return Err(McpError::new(
                McpErrorKind::ConnectFailed,
                format!(
                    "server {}: reconnect cooldown {:.1}s remaining; retry later",
                    self.name,
                    remaining.as_secs_f32()
                ),
            ));
        }
        // spawn 前最后闸检：缩小“闸落下后仍拉起子进程”的窗口。
        if self.gate.load(Ordering::Relaxed) {
            return Err(McpError::new(
                McpErrorKind::Shutdown,
                format!(
                    "server {}: daemon shutting down; lazy connect rejected",
                    self.name
                ),
            ));
        }

        let connect_timeout = self.settings.connect_timeout;
        let factory = Arc::clone(&self.connect_factory);
        let name = self.name.clone();
        let cfg = self.server_cfg.clone();
        let attempt = tokio::time::timeout(connect_timeout, factory(&name, &cfg)).await;
        match attempt {
            Err(_elapsed) => {
                self.arm_cooldown();
                Err(McpError::new(
                    McpErrorKind::ConnectTimeout,
                    format!(
                        "server {}: connect timed out after {connect_timeout:?}",
                        self.name
                    ),
                ))
            }
            Ok(Err(error)) => {
                self.arm_cooldown();
                Err(McpError::new(
                    McpErrorKind::ConnectFailed,
                    format!("server {}: connect failed: {error}", self.name),
                ))
            }
            Ok(Ok(service)) => {
                if self.gate.load(Ordering::Relaxed) {
                    // 闸在握手期间落下：立即丢弃（drop 链组杀子进程），不纳管。
                    return Err(McpError::new(
                        McpErrorKind::Shutdown,
                        format!(
                            "server {}: daemon shut down during connect; connection discarded",
                            self.name
                        ),
                    ));
                }
                self.store_connected(service);
                // 连接即拉取 tools/list 缓存（设计 §5.3：模型面重建的唯一来源）。
                // 失败只降级（无缓存 → 该 server 暂不出现在模型面），不视为连接失败。
                self.refresh_tools_cache().await;
                // PR-M2-1：同款拉取资源清单与模板（失败仅降级——资源缺失
                // 只影响 `mcp` 聚合工具的可见清单，不影响工具调用路径）。
                self.refresh_resources_cache().await;
                Ok(())
            }
        }
    }

    /// 连接后拉取 `tools/list` 快照入缓存并置脏（幂等；仅成功路径调用）。
    async fn refresh_tools_cache(self: &Arc<Self>) {
        let service = {
            let state = self.lock_state();
            state.service.clone()
        };
        let Some(service) = service else {
            return;
        };
        let fetched = tokio::time::timeout(Duration::from_secs(15), async {
            service.lock().await.list_all_tools().await
        })
        .await;
        match fetched {
            Ok(Ok(tools)) => {
                let count = tools.len();
                {
                    let mut state = self.lock_state();
                    state.tools = Some(Arc::new(tools));
                }
                self.dirty.store(true, Ordering::Relaxed);
                log::info!("[mcp] server {} cached {count} tool(s)", self.name);
            }
            Ok(Err(error)) => {
                log::warn!(
                    "[mcp] server {} tools/list after connect failed: {error} — tools stay unprojected until next connect",
                    self.name
                );
            }
            Err(_elapsed) => {
                log::warn!(
                    "[mcp] server {} tools/list after connect timed out — tools stay unprojected until next connect",
                    self.name
                );
            }
        }
    }

    /// 连接后拉取 `resources/list` + `resources/templates/list` 快照入缓存
    /// （PR-M2-1；幂等，仅成功路径调用）。
    ///
    /// 与 [`Self::refresh_tools_cache`] 同款失败降级：拉不到只影响聚合工具
    /// 的可见清单（提示未连接/无清单），不影响连接与工具调用路径。不置脏
    /// ——投影批次是工具维度；资源清单由 M2-2 的注入块在回合边界直接读快照。
    async fn refresh_resources_cache(self: &Arc<Self>) {
        let service = {
            let state = self.lock_state();
            state.service.clone()
        };
        let Some(service) = service else {
            return;
        };
        let fetch = async {
            let resources = service.lock().await.list_all_resources().await;
            let templates = service.lock().await.list_all_resource_templates().await;
            (resources, templates)
        };
        let fetched = tokio::time::timeout(Duration::from_secs(15), fetch).await;
        match fetched {
            Ok((Ok(resources), Ok(templates))) => {
                let (r, t) = (resources.len(), templates.len());
                {
                    let mut state = self.lock_state();
                    state.resources = Some(Arc::new(resources));
                    state.resource_templates = Some(Arc::new(templates));
                }
                log::info!(
                    "[mcp] server {} cached {r} resource(s) + {t} template(s)",
                    self.name
                );
            }
            Ok((Err(error), _)) | Ok((_, Err(error))) => {
                log::warn!(
                    "[mcp] server {} resources/list after connect failed: {error} — resource list stays empty until next connect",
                    self.name
                );
            }
            Err(_elapsed) => {
                log::warn!(
                    "[mcp] server {} resources/list after connect timed out — resource list stays empty until next connect",
                    self.name
                );
            }
        }
    }

    /// 当前工具缓存（只读快照；投影层 [`crate::bridge::take_projection_batch`] 消费）。
    pub fn cached_tools(&self) -> Option<Arc<Vec<rmcp::model::Tool>>> {
        self.lock_state().tools.clone()
    }

    /// 当前资源清单快照（PR-M2-1：聚合工具 `list_resources` 的数据源；
    /// 只读快照，未连接/未拉取时 `None`）。
    pub fn cached_resources(&self) -> Option<Arc<Vec<rmcp::model::Resource>>> {
        self.lock_state().resources.clone()
    }

    /// 当前资源模板快照（PR-M2-1：uriTemplate 展开提示的来源）。
    pub fn cached_resource_templates(&self) -> Option<Arc<Vec<rmcp::model::ResourceTemplate>>> {
        self.lock_state().resource_templates.clone()
    }

    fn store_connected(self: &Arc<Self>, service: ClientService) {
        {
            let mut state = self.lock_state();
            state.service = Some(Arc::new(TokioMutex::new(service)));
            state.connected = true;
            state.cooling_until = None;
            state.idle_since = Some(Instant::now());
        }
        // PR-M2-2：注册通知桥（Weak 表；断连/释放时注销）。
        conn_notify_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(self.name.clone(), Arc::downgrade(self));
        // 组长 pid（adapter 在 spawn 时登记）；清扫/兜底组杀用（Unix）。
        if let Some(pid) = crate::adapter::take_spawn_pid(&self.name) {
            *self
                .pgid
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pid);
        }
        self.ensure_watchdog();
    }

    /// 组杀兜底清扫（Unix；见 adapter.rs 头注的 rmcp graceful 漏杀缺口）。
    ///
    /// close/crash 后组内可能仍有 server 自行 spawn 的后代——向整组发
    /// SIGKILL（组已空则 ESRCH 无害）。一次性取走 pgid，防重复清扫。
    /// pgid 为 adapter spawn 的独立进程组（`ProcessGroup::leader`），不含
    /// daemon 自身；pid 复用撞组的窗口在 daemon 生命周期内可忽略（清扫
    /// 紧跟 close/crash，且进程组以组长 pid 命名——组长已死则组随最后一个
    /// 成员退出即消亡）。
    #[cfg(unix)]
    fn sweep_group(&self, reason: &str) {
        let pgid = self
            .pgid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(pgid) = pgid else {
            return;
        };
        let reason = reason.to_owned();
        bridge::runtime_handle().spawn(async move {
            // SAFETY：killpg 为纯信号系统调用；pgid 是本 daemon spawn 的
            // 独立进程组（不含自身组），目标进程均以 daemon 用户运行。
            let probe = unsafe { libc::killpg(pgid as libc::pid_t, 0) };
            if probe == 0 {
                let killed = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
                if killed == 0 {
                    log::info!("[mcp] group sweep ({reason}): killpg({pgid}, SIGKILL) sent");
                } else {
                    log::warn!(
                        "[mcp] group sweep ({reason}): killpg({pgid}) errno={}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        });
    }

    /// idle watchdog：`inflight == 0` 连续 `idle_shutdown_secs` → 优雅关闭
    /// （E-2 语义；否决"无调用即计时"）。`idle_secs == 0`（常驻）不启动。
    fn ensure_watchdog(self: &Arc<Self>) {
        if self.idle_secs == 0 {
            return;
        }
        let mut slot = self
            .watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let respawn = match slot.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if respawn {
            *slot = Some(self.spawn_watchdog_task());
        }
    }

    fn spawn_watchdog_task(self: &Arc<Self>) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let tick = self.settings.idle_tick;
        let idle = Duration::from_secs(self.idle_secs);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(conn) = weak.upgrade() else {
                    return; // 连接已释放
                };
                if conn.gate.load(Ordering::Relaxed) {
                    return; // 关闭闸：禁止 idle 重启路径，watchdog 无存在意义
                }
                let taken = {
                    let mut state = conn.lock_state();
                    if !state.connected {
                        return; // 已断连（shutdown/crash），watchdog 退出
                    }
                    let due = state.inflight == 0
                        && state
                            .idle_since
                            .is_some_and(|since| since.elapsed() >= idle);
                    if due {
                        state.connected = false;
                        state.idle_since = None;
                        state.service.take()
                    } else {
                        None
                    }
                };
                if let Some(service) = taken {
                    log::info!(
                        "[mcp] server {} idle for {}s with 0 inflight — reclaiming",
                        conn.name,
                        idle.as_secs()
                    );
                    conn.close_service(service).await;
                    return; // 回收完成；下次连接时重新拉起 watchdog
                }
            }
        })
    }

    /// 开启一次调用：占用 inflight（防 idle 回收），返回 RAII 守卫。
    ///
    /// 未连接时返回 `MCP_CONNECT_FAILED`——调用方应先走
    /// [`Self::ensure_connected`]（M1-5 桥接的固定次序）。
    pub fn begin_call(self: &Arc<Self>) -> Result<CallGuard, McpError> {
        if self.gate.load(Ordering::Relaxed) {
            return Err(McpError::new(
                McpErrorKind::Shutdown,
                format!("server {}: daemon shutting down; call rejected", self.name),
            ));
        }
        {
            let mut state = self.lock_state();
            if !state.connected || state.service.is_none() {
                return Err(McpError::new(
                    McpErrorKind::ConnectFailed,
                    format!(
                        "server {}: not connected (ensure_connected must run first)",
                        self.name
                    ),
                ));
            }
            // max_concurrent_calls 生效点（M1-5）：并发上限按 server 配置隔离。
            if state.inflight >= u64::from(self.server_cfg.max_concurrent_calls) {
                return Err(McpError::new(
                    McpErrorKind::Busy,
                    format!(
                        "server {}: {}/{} in-flight calls — max_concurrent_calls reached; retry after they drain",
                        self.name, state.inflight, self.server_cfg.max_concurrent_calls
                    ),
                ));
            }
            state.inflight += 1;
            state.idle_since = None;
        }
        Ok(CallGuard {
            conn: Arc::clone(self),
        })
    }

    /// 连通性探针：`tools/list` 自动翻页取全部工具名。
    ///
    /// 设计 §5.1 的 connect 验收动作；M1-4 在此基础上加缓存与白名单过滤。
    /// transport 断连 → 标记崩溃并返回 `MCP_SERVER_CRASHED`（当前调用不重试）。
    pub async fn probe_tools(self: &Arc<Self>) -> Result<Vec<String>, McpError> {
        let _guard = self.begin_call()?;
        let service = {
            let state = self.lock_state();
            state.service.clone().ok_or_else(|| {
                McpError::new(
                    McpErrorKind::ConnectFailed,
                    format!("server {}: service dropped before probe", self.name),
                )
            })?
        };
        let result = service.lock().await.list_all_tools().await;
        match result {
            Ok(tools) => Ok(tools.iter().map(|tool| tool.name.to_string()).collect()),
            Err(ServiceError::TransportClosed | ServiceError::TransportSend(_)) => {
                let detail = "transport closed (server crashed or exited)";
                self.handle_crash(detail);
                Err(McpError::new(
                    McpErrorKind::ServerCrashed,
                    format!("server {}: {detail}", self.name),
                ))
            }
            Err(other) => Err(McpError::new(
                McpErrorKind::Protocol,
                format!("server {}: tools/list failed: {other}", self.name),
            )),
        }
    }

    /// server 通知（tools/list_changed、resources/list_changed）触发的重拉
    /// 任务（PR-M2-2）：重拉 tools + resources 两清单并置脏，下个回合边界
    /// 批次重建/注入块刷新即同步。gate 检查防关闭后空拉；并发通知幂等
    /// （多任务都拉最新快照，最后写胜出，代价可忽略）。
    pub(crate) fn spawn_refresh_lists(self: &Arc<Self>) {
        if self.gate.load(Ordering::Relaxed) {
            return;
        }
        let conn = Arc::clone(self);
        bridge::runtime_handle().spawn(async move {
            conn.refresh_tools_cache().await;
            conn.refresh_resources_cache().await;
            log::info!(
                "[mcp] server {} refreshed lists after server notification",
                conn.name
            );
        });
    }

    /// 崩溃处置（设计 §5.1「执行中失败」）：标记断连并立即丢弃 service
    /// 工具调用 RPC（`tools/call` 透传；超时/错误码映射在桥接层完成）。
    ///
    /// - `timeout` 由桥接层按 ctx 超时链传入；此处 `tokio::time::timeout`
    ///   硬顶释放 service 锁——超时后连接保持健康（TransportClosed 才算
    ///   crash），server 侧可能仍在执行（§7 hint）。
    /// - 断连（子进程退出/半端关闭）→ [`Self::handle_crash`] 并返回
    ///   `MCP_SERVER_CRASHED`（当前调用不重试，下次调用单次重启）。
    pub async fn call_tool(
        self: &Arc<Self>,
        tool: &str,
        args: Option<rmcp::model::JsonObject>,
        timeout: Duration,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let _guard = self.begin_call()?;
        let service = {
            let state = self.lock_state();
            state.service.clone().ok_or_else(|| {
                McpError::new(
                    McpErrorKind::ConnectFailed,
                    format!("server {}: service dropped before call", self.name),
                )
            })?
        };
        let params = match args {
            Some(map) => {
                rmcp::model::CallToolRequestParams::new(tool.to_owned()).with_arguments(map)
            }
            None => rmcp::model::CallToolRequestParams::new(tool.to_owned()),
        };
        let attempt = tokio::time::timeout(timeout, async {
            service.lock().await.call_tool(params).await
        })
        .await;
        match attempt {
            Err(_elapsed) => Err(McpError::new(
                McpErrorKind::Timeout,
                format!(
                    "server {}: tool {tool:?} timed out after {timeout:?} — server may still be executing",
                    self.name
                ),
            )),
            Ok(Ok(result)) => Ok(result),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => {
                let detail = format!("transport closed during tool {tool:?} call");
                self.handle_crash(&detail);
                Err(McpError::new(
                    McpErrorKind::ServerCrashed,
                    format!("server {}: {detail}", self.name),
                ))
            }
            Ok(Err(other)) => Err(McpError::new(
                McpErrorKind::Protocol,
                format!("server {}: tools/call {tool:?} failed: {other}", self.name),
            )),
        }
    }

    /// 读取单个资源（PR-M2-1；`resources/read` RPC 透传，内容不缓存）。
    ///
    /// 与 [`Self::call_tool`] 同款保障：`begin_call` 占 inflight（防 idle
    /// 回收竞态）、超时硬顶释放 service 锁、断连 → crash 标记 +
    /// `MCP_SERVER_CRASHED`。读取失败（server 报错，如 uri 不存在）→
    /// `MCP_TOOL_ERROR`（server 侧错误透传，设计 §7）。
    pub async fn read_resource(
        self: &Arc<Self>,
        uri: &str,
        timeout: Duration,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        let _guard = self.begin_call()?;
        let service = {
            let state = self.lock_state();
            state.service.clone().ok_or_else(|| {
                McpError::new(
                    McpErrorKind::ConnectFailed,
                    format!("server {}: service dropped before read", self.name),
                )
            })?
        };
        let params = rmcp::model::ReadResourceRequestParams::new(uri.to_owned());
        let attempt = tokio::time::timeout(timeout, async {
            service.lock().await.read_resource(params).await
        })
        .await;
        match attempt {
            Err(_elapsed) => Err(McpError::new(
                McpErrorKind::Timeout,
                format!(
                    "server {}: read {uri:?} timed out after {timeout:?} — server may still be processing",
                    self.name
                ),
            )),
            Ok(Ok(result)) => Ok(result),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => {
                let detail = format!("transport closed during resource {uri:?} read");
                self.handle_crash(&detail);
                Err(McpError::new(
                    McpErrorKind::ServerCrashed,
                    format!("server {}: {detail}", self.name),
                ))
            }
            Ok(Err(other)) => Err(McpError::new(
                McpErrorKind::ToolError,
                format!(
                    "server {}: resources/read {uri:?} failed: {other}",
                    self.name
                ),
            )),
        }
    }

    /// best-effort 发送 `notifications/cancelled`（设计 §5.4：命中 cancel 时
    /// 调用；发送失败仅 debug 日志，不回传给调用方）。
    ///
    /// `request_id` 置 `None`：rmcp 在 `call_tool` 内部生成请求 id，调用方
    /// 拿不到——取消语义退化为“通知 server 放弃最近一次可取消的工作”，
    /// 结果本来就已被桥接层丢弃（丢弃即契约，不依赖 server 遵从）。
    pub fn try_send_cancelled(self: &Arc<Self>, reason: &str) {
        let service = {
            let state = self.lock_state();
            state.service.clone()
        };
        let Some(service) = service else {
            return;
        };
        let reason = reason.to_owned();
        bridge::runtime_handle().spawn(async move {
            let notification = rmcp::model::ClientNotification::CancelledNotification(
                rmcp::model::Notification::new(rmcp::model::CancelledNotificationParam::new(
                    None,
                    Some(reason),
                )),
            );
            if let Err(error) = service.lock().await.send_notification(notification).await {
                log::debug!("[mcp] cancelled notification send failed (best-effort): {error}");
            }
        });
    }

    /// 崩溃处置（设计 §5.1「执行中失败」）：标记断连并立即丢弃 service    /// （drop 链 → transport drop → 子进程组 kill，见 adapter.rs 文档）。
    /// 本函数**不**设冷却——下一次调用触发单次重启，重启失败才进冷却。
    /// 工具缓存同步清空并置脏：模型面重建（投影批次）与连接状态保持一致。
    pub fn handle_crash(&self, detail: &str) {
        // PR-M2-2：先注销通知桥（Weak 表；重连后 store_connected 重注册）。
        conn_notify_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.name);
        let (service, had_tools) = {
            let mut state = self.lock_state();
            state.connected = false;
            state.idle_since = None;
            let had_tools = state.tools.take().is_some();
            // PR-M2-1：资源缓存与工具同生命周期——crash 即清（下次重连重拉）。
            state.resources = None;
            state.resource_templates = None;
            (state.service.take(), had_tools)
        };
        drop(service);
        if had_tools {
            self.dirty.store(true, Ordering::Relaxed);
        }
        log::warn!("[mcp] server {} crashed: {detail}", self.name);
        // crash 后 server 的后代（若 server 自己 spawn 过子进程）成孤儿风险
        // 最高——兜底组杀（Unix；Windows 由 JobObject 全树覆盖）。
        #[cfg(unix)]
        self.sweep_group("crash");
    }

    /// 优雅关闭单连接（幂等；`manager.shutdown_all` / Drop 网络使用）。
    ///
    /// 次序（设计 §5.1 退出清理）：abort watchdog → cancel service（含
    /// transport close → 子进程 graceful shutdown → 组杀兜底）。
    pub async fn shutdown(&self) {
        // PR-M2-2：注销通知桥（优雅关闭后不再响应 server 通知）。
        conn_notify_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.name);
        self.abort_watchdog();
        let service = {
            let mut state = self.lock_state();
            state.connected = false;
            state.idle_since = None;
            state.service.take()
        };
        if let Some(service) = service {
            self.close_service(service).await;
        }
    }

    async fn close_service(&self, service: Arc<TokioMutex<ClientService>>) {
        let close_timeout = self.settings.close_timeout;
        let mut guard = service.lock().await;
        if let Err(error) = guard.close_with_timeout(close_timeout).await {
            log::warn!("[mcp] server {} close errored: {error}", self.name);
        }
        drop(guard);
        // rmcp graceful 只等直接子进程；组内残余（server 自 spawn 的后代）
        // 由兜底清扫收割（Unix；Windows 由 JobObject 全树覆盖）。
        #[cfg(unix)]
        self.sweep_group("close");
    }

    fn abort_watchdog(&self) {
        if let Some(handle) = self
            .watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

impl Drop for ServerConnection {
    fn drop(&mut self) {
        // 尽力终止 watchdog（watchdog 持 Weak，不会反过来阻止本 drop）。
        self.abort_watchdog();
        // state.service 若仍有值（未经 shutdown/handle_crash），随 drop 链释放：
        // RunningService（DropGuard 取消 ct）→ transport → ChildWithCleanup →
        // 组杀。前提：此时 tokio runtime 仍在（集成方须在 runtime 结束前显式
        // shutdown_all——rmcp 的 kill 任务依赖 runtime spawn）。
    }
}

/// 在飞调用守卫：drop 时归还 inflight 并起算 idle 计时（E-2）。
pub struct CallGuard {
    conn: Arc<ServerConnection>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut state = self
            .conn
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.inflight = state.inflight.saturating_sub(1);
        if state.inflight == 0 {
            state.idle_since = Some(Instant::now());
        }
    }
}
