//! PR-M1-5 验收（PLAN §4 出口）：`cargo test -p qaqh-mcp --test call_path` 全绿。
//!
//! 用例映射（in-memory mock，全部经 `dispatch_with`/`projection_batch_with`
//! 显式注入 manager——不触碰全局槽位，用例可并行）：
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `echo_round_trip` | connect→tools/list 缓存→投影批次→注册→调用成功 |
//! | `tool_error_maps_is_error` | `isError=true` → `MCP_TOOL_ERROR` + content 透传 |
//! | `cancel_hits_and_sends_notification` | 250ms 轮询命中 cancel → `MCP_CANCELLED` + 通知送达 mock |
//! | `timeout_keeps_connection_healthy` | 超时 → `MCP_TIMEOUT`；连接不 crash，后续调用正常 |
//! | `crash_during_call_then_reconnect` | 在飞断连 → `MCP_SERVER_CRASHED`；下次调用单次重启 |
//! | `busy_rejects_over_concurrency_cap` | `max_concurrent_calls=1` → 第二路 `MCP_BUSY`，排空后恢复 |
//! | `malformed_and_unknown_names` | 前缀/未知 server → `MCP_NOT_FOUND`（附名单） |
//!
//! 红线呼应：本文件不含 `set_cancel(false)`；取消只读注入的 `AtomicBool`。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::{ConnectFactory, LifecycleSettings};
use qaqh_workspace::ToolResult;
use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelledNotificationParam,
    ContentBlock, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{
    ClientLifecycleMode, ClientServiceExt, NotificationContext, RequestContext, ServiceExt,
};
use serde_json::json;

// ── mock：echo / fail / slow 三工具 + 取消通知记录 ──

#[derive(Default)]
struct MockControl {
    /// cancel 通知到达次数（on_cancelled 回调计数）。
    cancels_seen: AtomicU64,
    /// slow 工具睡眠时长（测试可调）。
    slow_ms: AtomicU64,
}

impl MockControl {
    fn cancels(&self) -> u64 {
        self.cancels_seen.load(Ordering::SeqCst)
    }
}

struct MockServer {
    control: Arc<MockControl>,
}

impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "callpath-mock".into();
        info.server_info.version = "0.1.0".into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema = Arc::new(
            json!({ "type": "object", "properties": {} })
                .as_object()
                .cloned()
                .unwrap(),
        );
        let echo = Tool::new("echo", "echo back", Arc::clone(&schema));
        let fail = Tool::new("fail", "always fails", Arc::clone(&schema));
        let slow = Tool::new("slow", "sleeps then succeeds", schema);
        Ok(ListToolsResult::with_all_items(vec![echo, fail, slow]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .map(|args| {
                        args.get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no text)")
                    })
                    .unwrap_or("(no args)");
                Ok(
                    CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))])
                        .into(),
                )
            }
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "boom: simulated tool failure",
            )])
            .into()),
            "slow" => {
                let ms = self.control.slow_ms.load(Ordering::SeqCst).max(500);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(CallToolResult::success(vec![ContentBlock::text("slow done")]).into())
            }
            other => Err(rmcp::ErrorData::invalid_params(
                format!("unknown tool {other:?}"),
                None,
            )),
        }
    }

    async fn on_cancelled(
        &self,
        _notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        self.control.cancels_seen.fetch_add(1, Ordering::SeqCst);
    }
}

// ── mock 工厂（lifecycle.rs 模式）──

/// in-memory 双工 + 每次连接新代 mock server（代数/注入失败控制面不再需要——
/// crash 语义改由真实子进程 SIGKILL 覆盖，见 subprocess_crash_* 用例）。
fn mock_factory(calls: Arc<MockControl>) -> ConnectFactory {
    Arc::new(move |name, _cfg| {
        let name = name.to_owned();
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            let (client_half, server_half) = tokio::io::duplex(64 * 1024);
            let (c_read, c_write) = tokio::io::split(client_half);
            let (s_read, s_write) = tokio::io::split(server_half);
            let server_task = tokio::spawn(async move {
                let service = MockServer { control: calls }
                    .serve((s_read, s_write))
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("mock server init failed: {e}").into()
                    })?;
                service.waiting().await.map_err(
                    |e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("mock server wait failed: {e}").into()
                    },
                )?;
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            });
            // 显式 drop（JoinHandle 是 future，`let _` 会触发
            // let_underscore_future）：drop 即 detach，生命周期由 transport
            // 半端与 runtime 托管。
            drop(server_task);
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

