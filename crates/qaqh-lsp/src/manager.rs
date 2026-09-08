//! [`LspManager`]：daemon 级 LSP 客户端生命周期容器（mcp manager.rs 同款）。
//!
//! - 归属：`QaqhService` 组装，`install_manager` 全局槽位；
//! - 连接键：`(server, root)` 双键（M1 决策 L3）——同一 server 不同 root 是
//!   不同连接，root 取会话 cwd；
//! - 关闭：[`LspManager::shutdown_all`] 置 `shutting_down` 闸 → 逐连接优雅关闭。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use qaqh_config::secrets::SecretStore;

use crate::connection::{LifecycleSettings, ServerConnection};
use crate::error::{LspError, LspErrorKind};
use qaqh_config::config::LspConfig;

/// [`LspManager::apply_config`] 的 diff 报告（热重载日志/观测用；mcp 同款）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub kept: Vec<String>,
}

fn conn_key(server: &str, root: &str) -> String {
    format!("{server}\0{root}")
}

/// daemon 级单例：配置快照 + 连接表 + 关闭闸。
pub struct LspManager {
    cfg: StdMutex<LspConfig>,
    settings: LifecycleSettings,
    gate: Arc<AtomicBool>,
    conns: StdMutex<BTreeMap<String, Arc<ServerConnection>>>,
    secret_store: SecretStore,
}

impl LspManager {
    pub fn new(cfg: LspConfig) -> Arc<Self> {
        Self::build(cfg, LifecycleSettings::default(), None)
    }

    pub fn with_settings(cfg: LspConfig, settings: LifecycleSettings) -> Arc<Self> {
        Self::build(cfg, settings, None)
    }

    pub fn with_secret_store(
        cfg: LspConfig,
        settings: LifecycleSettings,
        secret_store: SecretStore,
    ) -> Arc<Self> {
        Self::build(cfg, settings, Some(secret_store))
    }

