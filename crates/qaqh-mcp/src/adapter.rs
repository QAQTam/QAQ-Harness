//! rmcp / process-wrap 类型隔离层（设计 §4：SDK 类型不出 crate，换 SDK 只改这里）。
//!
//! M1-2 职责（handover §五草图）：
//! - [`build_stdio_command`]：stdio server → 进程组隔离的 `CommandWrap`
//!   （Unix `ProcessGroup::leader()` / Windows `JobObject`；env 注入）；
//! - [`connect`]：生产用 connect 工厂（stdio spawn + Auto 握手；http → M3 占位错误）。
//!
//! M1-3 追加：`${secret:name}` 连接时解析（设计 §6/E-4）——
//! [`resolve_server_secrets`] 在 spawn 前把 env/args/headers 里的占位符换成
//! `secrets.toml` 真值；结果只进本次子进程 env，**不回存**（`Config.mcp` 与
//! DTO/save 永远只见占位符）。失败 → `MCP_CONNECT_FAILED`，消息含字段与
//! secret 名、绝不含值（E-6 红线）。
//!
//! ## RAII 链（rmcp 3.2.0 源码实测，PLAN §7）
//!
//! - `TokioChildProcess` 被 drop 时，其 `ChildWithCleanup::drop` 会 spawn kill
//!   任务 → 经 `ProcessGroupChild::kill()` 走 `killpg` 整组（含
//!   `waitpid(-pgid)` 僵尸回收）；Windows 侧 `JobObjectChild` 走 job 全树。
//!   **无单 pid kill**（E-1 红线）。
//! - `Transport::close()` = `graceful_shutdown`（关 stdin → 等子进程退出 ≤3s →
//!   兜底 kill），由 `RunningService::close_with_timeout` 正常触发。
//!   **PR-M1-5 缺口补丁**：graceful_shutdown 只等**直接子进程**——server 自行
//!   spawn 的孙进程（npx→node→server 三层树常见）在 server 自退时漏杀成孤儿。
//!   兜底：spawn 后 [`record_spawn_pid`] 登记 pgid，连接层在 close/crash 后
//!   取走并发组杀清扫（connection.rs `sweep_group`；orphan_reap.rs 验证整树）。
//! - connect 失败/超时路径：`serve` future 被 drop → transport drop → 同上 kill 链。
//! - rmcp builder 的 stderr 默认 `Stdio::inherit()`（会漏进 daemon 日志）——
//!   本层显式置 `Stdio::null()`（设计 §5.5：stderr 默认丢弃，计数留痕归 M1-5）。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Mutex as StdMutex, OnceLock};

use process_wrap::tokio::CommandWrap;
use qaqh_config::config::{McpServerConfig, McpTransportKind, interpolate_secret_placeholders};
use qaqh_config::secrets::SecretStore;
use rmcp::model::ProtocolVersion;
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, NotificationContext};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

use crate::connection::{ConnectFuture, notify_lists_changed};
use crate::error::{McpError, McpErrorKind};

/// PR-M2-2：客户端通知桥——server 发出 `tools/list_changed` /
/// `resources/list_changed` 时按 name 触发连接层重拉（清单 + 模板重新
/// 缓存并置脏，下个回合边界批次重建/注入块刷新即同步）。其余通知走
/// ClientHandler 默认 no-op。doc(hidden)：类型经 `ClientService` 别名
/// 穿透到测试 mock factory（构造即用，通知不触发），非公共 API 契约。
#[doc(hidden)]
pub struct NotifyBridge {
    #[doc(hidden)]
    pub name: String,
}

impl ClientHandler for NotifyBridge {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        notify_lists_changed(&self.name);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        notify_lists_changed(&self.name);
    }
}

