//! [`McpManager`]：daemon 级 MCP 客户端生命周期容器（设计 §3 / §10-6）。
//!
//! - 归属：`QaqhService` 组装（设计 §10-6 决议），本 crate 只提供容器与
//!   全局槽位（OnceLock 模式，照抄 `qaqh-workspace/src/backend.rs:227`）。
//! - 语义：配置声明即信任（D4/D5）；daemon 启动不做网络操作——连接全部
//!   lazy（[`McpManager::get_or_connect`] → [`ServerConnection::ensure_connected`]）。
//! - 关闭：[`McpManager::shutdown_all`] 置 `shutting_down` 闸（对齐
//!   AgentRegistry 同名字段语义）→ 逐连接优雅关闭；`Drop` 为兜底网络。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use qaqh_config::secrets::SecretStore;

use crate::bridge;
use crate::connection::{ConnectFactory, LifecycleSettings, ServerConnection};
use crate::error::{McpError, McpErrorKind};
use qaqh_config::config::McpConfig;

/// daemon 级单例：配置快照 + 连接表 + 关闭闸。
///
/// 配置热重载归 Phase 2（设计 §6）——M1-2 以构建时快照为准。
pub struct McpManager {
    cfg: McpConfig,
    settings: LifecycleSettings,
    gate: Arc<AtomicBool>,
    /// 任一连接的工具缓存变化时置位；`take_projection_batch` 消费后复位。
    dirty: Arc<AtomicBool>,
    conns: StdMutex<BTreeMap<String, Arc<ServerConnection>>>,
    /// None → 生产工厂（adapter stdio）；测试注入 in-memory。
    factory_override: Option<ConnectFactory>,
    /// `${secret:name}` 解析源（连接时读取；见 adapter.rs）。
    secret_store: SecretStore,
}

impl McpManager {
    /// 生产构造：默认生命周期参数 + adapter 工厂。
    pub fn new(cfg: McpConfig) -> Arc<Self> {
        Self::build(cfg, LifecycleSettings::default(), None, None)
    }

    /// 生产工厂 + 自定义生命周期参数（测试需要缩短 10s 超时/5s 冷却）。
    pub fn with_settings(cfg: McpConfig, settings: LifecycleSettings) -> Arc<Self> {
        Self::build(cfg, settings, None, None)
    }

    /// 生产工厂 + 自定义 secret store（测试插值/多实例数据根）。
    pub fn with_secret_store(
        cfg: McpConfig,
        settings: LifecycleSettings,
        secret_store: SecretStore,
    ) -> Arc<Self> {
        Self::build(cfg, settings, None, Some(secret_store))
    }

    /// 测试/扩展构造：自定义生命周期参数与 connect 工厂。
    pub fn with_connect_factory(
        cfg: McpConfig,
        settings: LifecycleSettings,
        factory: ConnectFactory,
    ) -> Arc<Self> {
        Self::build(cfg, settings, Some(factory), None)
    }

