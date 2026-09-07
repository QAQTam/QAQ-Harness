//! Daemon discovery: read `daemon.json` from the platform data directory and
//! derive the HTTP base URL. The on-disk contract (`DaemonDiscovery` /
//! `CONTROL_PROTOCOL_VERSION`) is single-sourced in `qaqh-types` (PR-3-1);
//! this module owns only the client-side filesystem access and URL derivation.

use crate::error::{ClientError, Result};

pub use qaqh_types::DaemonDiscovery;

/// Client-side discovery extensions: `DaemonDiscovery` 定义于 `qaqh-types`，
/// 固有 impl 无法跨 crate 附加，故 `base_url` 推导落在本扩展 trait。
pub trait DiscoveryExt {
    /// HTTP base URL derived from discovery endpoint.
    /// Supports both legacy `ws://` (→ `http://`) and new `http://`/`https://`.
    fn base_url(&self) -> Result<String>;
}

impl DiscoveryExt for DaemonDiscovery {
    fn base_url(&self) -> Result<String> {
        let (rest, scheme) = if let Some(r) = self.endpoint.strip_prefix("ws://") {
            (r, "http")
        } else if let Some(r) = self.endpoint.strip_prefix("wss://") {
            (r, "https")
        } else if let Some(r) = self.endpoint.strip_prefix("http://") {
            (r, "http")
        } else if let Some(r) = self.endpoint.strip_prefix("https://") {
            (r, "https")
        } else {
            return Err(ClientError::Discovery(format!(
                "unexpected endpoint: {}",
                self.endpoint
            )));
        };
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() {
            return Err(ClientError::Discovery("endpoint has no host".into()));
        }
        Ok(format!("{scheme}://{host}"))
    }
}

/// Platform data directory — single-sourced in `qaqh_types::platform::data_dir`
/// (daemon and client must resolve the same data root).
pub use qaqh_types::platform::data_dir;

/// Path to the discovery file — re-export of the disk-contract helper.
pub use qaqh_types::platform::daemon_discovery_path as discovery_path;

/// Read and parse the discovery file.
pub fn read_discovery() -> Result<DaemonDiscovery> {
    let path = discovery_path();
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ClientError::Discovery(format!("cannot read {}: {e}", path.display())))?;
    let discovery: DaemonDiscovery = serde_json::from_str(&raw)
        .map_err(|e| ClientError::Discovery(format!("invalid {}: {e}", path.display())))?;
    Ok(discovery)
}

