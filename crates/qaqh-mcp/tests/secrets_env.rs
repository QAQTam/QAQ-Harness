//! PR-M1-3 e2e（qaqh-mcp 侧）：`${secret:name}` 从 secrets.toml 一路解析进
//! 子进程 env 的全链路证明（设计 §6/E-4）。
//!
//! 链路：`McpManager(with_secret_store)` → adapter `resolve_server_secrets`
//! （连接时解析）→ `CommandWrap` env 注入 → node fixture 把收到的 env 回写
//! pid 文件 → 测试比对。断言三件事：
//! 1. 子进程收到**解析值**（非占位符、非 `<unset>`）；
//! 2. 非占位符 env 原样透传；
//! 3. 停机后子进程组照常 reap（M1-2 链路在插值路径下不回归）。
//!
//! 无全局状态；DPAPI 往返在 qaqh-config 侧测试（cfg(windows)，M1-5 双平台补跑）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_config::secrets::SecretStore;
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::LifecycleSettings;

const E2E_SECRET: &str = "sk-e2e-SECRET-42";

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_server.mjs")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_placeholder_resolves_into_child_env() {
    let dir = std::env::temp_dir().join(format!("qaqh-mcp-secrets-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("dump.txt");

    let secrets = SecretStore::new(dir.join("secrets.toml"));
    secrets.set_mcp("e2e_key", E2E_SECRET).unwrap();

    let server = McpServerConfig {
        transport: McpTransportKind::Stdio,
        command: "node".to_owned(),
        args: vec![fixture_path().to_string_lossy().to_string()],
        env: BTreeMap::from([
            (
                "QAQH_TEST_PID_FILE".to_owned(),
                pid_file.to_string_lossy().to_string(),
            ),
            ("QAQH_TEST_MODE".to_owned(), "envdump".to_owned()),
            // 被解析的占位符 + 透传的普通值
            ("API_KEY".to_owned(), "${secret:e2e_key}".to_owned()),
        ]),
        url: String::new(),
        headers: BTreeMap::new(),
        tools: None,
        resources_enabled: true,
        default_timeout_secs: 60,
        max_concurrent_calls: 1,
    };
    let cfg = McpConfig {
        enabled: true,
        idle_shutdown_secs: 0,
        servers: BTreeMap::from([("echoenv".to_owned(), server)]),
    };
    let manager = McpManager::with_secret_store(cfg, LifecycleSettings::default(), secrets);

    // 连接（fixture 完成握手并回写 pid 文件）。
    let conn = manager.get_or_connect("echoenv").await.unwrap();
    let tools = tokio::time::timeout(Duration::from_secs(15), conn.probe_tools())
        .await
        .expect("握手应在 15s 内完成")
        .unwrap();
    assert_eq!(
        tools,
        vec!["echo", "slow"],
        "fixture 现暴露 echo/slow（M1-5 起含 slow）"
    );

    // 读 pid 文件三行：pid / API_KEY 回显 / MODE 回显。
    let dump = std::fs::read_to_string(&pid_file).expect("fixture 应写 pid 文件");
    let lines: Vec<&str> = dump.lines().collect();
    assert_eq!(lines.len(), 3, "envdump 三行：{dump:?}");
    assert_eq!(lines[1], E2E_SECRET, "子进程收到的是解析值，而非占位符");
    assert_ne!(lines[1], "${secret:e2e_key}", "占位符绝不能原样到达子进程");
    assert_eq!(lines[2], "envdump", "非占位符 env 原样透传");

    // 停机 reap（插值路径不回归 M1-2 的 RAII 链）。
    let pid: u32 = lines[0].parse().unwrap();
    manager.shutdown_all().await;
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!path.exists(), "子进程组应被 reap（pid {pid} 仍在）");

    let _ = std::fs::remove_dir_all(&dir);
}