// ── 夹具 ──

fn server_cfg(max_concurrent: u32) -> McpServerConfig {
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
        max_concurrent_calls: max_concurrent,
    }
}

fn make_manager(max_concurrent: u32, idle_secs: u64) -> (Arc<McpManager>, Arc<MockControl>) {
    let calls = Arc::new(MockControl {
        slow_ms: AtomicU64::new(2000),
        ..MockControl::default()
    });
    let cfg = McpConfig {
        import_external: false,
        enabled: true,
        idle_shutdown_secs: idle_secs,
        servers: BTreeMap::from([("mock".to_owned(), server_cfg(max_concurrent))]),
    };
    let settings = LifecycleSettings {
        connect_timeout: Duration::from_millis(500),
        reconnect_cooldown: Duration::from_millis(300),
        close_timeout: Duration::from_secs(2),
        idle_tick: Duration::from_millis(50),
    };
    let manager = McpManager::with_connect_factory(cfg, settings, mock_factory(Arc::clone(&calls)));
    (manager, calls)
}

fn dispatch_sync(
    manager: &Arc<McpManager>,
    tool: &str,
    args: serde_json::Value,
    cancel: &AtomicBool,
    timeout_secs: Option<u64>,
) -> ToolResult {
    qaqh_mcp::bridge_for_tests::dispatch_with(manager, tool, &args, cancel, timeout_secs)
}

/// 解析错误 ToolResult 的 §7 JSON（错误结果的 model_text 即 JSON 文本）。
fn error_code_of(result: &ToolResult) -> (String, String) {
    let text = result.model_text();
    let parsed: serde_json::Value = serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("error result must be §7 JSON, got {text:?}: {error}"));
    (
        parsed["code"].as_str().unwrap_or("").to_owned(),
        parsed["message"].as_str().unwrap_or("").to_owned(),
    )
}

/// 轮询直到连接缓存出现（连接后 tools/list 拉取是异步的）。
fn wait_cached_tools(manager: &Arc<McpManager>, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(conn) = manager.connection(name)
            && conn.cached_tools().is_some()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("tools cache never populated for {name}");
}