/// Ensure a daemon is running and publish its discovery.
///
/// Synchronous (no tokio runtime needed): spawns `qaqh-daemon run` detached
/// when no discovery file exists, then polls for up to `timeout`. Reuses an
/// existing discovery when the daemon process is alive.
pub fn ensure_daemon_running(timeout: std::time::Duration) -> Result<DaemonDiscovery> {
    if let Ok(discovery) = read_discovery()
        && process_is_running(discovery.pid)
    {
        return Ok(discovery);
    }
    // 已有 daemon 实例正在启动（lock 持有者存活但 discovery 尚未发布——
    // daemon 冷启动初始化可达数十秒，discovery 延迟到 HTTP 就绪后才写）：
    // 不重复 spawn，直接轮询等待其发布。
    if !lock_holder_alive() {
        spawn_daemon_detached()?;
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match read_discovery() {
            Ok(discovery) if process_is_running(discovery.pid) => return Ok(discovery),
            Ok(_) => {}
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::Discovery(
                "daemon did not publish discovery in time".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

/// 检查 `daemon.lock` 持有者进程是否存活（daemon 单实例锁，见
/// `qaqh-daemon::server::acquire_single_instance`）。lock 持有者活着即
/// 意味着有 daemon 正在启动/运行，即使 `daemon.json` 尚未发布。
/// `pub(crate)`：`client::wait_for_daemon` 在 spawn 前据此避免重复拉起。
pub(crate) fn lock_holder_alive() -> bool {
    #[cfg(not(windows))]
    {
        // 非 Windows 无 pid 判活实现（`process_is_running` stub 恒 true），
        // 回退旧行为：始终允许 spawn，由 daemon 侧单实例锁兜底。
        false
    }
    #[cfg(windows)]
    {
        let lock = data_dir().join("daemon.lock");
        match std::fs::read_to_string(&lock)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            Some(pid) => process_is_running(pid),
            None => false,
        }
    }
}

/// Resolve the daemon executable.
///
/// Candidate order (first hit wins):
///   1. `QAQH_BACKEND_ROOT/target/debug/qaqh-daemon` — dev
///   2. `<cwd>/target/debug/qaqh-daemon` — dev
///   3. `<exe_dir>/resources/qaqh-daemon` — packaged layout (installer keeps
///      the daemon inside the shell's resources dir; mirrors Electron sidecar)
///   4. `<exe_dir>/qaqh-daemon` — side-by-side layout
///   5. bare name (PATH lookup)
pub fn daemon_executable() -> std::path::PathBuf {
    let exe = if cfg!(windows) {
        "qaqh-daemon.exe"
    } else {
        "qaqh-daemon"
    };

    for base in [
        std::env::var("QAQH_BACKEND_ROOT").ok(),
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let p = std::path::PathBuf::from(base)
            .join("target")
            .join("debug")
            .join(exe);
        if p.exists() {
            return p;
        }
    }

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        for base in [dir.join("resources"), dir.clone()] {
            let p = base.join(exe);
            if p.exists() {
                return p;
            }
        }
    }

    std::path::PathBuf::from(exe)
}

fn spawn_daemon_detached() -> Result<()> {
    let executable = daemon_executable();
    log::info!("[qaqh-client] spawning daemon: {}", executable.display());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new(&executable)
            .arg("run")
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(&executable)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
}

/// Process liveness probe — client-side implementation, deliberately distinct
/// from `qaqh_types::platform::process_is_running` (which shells out to
/// `tasklist`/`kill`): this one uses the Win32 API directly (no subprocess
/// latency) and treats non-Windows discovery presence as sufficient. Do NOT
/// merge the two; they serve different perf/semantics envelopes.
#[cfg(windows)]
pub fn process_is_running(pid: u32) -> bool {
    let handle = unsafe {
        windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        false
    }
    let exit_code = unsafe {
        let mut code: u32 = 0;
        windows_sys::Win32::System::Threading::GetExitCodeProcess(handle, &mut code);
        code
    };
    unsafe {
        let _ = windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    exit_code == 259 // STILL_ACTIVE
}

#[cfg(not(windows))]
pub fn process_is_running(_pid: u32) -> bool {
    true // discovery presence is the check on non-Windows for now
}

#[cfg(test)]
mod tests {
    use super::*;

    // frontend-contract.md §1 冻结面锚点：discovery endpoint 的兼容解析。
    // 旧形态 ws://host:port/control/v1 必须无损转 http://，新形态原样通过；
    // 破坏任一分支即破坏已发布客户端的 discovery 兼容。
    #[test]
    fn base_url_accepts_legacy_ws_and_new_http_forms() {
        let legacy = DaemonDiscovery {
            endpoint: "ws://127.0.0.1:9101/control/v1".into(),
            token: String::new(),
            pid: 0,
            server_epoch: String::new(),
            protocol_version: 1,
            daemon_version: String::new(),
            build_id: String::new(),
            channel: String::new(),
            executable: String::new(),
        };
        assert_eq!(legacy.base_url().unwrap(), "http://127.0.0.1:9101");

        let modern = DaemonDiscovery {
            endpoint: "http://127.0.0.1:9101".into(),
            ..legacy
        };
        assert_eq!(modern.base_url().unwrap(), "http://127.0.0.1:9101");
    }

    // PR-3-1 兼容红线：旧格式 daemon.json（无 build_id/channel/executable 三字段）
    // 必须可解析，且解析 → 序列化 → 再解析字段逐字段保全。
    #[test]
    fn legacy_discovery_json_roundtrip_preserves_fields() {
        let legacy = r#"{
            "endpoint": "http://127.0.0.1:41831",
            "token": "tok-123",
            "pid": 4242,
            "server_epoch": "epoch-1",
            "protocol_version": 1,
            "daemon_version": "0.8.9"
        }"#;
        let parsed: DaemonDiscovery =
            serde_json::from_str(legacy).expect("legacy 6-field sample must parse");
        assert_eq!(parsed.endpoint, "http://127.0.0.1:41831");
        assert_eq!(parsed.token, "tok-123");
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.server_epoch, "epoch-1");
        assert_eq!(parsed.protocol_version, 1);
        assert_eq!(parsed.daemon_version, "0.8.9");
        // 新增字段缺省（pre-0.9 兼容语义）
        assert_eq!(parsed.build_id, "");
        assert_eq!(parsed.channel, "");
        assert_eq!(parsed.executable, "");

        let json = serde_json::to_string(&parsed).expect("serialize");
        let reparsed: DaemonDiscovery = serde_json::from_str(&json).expect("reparse");
        assert_eq!(reparsed, parsed);
    }
}