    fn build(
        cfg: LspConfig,
        settings: LifecycleSettings,
        secret_store: Option<SecretStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg: StdMutex::new(cfg),
            settings,
            gate: Arc::new(AtomicBool::new(false)),
            conns: StdMutex::new(BTreeMap::new()),
            secret_store: secret_store.unwrap_or_else(SecretStore::default_location),
        })
    }

    /// 禁用配置的空 manager（全局槽位默认值；所有调用报 `LSP_DISABLED`）。
    pub fn disabled() -> Arc<Self> {
        Self::new(LspConfig::default())
    }

    /// 配置快照（热重载下配置可变——返回 clone 而非引用；mcp 同款）。
    pub fn config(&self) -> LspConfig {
        self.lock_cfg().clone()
    }

    fn lock_cfg(&self) -> std::sync::MutexGuard<'_, LspConfig> {
        self.cfg
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot_cfg(&self) -> LspConfig {
        self.lock_cfg().clone()
    }

    fn set_cfg(&self, cfg: LspConfig) {
        *self.lock_cfg() = cfg;
    }

    pub fn shutting_down(&self) -> bool {
        self.gate.load(Ordering::Relaxed)
    }

    /// 已纳管的连接（只读查看，不触发连接；测试/指标用）。
    pub fn connection(&self, server: &str, root: &str) -> Option<Arc<ServerConnection>> {
        self.lock_conns().get(&conn_key(server, root)).cloned()
    }

    /// 全部已纳管连接快照（不触发连接）。
    pub fn connections(&self) -> Vec<Arc<ServerConnection>> {
        self.lock_conns().values().cloned().collect()
    }

    /// 热重载——新 `[lsp]` 配置并入运行时（免重启 daemon；mcp 同款 diff 语义）。
    ///
    /// 注意：连接键含 root——diff 只比 server 配置面；root 维度由调用方
    /// （会话 cwd 变化 → 旧 root 连接 idle 回收自然收敛，不主动杀）。
    pub async fn apply_config(self: &Arc<Self>, new_cfg: LspConfig) -> ApplyReport {
        let mut report = ApplyReport::default();
        let old_servers = self.snapshot_cfg().servers;

        let mut removed_names = Vec::new();
        for (name, old_server_cfg) in &old_servers {
            match new_cfg.servers.get(name) {
                None => removed_names.push(name.clone()),
                Some(new_server_cfg) if new_server_cfg != old_server_cfg => {
                    report.updated.push(name.clone());
                    removed_names.push(name.clone());
                }
                Some(_) => report.kept.push(name.clone()),
            }
        }
        // 同一 server 名可能有多个 root 连接——按 server 名前缀清扫。
        let keys: Vec<String> = self
            .lock_conns()
            .keys()
            .filter(|k| {
                removed_names
                    .iter()
                    .any(|n| k.as_str() == n.as_str() || k.starts_with(&format!("{n}\0")))
            })
            .cloned()
            .collect();
        for key in &keys {
            let conn = {
                let mut conns = self.lock_conns();
                conns.remove(key)
            };
            if let Some(conn) = conn {
                conn.shutdown().await;
            }
        }
        report.removed = removed_names;

        for name in new_cfg.servers.keys() {
            if !old_servers.contains_key(name) {
                report.added.push(name.clone());
            }
        }

        if !new_cfg.enabled {
            let conns: Vec<Arc<ServerConnection>> = {
                let mut map = self.lock_conns();
                std::mem::take(&mut *map).into_values().collect()
            };
            for conn in &conns {
                conn.shutdown().await;
            }
            report.removed.append(&mut report.kept);
        }

        self.set_cfg(new_cfg);
        report
    }

    /// 取连接并确保已连接（lazy connect 入口；幂等）。
    ///
    /// 拒绝路径：`enabled=false` → `Disabled`；闸已落下 → `Shutdown`；
    /// 未知 server → `NotFound`（附可用名单）；连接失败/超时/冷却 → 对应错误码。
    pub async fn get_or_connect(
        &self,
        server: &str,
        root: &str,
    ) -> Result<Arc<ServerConnection>, LspError> {
        let cfg_snapshot = self.snapshot_cfg();
        if !cfg_snapshot.enabled {
            return Err(LspError::new(
                LspErrorKind::Disabled,
                "[lsp].enabled=false — enable LSP in config.toml to use LSP tools".to_owned(),
            ));
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                "daemon is shutting down; LSP calls rejected".to_owned(),
            ));
        }
        let server_cfg = cfg_snapshot.servers.get(server).cloned().ok_or_else(|| {
            let available: Vec<&str> = cfg_snapshot.servers.keys().map(String::as_str).collect();
            LspError::new(
                LspErrorKind::NotFound,
                format!("unknown LSP server {server:?}; configured: {available:?}"),
            )
        })?;
        let key = conn_key(server, root);
        let conn = {
            let mut conns = self.lock_conns();
            if let Some(existing) = conns.get(&key) {
                Arc::clone(existing)
            } else {
                let conn = Arc::new(ServerConnection::new(
                    server.to_owned(),
                    root.to_owned(),
                    server_cfg,
                    cfg_snapshot.idle_shutdown_secs,
                    self.settings.clone(),
                    Arc::clone(&self.gate),
                    self.secret_store.clone(),
                ));
                conns.insert(key, Arc::clone(&conn));
                conn
            }
        };
        conn.ensure_connected().await?;
        Ok(conn)
    }

    /// 按文件扩展名路由到 server（M1 决策 L1 路由键）。
    ///
    /// `path` 取 `.` 后小写后缀；首个声明该扩展名的 server 获胜（配置顺序
    /// = BTreeMap 字母序，确定性）；冲突在启动日志由调用方记录（tool 层）。
    pub fn server_for_extension(&self, path: &str) -> Option<String> {
        let ext = path.rsplit('.').next()?.trim().to_ascii_lowercase();
        if ext.is_empty() || ext == path.trim().to_ascii_lowercase() {
            return None;
        }
        // 去掉查询串/行号污染：只取扩展名中的字母数字前缀。
        let ext: String = ext.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if ext.is_empty() {
            return None;
        }
        self.snapshot_cfg()
            .servers
            .iter()
            .find(|(_, cfg)| cfg.extensions.iter().any(|e| e == &ext))
            .map(|(name, _)| name.clone())
    }

    /// 预热全部已声明 server（root 缺省为 daemon cwd；mcp prime 同款鸡生蛋修复）。
    pub async fn prime_all(&self, root: &str) {
        if !self.snapshot_cfg().enabled {
            return;
        }
        for name in self.snapshot_cfg().servers.keys() {
            if let Err(error) = self.get_or_connect(name, root).await {
                log::warn!("[lsp] prime {name}: {error}");
            }
        }
    }

    /// 全部已声明 server 的状态行（`lsp` 聚合工具 `list_servers` 数据源；
    /// 不触发连接——未连接的 server 显示 disconnected）。
    pub fn server_status_lines(&self) -> Vec<String> {
        let shutting = self.gate.load(Ordering::Relaxed);
        self.snapshot_cfg()
            .servers
            .keys()
            .map(|name| {
                // 同名多 root 连接合并为一行（连接数标注）。
                let conns: Vec<Arc<ServerConnection>> = self
                    .lock_conns()
                    .iter()
                    .filter(|(k, _)| k.as_str() == name.as_str() || k.starts_with(&format!("{name}\0")))
                    .map(|(_, v)| Arc::clone(v))
                    .collect();
                let state = if conns.is_empty() {
                    "disconnected".to_owned()
                } else {
                    let parts: Vec<String> = conns
                        .iter()
                        .map(|conn| match conn.status() {
                            crate::connection::ConnStatus::Connected { .. } => {
                                format!("connected@{}", conn.root())
                            }
                            crate::connection::ConnStatus::Cooling { remaining_ms } => {
                                format!("cooling ({}s left)", remaining_ms / 1000)
                            }
                            crate::connection::ConnStatus::Disconnected => {
                                format!("disconnected@{}", conn.root())
                            }
                            crate::connection::ConnStatus::ShuttingDown => {
                                "shutting_down".to_owned()
                            }
                        })
                        .collect();
                    parts.join(", ")
                };
                let suffix = if shutting { " [shutting down]" } else { "" };
                format!("{name}: {state}{suffix}")
            })
            .collect()
    }

    /// 关闭全部连接并落下 `shutting_down` 闸。
    pub async fn shutdown_all(&self) {
        self.gate.store(true, Ordering::Relaxed);
        let conns: Vec<Arc<ServerConnection>> = {
            let mut conns = self.lock_conns();
            std::mem::take(&mut *conns).into_values().collect()
        };
        for conn in &conns {
            conn.shutdown().await;
        }
        log::info!("[lsp] shutdown_all complete ({} connections)", conns.len());
    }

    fn lock_conns(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<ServerConnection>>> {
        self.conns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(extensions: Vec<&str>) -> LspConfig {
        LspConfig {
            enabled: true,
            idle_shutdown_secs: 120,
            servers: BTreeMap::from([(
                "rust".to_owned(),
                qaqh_config::config::LspServerConfig {
                    command: "rust-analyzer".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    extensions: extensions.into_iter().map(str::to_owned).collect(),
                    startup_timeout_secs: 30,
                    default_timeout_secs: 30,
                },
            )]),
        }
    }

    #[test]
    fn server_for_extension_routes_by_suffix() {
        let manager = LspManager::new(cfg_with(vec!["rs"]));
        assert_eq!(
            manager.server_for_extension("src/main.rs"),
            Some("rust".to_owned())
        );
        assert_eq!(manager.server_for_extension("SRC/MAIN.RS"), Some("rust".to_owned()));
        assert_eq!(manager.server_for_extension("Makefile"), None);
        assert_eq!(manager.server_for_extension("noext"), None);
    }

    #[test]
    fn disabled_manager_rejects_with_disabled_code() {
        let manager = LspManager::disabled();
        assert!(manager.server_for_extension("a.rs").is_none());
    }
}
