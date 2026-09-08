//! rmcp / process-wrap 类型隔离层（mcp adapter.rs 同款：SDK 类型不出 crate）。
//!
//! - [`build_stdio_command`]：stdio server → 进程组隔离的 `CommandWrap`
//!   （Unix `ProcessGroup::leader()` / Windows `JobObject`；env 注入 +
//!   `cwd` 透传——path-sensitive server 用 cwd 钉住工作区）；
//! - [`spawn_pipes`]：spawn 后拆出 stdin/stdout（stderr 默认 null，mcp §5.5
//!   同款）+ 登记 pgid（组杀兜底）；
//! - [`resolve_server_secrets`]：`${secret:name}` 连接时解析（mcp M1-3 同款，
//!   secret 段复用 `[secrets.mcp]`，结果只进本次子进程 env，不回存）。
//!
//! ## RAII 链（mcp adapter.rs 头注同款）
//!
//! - 子进程句柄 drop → kill 任务 → `killpg` 整组；Windows 侧 job 全树；
//! - stderr 默认 `Stdio::null()`（防漏进 daemon 日志）。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Mutex as StdMutex, OnceLock};

use process_wrap::tokio::CommandWrap;
use qaqh_config::config::{LspServerConfig, interpolate_secret_placeholders};
use qaqh_config::secrets::SecretStore;

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;

use crate::error::{LspError, LspErrorKind};

/// spawn 后的 pgid 登记表（server 名 → 直接子进程 pid；`ProcessGroup::leader`
/// 使其即为进程组长 id）。连接层在 close/crash 后取走并做组杀兜底清扫——
/// mcp adapter.rs 头注的 rmcp graceful 漏杀缺口同款。
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

/// stdio server → 进程组隔离的 [`CommandWrap`]（E-1：必须独立进程组）。
///
/// env 必须是**已解析**的值（见 [`resolve_server_secrets`]）——本函数不做
/// 占位符处理。`cwd` 为空时继承 daemon cwd（tokio 默认行为，不显式设）。
pub(crate) fn build_stdio_command(cfg: &LspServerConfig, root: &str) -> CommandWrap {
    let mut command = tokio::process::Command::new(&cfg.command);
    command.args(&cfg.args);
    command.envs(cfg.env.iter());
    command.current_dir(root);
    let mut wrap = CommandWrap::from(command);
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);
    wrap
}

/// spawn 子进程并拆出 stdio 管道（stdin/stdout piped，stderr null）。
///
/// 调用方须在 spawn 前把 stdin/stdout 设为 piped、stderr 设为 null
/// （见 [`build_stdio_command`] 的配套用法：tokio Command 默认 inherit，
/// mainloop 需要 owned 管道）。返回 child 句柄 + pid——句柄由 connection
/// 持有（drop 即 kill 链，mcp RAII 同款），pid 登记供兜底组杀。
pub(crate) struct SpawnedPipes {
    pub child: Box<dyn process_wrap::tokio::ChildWrapper>,
    pub pid: u32,
}

pub(crate) fn spawn_server(wrap: CommandWrap) -> Result<SpawnedPipes, LspError> {
    let mut wrap = wrap;
    let child = wrap
        .spawn_with(|command| {
            command.stdin(Stdio::piped());
            command.stdout(Stdio::piped());
            command.stderr(null_stderr());
            command.spawn()
        })
        .map_err(|e| LspError::new(LspErrorKind::ConnectFailed, format!("spawn failed: {e}")))?;
    let pid = child.id().ok_or_else(|| {
        LspError::new(
            LspErrorKind::ConnectFailed,
            "spawned LSP server has no pid".to_owned(),
        )
    })?;
    Ok(SpawnedPipes { child, pid })
}

