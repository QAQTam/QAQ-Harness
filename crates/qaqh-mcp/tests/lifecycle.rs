//! PR-M1-2 验收（PLAN §4 出口）：`cargo test -p qaqh-mcp --test lifecycle` 全绿。
//!
//! 五用例映射（connect / timeout / crash-reconnect / idle / shutdown，
//! in-memory 为主、子进程覆盖拉起/超时/RAII reap）：
//!
//! | 用例 | 覆盖 | 传输 |
//! |---|---|---|
//! | `connect_in_memory_serves_tools_list` | connect（Auto 回退 legacy） | in-memory |
//! | `connect_timeout_kills_deaf_child` | timeout + 失败路径组杀 | 子进程 |
//! | `crash_marks_disconnected_then_reconnects` | crash-reconnect（下次调用单次重启） | in-memory |
//! | `failed_restart_arms_cooldown` | 重启失败→冷却→期内拒绝→期满恢复 | in-memory |
//! | `idle_reclaim_fires_then_reconnects` | inflight==0 idle 回收 + 重启 | in-memory |
//! | `shutdown_gate_rejects_further_work` | shutdown 闸 + Disabled/NotFound | in-memory |
//! | `subprocess_connect_and_shutdown_reaps_child` | connect + shutdown RAII reap | 子进程 |
//! | `manager_slot_defaults_to_disabled_and_swaps` | 全局槽位默认/装配 | in-memory |
//!
//! 状态隔离：manager 均为本测试私有实例，不触碰全局槽默认值（槽位用例
//! 自装自卸）；无 TEST_RUNTIME_SERIAL 需求。子进程 reap 验证依赖 /proc
//! （Linux；Windows 专项归 M1-5，见 handover §四）。
//!
//! Auto 生命周期注：生产与测试都用 `ClientLifecycleMode::Auto`——legacy
//! mock/fixture 对 `server/discover` 回 -32601（rmcp 默认），客户端据此
//! 回退 legacy initialize，这正是设计 §5.1 要验证的回退链。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::connection::{ConnectFactory, LifecycleSettings};
use qaqh_mcp::error::McpErrorKind;
use qaqh_mcp::{ConnStatus, McpManager, install_manager, manager_slot};
use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RequestContext, ServiceExt};
use serde_json::json;

// ── in-memory mock：M0 模式复用（echo 工具 + legacy ServerHandler）──

#[derive(Debug, Default, Clone)]
struct MockServer;

impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "lifecycle-mock".into();
        info.server_info.version = "0.1.0".into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema = json!({ "type": "object", "properties": {} });
        let tool = Tool::new(
            "echo",
            "echo back",
            Arc::new(schema.as_object().unwrap().clone()),
        );
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }
}

/// mock 工厂的控制面：代数计数 / 注入失败 / 崩溃最新一代。
#[derive(Default)]
struct MockControl {
    attempts: AtomicU32,
    fail_next: AtomicBool,
    generations: StdMutex<Vec<tokio::task::AbortHandle>>,
}

impl MockControl {
    fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::SeqCst)
    }

    /// 崩溃最新一代 mock server（abort → server 半端关闭 → client transport closed）。
    fn crash_latest(&self) {
        let handle = self
            .generations
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("at least one mock generation exists");
        handle.abort();
    }
}

/// in-memory connect 工厂：每次调用生成新的双工对 + mock server 任务。
fn mock_factory(control: Arc<MockControl>) -> ConnectFactory {
    Arc::new(move |name, _cfg| {
        let name = name.to_owned();
        control.attempts.fetch_add(1, Ordering::SeqCst);
        if control.fail_next.swap(false, Ordering::SeqCst) {
            let error: Box<dyn std::error::Error + Send + Sync> =
                "injected connect failure".to_owned().into();
            return Box::pin(async move { Err(error) });
        }
        let control = Arc::clone(&control);
        Box::pin(async move {
            let (client_half, server_half) = tokio::io::duplex(64 * 1024);
            let (c_read, c_write) = tokio::io::split(client_half);
            let (s_read, s_write) = tokio::io::split(server_half);
            let server_task = tokio::spawn(async move {
                let service = MockServer.serve((s_read, s_write)).await.map_err(
                    |e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("mock server init failed: {e}").into()
                    },
                )?;
                service.waiting().await.map_err(
                    |e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("mock server wait failed: {e}").into()
                    },
                )?;
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            });
            control
                .generations
                .lock()
                .unwrap()
                .push(server_task.abort_handle());
            let lifecycle = ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            };
            let receiver = qaqh_mcp::adapter::NotifyBridge { name };
            let service = receiver
                .serve_with_lifecycle((c_read, c_write), lifecycle)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    e.to_string().into()
                })?;
            Ok(service)
        })
    })
}