// ── 用例 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_round_trip() {
    let (manager, _calls) = make_manager(4, 0);
    let conn = manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");

    // 投影批次：连接置脏 → 全量批次 → 注册进 ToolManager → 模型面合并。
    let batch = qaqh_mcp::bridge_for_tests::projection_batch_with(&manager)
        .expect("cache just populated — batch must exist");
    let names: Vec<&str> = batch.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "mcp",
            "mcp__mock__echo",
            "mcp__mock__fail",
            "mcp__mock__slow"
        ],
        "批次[0] = 聚合工具（PR-M2-1 钉底）；无白名单 = 全暴露（设计 §5.3）"
    );
    let mut tool_manager = qaqh_workspace::ToolManager::new();
    for (name, tool) in &batch {
        tool_manager
            .register_dynamic(name.clone(), tool.clone())
            .unwrap();
    }
    assert!(tool_manager.category_of("mcp__mock__echo").is_some());

    // 消费后置脏复位：无变化 → None。
    assert!(qaqh_mcp::bridge_for_tests::projection_batch_with(&manager).is_none());

    // 调用：echo 回声。
    let cancel = AtomicBool::new(false);
    let result = dispatch_sync(
        &manager,
        "mcp__mock__echo",
        json!({ "text": "hi" }),
        &cancel,
        None,
    );
    assert!(
        result.is_success(),
        "echo should succeed: {}",
        result.model_text()
    );
    assert!(result.model_text().contains("echo: hi"));
    assert_eq!(
        conn.status(),
        qaqh_mcp::ConnStatus::Connected { inflight: 0 }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_error_maps_is_error() {
    let (manager, _calls) = make_manager(4, 0);
    manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");
    let cancel = AtomicBool::new(false);
    let result = dispatch_sync(&manager, "mcp__mock__fail", json!({}), &cancel, None);
    assert!(!result.is_success());
    let (code, message) = error_code_of(&result);
    assert_eq!(code, "MCP_TOOL_ERROR");
    assert!(
        message.contains("boom: simulated tool failure"),
        "content 透传：{message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_hits_and_sends_notification() {
    let (manager, calls) = make_manager(4, 0);
    manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_thread = Arc::clone(&cancel);
    // 300ms 后置取消；slow 工具睡 2s——命中必须远早于工具完成。
    let flagger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel_thread.store(true, Ordering::Relaxed);
    });

    let started = Instant::now();
    let result = dispatch_sync(&manager, "mcp__mock__slow", json!({}), &cancel, None);
    flagger.join().unwrap();
    let elapsed = started.elapsed();
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_CANCELLED", "实际返回：{}", result.model_text());
    assert!(
        elapsed < Duration::from_millis(1800),
        "取消应在工具完成前生效（实际 {elapsed:?}）"
    );
    // 取消通知送达 mock（on_cancelled 计数；best-effort 的正向断言）。
    // 注：在飞 RPC 持有 service 锁，通知任务要等锁释放后才发出——轮询窗口
    // 须覆盖 slow 的睡眠时长（2s）+ 余量。
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && calls.cancels() == 0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        calls.cancels() >= 1,
        "notifications/cancelled 应送达 mock server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_keeps_connection_healthy() {
    let (manager, _calls) = make_manager(4, 0);
    let conn = manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");

    let cancel = AtomicBool::new(false);
    let result = dispatch_sync(&manager, "mcp__mock__slow", json!({}), &cancel, Some(1));
    let (code, message) = error_code_of(&result);
    assert_eq!(code, "MCP_TIMEOUT");
    assert!(
        message.contains("may still be executing"),
        "hint 语义：{message}"
    );

    // 超时后连接健康（不是 crash）：worker 侧 RPC 由 conn 层 tokio timeout
    // 硬顶收尾，inflight 归零晚于桥接返回一拍——轮询等待而非立即断言。
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if conn.status() == (qaqh_mcp::ConnStatus::Connected { inflight: 0 }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "inflight 未归零：{:?}",
            conn.status()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let result = dispatch_sync(
        &manager,
        "mcp__mock__echo",
        json!({ "text": "after" }),
        &cancel,
        None,
    );
    assert!(
        result.is_success(),
        "超时后 echo 应正常：{}",
        result.model_text()
    );
}

// ── 子进程 crash（真实进程死亡 → transport EOF → 在飞 RPC TransportClosed；
// Windows 专项归双平台备注，见 PLAN §4 M1-5 行）──

#[cfg(unix)]
fn pid_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "qaqh-mcp-callpath-pid-{}-{tag}.txt",
        std::process::id()
    ))
}

#[cfg(unix)]
fn subprocess_cfg(tag: &str, slow_ms: u64) -> McpConfig {
    let pids = pid_file(tag);
    let _ = std::fs::remove_file(&pids);
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp_stdio_server.mjs");
    let server = McpServerConfig {
        transport: McpTransportKind::Stdio,
        command: "node".to_owned(),
        args: vec![fixture.to_string_lossy().to_string()],
        env: BTreeMap::from([
            (
                "QAQH_TEST_PID_FILE".to_owned(),
                pids.to_string_lossy().to_string(),
            ),
            ("QAQH_TEST_MODE".to_owned(), "normal".to_owned()),
            ("QAQH_TEST_SLOW_MS".to_owned(), slow_ms.to_string()),
        ]),
        ..server_cfg(4)
    };
    McpConfig {
        import_external: false,
        enabled: true,
        idle_shutdown_secs: 0,
        servers: BTreeMap::from([(tag.to_owned(), server)]),
    }
}