/// 连接时解析：env/args 里的 `${secret:name}` → secrets.toml 真值。
///
/// 解析结果仅用于本次 spawn，不回存任何运行时结构。失败 → `ConnectFailed`，
/// 消息含字段与 secret 名、绝不含值（mcp E-6 红线同款）。
pub(crate) fn resolve_server_secrets(
    cfg: &LspServerConfig,
    secrets: &SecretStore,
) -> Result<LspServerConfig, LspError> {
    Ok(LspServerConfig {
        env: resolve_map(&cfg.env, "env", secrets)?,
        args: resolve_args(&cfg.args, secrets)?,
        ..cfg.clone()
    })
}

fn resolve_map(
    map: &BTreeMap<String, String>,
    label: &str,
    secrets: &SecretStore,
) -> Result<BTreeMap<String, String>, LspError> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let resolved = interpolate(value, secrets)
            .map_err(|e| LspError::new(LspErrorKind::ConnectFailed, format!("{label}[{key}]: {e}")))?;
        out.insert(key.clone(), resolved);
    }
    Ok(out)
}

fn resolve_args(args: &[String], secrets: &SecretStore) -> Result<Vec<String>, LspError> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| {
            interpolate(arg, secrets).map_err(|e| {
                LspError::new(LspErrorKind::ConnectFailed, format!("args[{index}]: {e}"))
            })
        })
        .collect()
}

fn interpolate(value: &str, secrets: &SecretStore) -> Result<String, String> {
    interpolate_secret_placeholders(value, |name| secrets.load_mcp(name))
}

/// stderr 装配：默认 null（mcp §5.5 同款：stderr 默认丢弃）。
pub(crate) fn null_stderr() -> Stdio {
    Stdio::null()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_cfg(env: BTreeMap<String, String>, args: Vec<String>) -> LspServerConfig {
        LspServerConfig {
            command: "rust-analyzer".to_owned(),
            args,
            env,
            extensions: vec!["rs".to_owned()],
            startup_timeout_secs: 30,
            default_timeout_secs: 30,
        }
    }

    fn store_with(name: &str, value: &str) -> SecretStore {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-lsp-adapter-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let store = SecretStore::new(dir.join("secrets.toml"));
        store.set_mcp(name, value).expect("set secret");
        store
    }

    #[test]
    fn resolve_replaces_placeholders_and_keeps_plain_values() {
        let secrets = store_with("lsp_key", "sk-abc123XYZ");
        let cfg = server_cfg(
            BTreeMap::from([
                ("API_KEY".to_owned(), "${secret:lsp_key}".to_owned()),
                ("MODE".to_owned(), "production".to_owned()),
            ]),
            vec!["--key=${secret:lsp_key}".to_owned()],
        );
        let resolved = resolve_server_secrets(&cfg, &secrets).expect("resolve ok");
        assert_eq!(resolved.env.get("API_KEY").unwrap(), "sk-abc123XYZ");
        assert_eq!(resolved.env.get("MODE").unwrap(), "production");
        assert_eq!(resolved.args[0], "--key=sk-abc123XYZ");
        assert_eq!(cfg.env.get("API_KEY").unwrap(), "${secret:lsp_key}");
    }

    #[test]
    fn resolve_missing_secret_fails_without_value_leak() {
        let secrets = store_with("lsp_key", "sk-abc123XYZ");
        let cfg = server_cfg(
            BTreeMap::from([("API_KEY".to_owned(), "${secret:nope}".to_owned())]),
            vec![],
        );
        let error = resolve_server_secrets(&cfg, &secrets).expect_err("must fail");
        assert_eq!(error.kind, LspErrorKind::ConnectFailed);
        assert!(error.message.contains("nope"), "报错应指明 secret 名");
        assert!(!error.message.contains("sk-abc123"), "值绝不能进错误信息");
    }

    #[test]
    fn build_stdio_command_pins_root_as_cwd() {
        let cfg = server_cfg(BTreeMap::new(), vec!["--stdio".to_owned()]);
        let wrap = build_stdio_command(&cfg, "/tmp/qaqh-lsp-probe");
        assert_eq!(
            wrap.command().as_std().get_current_dir(),
            Some(std::path::Path::new("/tmp/qaqh-lsp-probe")),
            "root 应钉为子进程 cwd"
        );
    }
}