// ── 测试夹具：配置 / 参数 ──

fn mock_server_cfg() -> McpServerConfig {
    McpServerConfig {
        transport: McpTransportKind::Stdio,
        command: "unused-by-mock".to_owned(),
        args: vec![],
        env: BTreeMap::new(),
        url: String::new(),
        headers: BTreeMap::new(),
        tools: None,
        resources_enabled: true,
        default_timeout_secs: 60,
        max_concurrent_calls: 1,
    }
}

fn mock_cfg(idle_secs: u64) -> McpConfig {
    McpConfig {
        enabled: true,
        idle_shutdown_secs: idle_secs,
        servers: BTreeMap::from([("mock".to_owned(), mock_server_cfg())]),
    }
}

fn test_settings() -> LifecycleSettings {
    LifecycleSettings {
        connect_timeout: Duration::from_millis(500),
        reconnect_cooldown: Duration::from_millis(300),
        close_timeout: Duration::from_secs(2),
        idle_tick: Duration::from_millis(50),
    }
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_server.mjs")
}

fn pid_file(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "qaqh-mcp-lifecycle-pid-{}-{tag}.txt",
        std::process::id()
    ))
}

/// 子进程 server 配置：node fixture + pid 文件/模式注入（adapter env 注入路径）。
fn subprocess_cfg(tag: &str, mode: &str, idle_secs: u64) -> McpConfig {
    let pids = pid_file(tag);
    let _ = std::fs::remove_file(&pids);
    let server = McpServerConfig {
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
        ..mock_server_cfg()
    };
    McpConfig {
        enabled: true,
        idle_shutdown_secs: idle_secs,
        servers: BTreeMap::from([(tag.to_owned(), server)]),
    }
}

/// 轮询直到 pid 文件出现（fixture 启动写入），返回 pid。
async fn wait_pid_file(path: &Path) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// 轮询直到 /proc/<pid> 消失（RAII reap 验证；Linux）。
#[cfg(unix)]
async fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !path.exists()
}

// ── 断言辅助 ──

/// `get_or_connect` 错误路径断言（`Arc<ServerConnection>` 非 Debug，不能用
/// `unwrap_err`——改用 match 提取）。
fn connect_err(
    result: Result<Arc<qaqh_mcp::ServerConnection>, qaqh_mcp::error::McpError>,
) -> qaqh_mcp::error::McpError {
    match result {
        Ok(_) => panic!("expected connect error"),
        Err(error) => error,
    }
}

// ── 用例 1：connect（in-memory，Auto 回退 legacy + tools/list 验收）──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_in_memory_serves_tools_list() {
    let control = Arc::new(MockControl::default());
    let manager = McpManager::with_connect_factory(
        mock_cfg(0),
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );

    let conn = manager.get_or_connect("mock").await.unwrap();
    assert_eq!(conn.name(), "mock");

    let tools = conn.probe_tools().await.unwrap();
    assert_eq!(tools, vec!["echo"], "Auto→legacy 握手后 tools/list 应可用");
    assert_eq!(conn.status(), ConnStatus::Connected { inflight: 0 });
    assert_eq!(control.attempts(), 1, "幂等 get_or_connect 不应重复连接");

    // 复用同一连接：不新增 spawn。
    let again = manager.get_or_connect("mock").await.unwrap();
    assert_eq!(again.name(), "mock");
    assert_eq!(control.attempts(), 1);

    manager.shutdown_all().await;
    assert!(manager.shutting_down());
    assert_eq!(conn.status(), ConnStatus::ShuttingDown);
}