    fn build(
        cfg: McpConfig,
        settings: LifecycleSettings,
        factory_override: Option<ConnectFactory>,
        secret_store: Option<SecretStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            settings,
            gate: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
            conns: StdMutex::new(BTreeMap::new()),
            factory_override,
            secret_store: secret_store.unwrap_or_else(SecretStore::default_location),
        })
    }

    /// 禁用配置的空 manager（全局槽位默认值；所有调用报 `MCP_DISABLED`）。
    pub fn disabled() -> Arc<Self> {
        Self::new(McpConfig::default())
    }

    /// 配置快照（只读）。
    pub fn config(&self) -> &McpConfig {
        &self.cfg
    }

    /// `shutting_down` 闸状态。
    pub fn shutting_down(&self) -> bool {
        self.gate.load(Ordering::Relaxed)
    }

    /// 已纳管的连接（只读查看，不触发连接；测试/指标/投影批次用）。
    pub fn connection(&self, server: &str) -> Option<Arc<ServerConnection>> {
        self.lock_conns().get(server).cloned()
    }

    /// 全部已纳管连接快照（投影批次遍历用；不触发连接）。
    pub fn connections(&self) -> Vec<Arc<ServerConnection>> {
        self.lock_conns().values().cloned().collect()
    }

    /// 取走“工具缓存已变脏”标记（投影批次消费入口；幂等：无脏返回 false）。
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// 取连接并确保已连接（lazy connect 入口；幂等）。
    ///
    /// 拒绝路径（设计 §5.1）：`enabled=false` → `MCP_DISABLED`；闸已落下 →
    /// `MCP_SHUTDOWN`；未知 server → `MCP_NOT_FOUND`（附可用名单）；连接
    /// 失败/超时/冷却 → 对应错误码。
    pub async fn get_or_connect(&self, server: &str) -> Result<Arc<ServerConnection>, McpError> {
        if !self.cfg.enabled {
            return Err(McpError::new(
                McpErrorKind::Disabled,
                "[mcp].enabled=false — enable MCP in config.toml to use MCP tools".to_owned(),
            ));
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(McpError::new(
                McpErrorKind::Shutdown,
                "daemon is shutting down; MCP calls rejected".to_owned(),
            ));
        }
        let server_cfg = self.cfg.servers.get(server).cloned().ok_or_else(|| {
            let available: Vec<&str> = self.cfg.servers.keys().map(String::as_str).collect();
            McpError::new(
                McpErrorKind::NotFound,
                format!("unknown MCP server {server:?}; configured: {available:?}"),
            )
        })?;
        let conn = {
            let mut conns = self.lock_conns();
            if let Some(existing) = conns.get(server) {
                Arc::clone(existing)
            } else {
                let factory = self.factory_for();
                let conn = Arc::new(ServerConnection::new(
                    server.to_owned(),
                    server_cfg,
                    self.cfg.idle_shutdown_secs,
                    self.settings.clone(),
                    Arc::clone(&self.gate),
                    Arc::clone(&self.dirty),
                    factory,
                ));
                conns.insert(server.to_owned(), Arc::clone(&conn));
                conn
            }
        };
        conn.ensure_connected().await?;
        Ok(conn)
    }

    /// 预热全部已声明 server（投影可见性闭环）：逐个 `get_or_connect`（幂等）。
    ///
    /// 没有预热时存在先有鸡还是先有蛋：连接唯一触发点是工具执行（dispatch），
    /// 而工具要被调用必须先投影进工具表，投影只在连接后的批次里——全新
    /// daemon 上模型永远看不到 MCP 工具。逐 server 失败只告警不短路（部分
    /// 可用优于全部不可用）；成功连接即缓存 tools/list 并置脏，下个回合边界
    /// `take_projection_batch` 即可拉到批次。
    pub async fn prime_all(&self) {
        if !self.cfg.enabled {
            return;
        }
        for name in self.cfg.servers.keys() {
            if let Err(error) = self.get_or_connect(name).await {
                log::warn!("[mcp] prime {name}: {error}");
            }
        }
    }

    /// 关闭全部连接并落下 `shutting_down` 闸（设计 §5.1 退出清理）。
    ///
    /// 闸落下后 lazy connect / 重连 / idle 重启全部被拒；已建立连接优雅关闭
    /// （service cancel → transport close → 子进程组杀兜底）。
    pub async fn shutdown_all(&self) {
        self.gate.store(true, Ordering::Relaxed);
        let conns: Vec<Arc<ServerConnection>> = {
            let mut conns = self.lock_conns();
            std::mem::take(&mut *conns).into_values().collect()
        };
        for conn in &conns {
            conn.shutdown().await;
        }
        log::info!("[mcp] shutdown_all complete ({} connections)", conns.len());
    }

    fn factory_for(&self) -> ConnectFactory {
        match &self.factory_override {
            Some(factory) => Arc::clone(factory),
            None => {
                let secrets = self.secret_store.clone();
                Arc::new(move |name, cfg| crate::adapter::connect(name, cfg, &secrets))
            }
        }
    }

    fn lock_conns(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<ServerConnection>>> {
        self.conns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.gate.store(true, Ordering::Relaxed);
        let pending: Vec<Arc<ServerConnection>> = {
            let mut conns = self.lock_conns();
            std::mem::take(&mut *conns).into_values().collect()
        };
        if pending.is_empty() {
            return;
        }
        // 兜底网络：有 runtime 则派生关闭任务（连接 Arc 已被移出，独立存活）；
        // 无 runtime（daemon 退出末端）只能依赖"集成方在 runtime 结束前显式
        // shutdown_all"的约定——rmcp 的 kill 任务需要 runtime（adapter.rs 注）。
        let pending_len = pending.len();
        let current = tokio::runtime::Handle::try_current().ok();
        let spawned = bridge::try_runtime_handle()
            .or(current.as_ref())
            .map(|handle| {
                handle.spawn(async move {
                    for conn in pending {
                        conn.shutdown().await;
                    }
                });
            });
        if spawned.is_none() {
            log::warn!(
                "[mcp] manager dropped without tokio runtime; {pending_len} connection(s) left to drop-chain reaping — call shutdown_all before teardown"
            );
        }
    }
}