/// 轮询直到 pid 文件出现（fixture 启动写入），返回 pid。
#[cfg(unix)]
fn wait_pid_file(path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pid file never appeared: {path:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subprocess_crash_mid_call_reports_server_crashed() {
    let manager = McpManager::new(subprocess_cfg("crash", 8000));
    let conn = manager.get_or_connect("crash").await.unwrap();
    wait_cached_tools(&manager, "crash");

    let pid = wait_pid_file(&pid_file("crash"));
    // 在飞 slow（8s）途中 SIGKILL node → transport EOF → ServerCrashed。
    let cancel = Arc::new(AtomicBool::new(false));
    let manager_bg = Arc::clone(&manager);
    let cancel_bg = Arc::clone(&cancel);
    let slow = std::thread::spawn(move || {
        dispatch_sync(
            &manager_bg,
            "mcp__crash__slow",
            json!({}),
            &cancel_bg,
            Some(60),
        )
    });
    std::thread::sleep(Duration::from_millis(1500));
    let killed = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("kill -9 must run");
    assert!(killed.success(), "kill -9 {pid} failed");

    let result = slow.join().unwrap();
    let (code, _message) = error_code_of(&result);
    assert_eq!(
        code,
        "MCP_SERVER_CRASHED",
        "进程死亡必须报 crash：{}",
        result.model_text()
    );
    assert_eq!(conn.status(), qaqh_mcp::ConnStatus::Disconnected);

    // 下一次调用单次重启（设计 §5.1：crash 不设冷却；attempts 无法计数——
    // 真实 adapter 无 mock 工厂，以“echo 恢复成功”作为重启证据）。
    let cancel2 = AtomicBool::new(false);
    let result = dispatch_sync(
        &manager,
        "mcp__crash__echo",
        json!({ "text": "back" }),
        &cancel2,
        None,
    );
    assert!(
        result.is_success(),
        "crash 后应单次重启成功：{}",
        result.model_text()
    );

    manager.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_rejects_over_concurrency_cap() {
    // max_concurrent_calls=1：第一路慢调用在飞，第二路必须 MCP_BUSY。
    let (manager, calls) = make_manager(1, 0);
    manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");

    let cancel = Arc::new(AtomicBool::new(false));
    let manager_bg = Arc::clone(&manager);
    let cancel_bg = Arc::clone(&cancel);
    let first = std::thread::spawn(move || {
        dispatch_sync(&manager_bg, "mcp__mock__slow", json!({}), &cancel_bg, None)
    });
    // 等第一路真正开始（连接 + RPC 发出）。
    std::thread::sleep(Duration::from_millis(500));

    let cancel2 = AtomicBool::new(false);
    let second = dispatch_sync(&manager, "mcp__mock__echo", json!({}), &cancel2, None);
    let (code, message) = error_code_of(&second);
    assert_eq!(code, "MCP_BUSY", "并发上限拒绝：{message}");

    first.join().unwrap();
    // 排空后恢复可用。
    let result = dispatch_sync(
        &manager,
        "mcp__mock__echo",
        json!({ "text": "drained" }),
        &cancel2,
        None,
    );
    assert!(result.is_success(), "排空后应恢复：{}", result.model_text());
    let _ = calls;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_unknown_names() {
    let (manager, _calls) = make_manager(4, 0);
    manager.get_or_connect("mock").await.unwrap();
    wait_cached_tools(&manager, "mock");
    let cancel = AtomicBool::new(false);

    let result = dispatch_sync(&manager, "echo", json!({}), &cancel, None);
    let (code, message) = error_code_of(&result);
    assert_eq!(code, "MCP_NOT_FOUND", "缺前缀：{message}");

    let result = dispatch_sync(&manager, "mcp__nope__echo", json!({}), &cancel, None);
    let (code, message) = error_code_of(&result);
    assert_eq!(code, "MCP_NOT_FOUND");
    assert!(message.contains("mock"), "附可用名单：{message}");
}