/// spawn 后的 pgid 登记表（server 名 → 直接子进程 pid；`ProcessGroup::leader`
/// 使其即为进程组长 id）。连接层在 close/crash 后取走并做组杀兜底清扫——
/// 绕过 rmcp `graceful_shutdown` 只等直接子进程的漏杀缺口（见模块头注）。
/// 同名重复 connect 会覆盖旧值（连接层每代连接各自取走，窗口内由
/// connect_serializer 串行化保证无竞争）。
fn spawn_pid_slot() -> &'static StdMutex<BTreeMap<String, u32>> {
    static SPAWN_PIDS: OnceLock<StdMutex<BTreeMap<String, u32>>> = OnceLock::new();
    SPAWN_PIDS.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

/// 登记 spawn 的直接子进程 pid（即 pgid）。
pub(crate) fn record_spawn_pid(server: &str, pid: u32) {
    spawn_pid_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(server.to_owned(), pid);
}

/// 取走登记的 pgid（清扫入口；None = 尚无登记或已被取走）。
pub(crate) fn take_spawn_pid(server: &str) -> Option<u32> {
    spawn_pid_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(server)
}

/// 设计 §5.1 点名的 Auto 生命周期：优先 `server/discover`（协议 2026-07-28），
/// 无响应/不支持时回退 legacy `initialize`（2025-11-25；10s 计时 rmcp 内建）。
pub(crate) fn auto_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// stdio server → 进程组隔离的 [`CommandWrap`]（E-1：必须独立进程组）。
///
/// env 必须是**已解析**的值（见 [`resolve_server_secrets`]）——本函数不做
/// 占位符处理。
pub(crate) fn build_stdio_command(cfg: &McpServerConfig) -> CommandWrap {
    let mut command = tokio::process::Command::new(&cfg.command);
    command.args(&cfg.args);
    command.envs(cfg.env.iter());
    let mut wrap = CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    // creation-flags feature 与 job-object 配套（handover §四探针结论）。
    wrap.wrap(JobObject);
    wrap
}

/// 连接时解析：env/args/headers 里的 `${secret:name}` → secrets.toml 真值。
///
/// 解析结果仅用于本次 spawn，不回存任何运行时结构（`Config.mcp` 保持占位符
/// ——DTO/save 永不见明文）。每次连接重新读取：对密钥轮换友好（改
/// secrets.toml → 下次重连生效）。失败 → `MCP_CONNECT_FAILED`，消息含字段
/// 与 secret 名、绝不含值（E-6）。
pub(crate) fn resolve_server_secrets(
    cfg: &McpServerConfig,
    secrets: &SecretStore,
) -> Result<McpServerConfig, McpError> {
    Ok(McpServerConfig {
        env: resolve_map(&cfg.env, "env", secrets)?,
        headers: resolve_map(&cfg.headers, "headers", secrets)?,
        args: resolve_args(&cfg.args, secrets)?,
        ..cfg.clone()
    })
}

fn resolve_map(
    map: &BTreeMap<String, String>,
    label: &str,
    secrets: &SecretStore,
) -> Result<BTreeMap<String, String>, McpError> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let resolved = interpolate(value, secrets).map_err(|e| {
            McpError::new(McpErrorKind::ConnectFailed, format!("{label}[{key}]: {e}"))
        })?;
        out.insert(key.clone(), resolved);
    }
    Ok(out)
}

fn resolve_args(args: &[String], secrets: &SecretStore) -> Result<Vec<String>, McpError> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| {
            interpolate(arg, secrets).map_err(|e| {
                McpError::new(McpErrorKind::ConnectFailed, format!("args[{index}]: {e}"))
            })
        })
        .collect()
}

fn interpolate(value: &str, secrets: &SecretStore) -> Result<String, String> {
    interpolate_secret_placeholders(value, |name| secrets.load_mcp(name))
}

