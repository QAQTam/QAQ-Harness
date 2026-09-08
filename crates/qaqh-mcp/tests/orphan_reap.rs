//! PR-M1-5 孤儿进程专项（PLAN §4 出口）：`cargo test -p qaqh-mcp --test orphan_reap` 全绿。
//!
//! Linux 专项（Windows `tasklist /T` 验证归双平台备注，见 PLAN §4 M1-5 行）：
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `shutdown_reaps_whole_group` | 三层树（node→fixture→grandchild）：`shutdown_all` 后 `pgrep -g <pgid>` 空 |
//! | `connect_failure_leaves_no_group` | deaf（握手不回应）connect 超时 → drop 链组杀 → 组空 |
//! | `fixture_exits_on_stdin_eof` | daemon 侧 SIGKILL 的兜底语义：stdin EOF → fixture 自退（防孤儿最后一道闸） |
//!
//! 组杀证据链：adapter spawn 设独立进程组（ProcessGroup::leader）→ pid 文件
//! 记录 server/孙进程 pid → shutdown 前读 `/proc/<pid>/stat` 取 pgrp →
//! shutdown 后 `pgrep -g <pgrp>` 退出码 1（组内零进程）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::LifecycleSettings;

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_server.mjs")
}

fn pid_file(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "qaqh-mcp-orphan-pid-{}-{tag}.txt",
        std::process::id()
    ))
}

fn server_cfg(tag: &str, mode: &str) -> McpServerConfig {
    let pids = pid_file(tag);
    let _ = std::fs::remove_file(&pids);
    McpServerConfig {
        transport: McpTransportKind::Stdio,
        command: "node".to_owned(),
        args: vec![fixture_path().to_string_lossy().to_string()],
        env: BTreeMap::from([
            (
                "QAQH_TEST_PID_FILE".to_owned(),
                pids.to_string_lossy().to_string(),
            ),
            ("QAQH_TEST_MODE".to_owned(), mode.to_owned()),
        ]),
        url: String::new(),
        headers: BTreeMap::new(),
        tools: None,
        resources_enabled: true,
        default_timeout_secs: 60,
        max_concurrent_calls: 4,
        cwd: String::new(),
    }
}

fn cfg_with(tag: &str, mode: &str, idle_secs: u64) -> McpConfig {
    McpConfig {
        import_external: false,
        enabled: true,
        idle_shutdown_secs: idle_secs,
        servers: BTreeMap::from([(tag.to_owned(), server_cfg(tag, mode))]),
    }
}

fn short_settings() -> LifecycleSettings {
    LifecycleSettings {
        connect_timeout: Duration::from_millis(500),
        reconnect_cooldown: Duration::from_millis(300),
        close_timeout: Duration::from_secs(2),
        idle_tick: Duration::from_millis(50),
    }
}

/// 轮询直到 pid 文件出现，返回行集合（server pid 在首行）。
fn wait_pid_lines(tag: &str, expect: usize) -> Vec<u32> {
    let path = pid_file(tag);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "pid file never appeared: {path:?}"
        );
        if let Ok(text) = std::fs::read_to_string(&path) {
            let lines: Vec<u32> = text
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect();
            if lines.len() >= expect {
                return lines;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 僵尸感知的存活判定：`Z` = 已死未收尸（父进程尚未 wait）——组杀语义下
/// 视为“已退出”，否则短暂退出的进程会让 /proc 探测误报存活。
fn pid_alive(pid: u32) -> bool {
    match pid_state(pid) {
        Some(state) => state != "Z",
        None => false,
    }
}

/// /proc/<pid>/stat 的 state 字段（R/S/Z/T…）；None = 进程不存在。
#[cfg(unix)]
fn pid_state(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 1..)?;
    rest.split_whitespace().next().map(str::to_owned)
}

/// 轮询直到所有 pid 消失（/proc 验证）。
fn wait_all_gone(pids: &[u32], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !pid_alive(*pid)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    pids.iter().all(|pid| !pid_alive(*pid))
}

/// 从 /proc/<pid>/stat 取进程组 id（第 5 字段；comm 可能含空格，取最后一个 ')'）。
fn pgrp_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    // rest 以空格开头；字段从 state 开始计数：state(1) ppid(2) pgrp(3)…
    let rest = stat.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(2)?.parse().ok()
}

/// `pgrep -g <pgid>`：退出码 1 = 组内零进程（2+ = pgrep 自身错误）。
fn group_empty(pg: u32) -> bool {
    let status = std::process::Command::new("pgrep")
        .arg("-g")
        .arg(pg.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("pgrep must run (procps installed)");
    !status.success()
}

// ── 用例 1：三层树组杀 ──

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_reaps_whole_group() {
    let manager = McpManager::with_settings(cfg_with("tree", "grandchild", 0), short_settings());
    let conn = manager.get_or_connect("tree").await.unwrap();
    let pids = wait_pid_lines("tree", 2);
    let (server_pid, grandchild_pid) = (pids[0], pids[1]);
    assert!(
        pid_alive(grandchild_pid),
        "grandchild 必须在 shutdown 前存活"
    );

    let pg = pgrp_of(server_pid).expect("server pgrp readable");
    assert!(
        pgrp_of(grandchild_pid) == Some(pg),
        "grandchild 必须与 server 同组（继承，未 setsid）"
    );

    manager.shutdown_all().await;

    assert!(
        wait_all_gone(&[server_pid, grandchild_pid], Duration::from_secs(5)),
        "shutdown 后必须全部退出：server({server_pid})={:?} grandchild({grandchild_pid})={:?}",
        pid_state(server_pid),
        pid_state(grandchild_pid)
    );
    assert!(group_empty(pg), "pgrep -g {pg} 必须为空——组杀覆盖整棵树");
    let _ = conn;
}

// ── 用例 2：connect 失败路径不留孤儿 ──

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_failure_leaves_no_group() {
    let manager = McpManager::with_settings(cfg_with("deaf", "deaf", 0), short_settings());
    let attempt =
        tokio::time::timeout(Duration::from_secs(5), manager.get_or_connect("deaf")).await;
    assert!(
        attempt.is_err() || attempt.unwrap().is_err(),
        "deaf 必须连接失败/超时"
    );

    let pid = wait_pid_lines("deaf", 1)[0];
    let pg = pgrp_of(pid).expect("pgrp readable");
    assert!(
        wait_all_gone(&[pid], Duration::from_secs(5)),
        "connect 失败后 deaf 子进程（{pid}）必须被 drop 链组杀"
    );
    assert!(group_empty(pg), "组 {pg} 必须为空");
}

// ── 用例 3：stdin EOF 自退（daemon 侧死亡时的最后一道孤儿闸）──

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_exits_on_stdin_eof() {
    // 模拟 daemon SIGKILL：daemon 死亡 → 所有 fd 关闭 → 子进程 stdin EOF
    // → fixture `rl.on("close") → process.exit(0)` 自退（无需任何外部清理）。
    let mut child = std::process::Command::new("node")
        .arg(fixture_path())
        .env("QAQH_TEST_MODE", "normal")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fixture");
    let pid = child.id();

    // 显式关闭 stdin（模拟 daemon 死亡后的 fd 全关）。
    drop(child.stdin.take());

    let gone = wait_all_gone(&[pid], Duration::from_secs(5));
    // 兜底收尾（wait 避免 clippy spawned-not-waited 告警与进程泄漏）。
    let _ = child.kill();
    let _ = child.wait();
    assert!(gone, "stdin EOF 后 fixture 必须自退（pid {pid} 仍存活）");
}