// ── 用例 2：connect 超时（子进程 deaf fixture；失败路径组杀验证）──

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_timeout_kills_deaf_child() {
    // 生产工厂（adapter：真 spawn + Auto 握手）+ 短 connect_timeout。
    let manager = McpManager::with_settings(subprocess_cfg("deaf", "deaf", 0), test_settings());

    let attempt = tokio::time::timeout(Duration::from_secs(5), manager.get_or_connect("deaf"))
        .await
        .expect("connect_timeout=500ms 应先于外层 5s 兜底触发");
    let err = connect_err(attempt);
    assert_eq!(err.kind, McpErrorKind::ConnectTimeout, "实际：{err}");

    // 超时路径：serve future 被 drop → transport drop → 子进程组被 kill。
    let pid = wait_pid_file(&pid_file("deaf"))
        .await
        .expect("deaf fixture 应写入 pid 文件");
    assert!(
        wait_pid_gone(pid, Duration::from_secs(5)).await,
        "connect 超时后 deaf 子进程应被组杀（pid {pid} 仍在）"
    );
}

// ── 用例 3：crash → 标记断连 → 下一次调用单次重启 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_marks_disconnected_then_reconnects() {
    let control = Arc::new(MockControl::default());
    let manager = McpManager::with_connect_factory(
        mock_cfg(0),
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );

    let conn = manager.get_or_connect("mock").await.unwrap();
    assert!(conn.probe_tools().await.is_ok());

    // 模拟 server 崩溃：最新一代 mock server 被 abort → transport closed。
    control.crash_latest();
    let err = conn.probe_tools().await.unwrap_err();
    assert_eq!(err.kind, McpErrorKind::ServerCrashed, "当前调用不重试");
    assert_eq!(conn.status(), ConnStatus::Disconnected);
    // 设计 §5.1：crash 本身不设冷却（重启失败才设）。
    assert!(manager.connection("mock").is_some(), "连接对象仍在纳管");

    // 下一次调用触发单次重启（gen2）。
    manager.get_or_connect("mock").await.unwrap();
    let tools = conn.probe_tools().await.unwrap();
    assert_eq!(tools, vec!["echo"]);
    assert_eq!(control.attempts(), 2, "单次重启恰一次 spawn");

    manager.shutdown_all().await;
}

// ── 用例 4：重启失败 → 冷却 → 期内拒绝 → 期满恢复 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_restart_arms_cooldown() {
    let control = Arc::new(MockControl::default());
    let manager = McpManager::with_connect_factory(
        mock_cfg(0),
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );

    let conn = manager.get_or_connect("mock").await.unwrap();
    assert!(conn.probe_tools().await.is_ok());
    control.crash_latest();
    assert!(conn.probe_tools().await.is_err());

    // 注入重启失败 → 冷却落位。
    control.fail_next.store(true, Ordering::SeqCst);
    let err = connect_err(manager.get_or_connect("mock").await);
    assert_eq!(
        err.kind,
        McpErrorKind::ConnectFailed,
        "重启失败报 CONNECT_FAILED"
    );

    // 冷却期内：直接报错不重启（attempts 不增）。
    let err = connect_err(manager.get_or_connect("mock").await);
    assert_eq!(err.kind, McpErrorKind::ConnectFailed);
    assert!(
        err.message.contains("cooldown"),
        "冷却期报错应说明冷却：{err}"
    );
    assert_eq!(control.attempts(), 2, "冷却期内不应再 spawn");
    assert!(
        matches!(conn.status(), ConnStatus::Cooling { .. }),
        "状态应为 Cooling，实际 {:?}",
        conn.status()
    );

    // 冷却期满（300ms + 余量）：下一次调用恢复成功（gen3）。
    tokio::time::sleep(Duration::from_millis(450)).await;
    manager.get_or_connect("mock").await.unwrap();
    let tools = conn.probe_tools().await.unwrap();
    assert_eq!(tools, vec!["echo"]);
    assert_eq!(control.attempts(), 3);

    manager.shutdown_all().await;
}