/// 生产 connect 工厂（`McpManager` 默认装配；store 由 manager 注入）：
/// stdio → 解析占位符 + spawn + Auto 握手。
///
/// streamable HTTP（设计 §6 `[mcp.servers.*].url`）归 M3——在 connect 时报
/// `ConnectFailed` 而非启动期拒绝，保持"配置仅静态校验"的边界（M1-1 已校验
/// url 形态；此处是运行期能力边界）。
pub(crate) fn connect(name: &str, cfg: &McpServerConfig, secrets: &SecretStore) -> ConnectFuture {
    let resolved = match resolve_server_secrets(cfg, secrets) {
        Ok(resolved) => resolved,
        Err(error) => return Box::pin(async move { Err(Box::new(error) as _) }),
    };
    let name = name.to_owned();
    Box::pin(async move {
        match resolved.transport {
            McpTransportKind::Stdio => {
                let command = build_stdio_command(&resolved);
                log::info!(
                    "[mcp] server {name}: spawning stdio transport (command={:?})",
                    resolved.command
                );
                let (transport, _stderr) = TokioChildProcess::builder(command)
                    .stderr(Stdio::null())
                    .spawn()?;
                // 组长 pid 即 pgid（ProcessGroup::leader）；登记供连接层兜底组杀。
                if let Some(pid) = transport.id() {
                    record_spawn_pid(&name, pid);
                }
                let service = NotifyBridge { name: name.clone() }
                    .serve_with_lifecycle(transport, auto_lifecycle())
                    .await?;
                Ok(service)
            }
            McpTransportKind::Http => {
                Err("streamable HTTP transport lands in M3 ".to_owned().into())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::McpErrorKind;

    fn store_with(name: &str, value: &str) -> SecretStore {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-mcp-adapter-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore::new(dir.join("secrets.toml"));
        store.set_mcp(name, value).unwrap();
        store
    }

    fn server_cfg(env: BTreeMap<String, String>, args: Vec<String>) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransportKind::Stdio,
            command: "node".to_owned(),
            args,
            env,
            url: String::new(),
            headers: BTreeMap::new(),
            tools: None,
            resources_enabled: true,
            default_timeout_secs: 60,
            max_concurrent_calls: 1,
        }
    }

    #[test]
    fn resolve_replaces_placeholders_and_keeps_plain_values() {
        let secrets = store_with("ctx_key", "sk-abc123XYZ");
        let cfg = server_cfg(
            BTreeMap::from([
                ("API_KEY".to_owned(), "${secret:ctx_key}".to_owned()),
                ("MODE".to_owned(), "production".to_owned()),
                ("AUTH".to_owned(), "Bearer ${secret:ctx_key}".to_owned()),
            ]),
            vec!["--key=${secret:ctx_key}".to_owned()],
        );
        let resolved = resolve_server_secrets(&cfg, &secrets).unwrap();
        assert_eq!(resolved.env.get("API_KEY").unwrap(), "sk-abc123XYZ");
        assert_eq!(resolved.env.get("MODE").unwrap(), "production");
        assert_eq!(resolved.env.get("AUTH").unwrap(), "Bearer sk-abc123XYZ");
        assert_eq!(resolved.args[0], "--key=sk-abc123XYZ");
        // 原配置不被污染（占位符保留 → DTO/save 安全）。
        assert_eq!(cfg.env.get("API_KEY").unwrap(), "${secret:ctx_key}");
    }

    #[test]
    fn resolve_missing_secret_fails_without_value_leak() {
        let secrets = store_with("ctx_key", "sk-abc123XYZ");
        let cfg = server_cfg(
            BTreeMap::from([("API_KEY".to_owned(), "${secret:nope}".to_owned())]),
            vec![],
        );
        let error = resolve_server_secrets(&cfg, &secrets).unwrap_err();
        assert_eq!(error.kind, McpErrorKind::ConnectFailed);
        assert!(error.message.contains("nope"), "报错应指明 secret 名");
        assert!(!error.message.contains("sk-abc123"), "值绝不能进错误信息");
    }

    #[test]
    fn resolve_malformed_placeholder_fails() {
        let secrets = store_with("ctx_key", "sk-abc123XYZ");
        let cfg = server_cfg(
            BTreeMap::from([("API_KEY".to_owned(), "${secret:Bad Name}".to_owned())]),
            vec![],
        );
        let error = resolve_server_secrets(&cfg, &secrets).unwrap_err();
        assert_eq!(error.kind, McpErrorKind::ConnectFailed);
    }
}