// ── 用例 5：idle 回收（inflight==0 连续阈值）→ 下次调用重启 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_reclaim_fires_then_reconnects() {
    let control = Arc::new(MockControl::default());
    let manager = McpManager::with_connect_factory(
        mock_cfg(1), // idle_shutdown_secs = 1s
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );

    let conn = manager.get_or_connect("mock").await.unwrap();
    assert!(conn.probe_tools().await.is_ok());
    assert_eq!(control.attempts(), 1);

    // probe 结束后 inflight==0 起算 idle；1s 阈值 + 50ms tick → 1.4s 内必然回收。
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(
        conn.status(),
        ConnStatus::Disconnected,
        "idle watchdog 应已回收连接"
    );
    assert_eq!(control.attempts(), 1, "idle 回收不产生新 spawn");

    // 下一次调用重新拉起（gen2）。
    manager.get_or_connect("mock").await.unwrap();
    let tools = conn.probe_tools().await.unwrap();
    assert_eq!(tools, vec!["echo"]);
    assert_eq!(control.attempts(), 2);

    manager.shutdown_all().await;
}

// ── 用例 6：shutdown 闸 + Disabled / NotFound 拒绝 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_gate_rejects_further_work() {
    // Disabled：未启用配置 → MCP_DISABLED（零网络操作）。
    let disabled = McpManager::new(McpConfig::default());
    let err = connect_err(disabled.get_or_connect("anything").await);
    assert_eq!(err.kind, McpErrorKind::Disabled);

    let control = Arc::new(MockControl::default());
    let manager = McpManager::with_connect_factory(
        mock_cfg(0),
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );

    // NotFound：未知 server 名。
    let err = connect_err(manager.get_or_connect("nope").await);
    assert_eq!(err.kind, McpErrorKind::NotFound);
    assert!(err.message.contains("mock"), "报错应列出可用 server 名");

    // 正常连接 → shutdown_all → 闸拒绝一切后续动作。
    let conn = manager.get_or_connect("mock").await.unwrap();
    assert!(conn.probe_tools().await.is_ok());
    manager.shutdown_all().await;

    let err = connect_err(manager.get_or_connect("mock").await);
    assert_eq!(err.kind, McpErrorKind::Shutdown);
    let err = conn.ensure_connected().await.unwrap_err();
    assert_eq!(err.kind, McpErrorKind::Shutdown, "重连被闸拒绝");
    assert_eq!(conn.status(), ConnStatus::ShuttingDown);
    assert_eq!(control.attempts(), 1, "闸后不应再 spawn");
}

// ── 用例 7：子进程 connect + shutdown RAII reap ──

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_connect_and_shutdown_reaps_child() {
    let manager = McpManager::new(subprocess_cfg("real", "normal", 0));

    let conn = manager.get_or_connect("real").await.unwrap();
    let tools = tokio::time::timeout(Duration::from_secs(15), conn.probe_tools())
        .await
        .expect("真 stdio 握手应在 15s 内完成（含 Auto 探测回退）")
        .unwrap();
    assert_eq!(
        tools,
        vec!["echo", "slow"],
        "node fixture 应暴露 echo/slow 工具"
    );

    let pid = wait_pid_file(&pid_file("real"))
        .await
        .expect("fixture 应写入 pid 文件");

    manager.shutdown_all().await;
    assert!(
        wait_pid_gone(pid, Duration::from_secs(5)).await,
        "shutdown_all 后子进程组应被 reap（pid {pid} 仍在）"
    );
}

// ── 用例 8：全局槽位（默认 disabled → 装配 → 读回）──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_slot_defaults_to_disabled_and_swaps() {
    // 默认占位 manager：全部拒绝（MCP_DISABLED），无网络操作。
    let err = connect_err(manager_slot().get_or_connect("anything").await);
    assert_eq!(err.kind, McpErrorKind::Disabled);

    let control = Arc::new(MockControl::default());
    let mine = McpManager::with_connect_factory(
        mock_cfg(0),
        test_settings(),
        mock_factory(Arc::clone(&control)),
    );
    let _previous = install_manager(Arc::clone(&mine));

    let conn = manager_slot().get_or_connect("mock").await.unwrap();
    let tools = conn.probe_tools().await.unwrap();
    assert_eq!(tools, vec!["echo"]);
    mine.shutdown_all().await;
}
