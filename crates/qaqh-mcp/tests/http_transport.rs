//! PR-M3-1 验收（PLAN §4 出口）：`cargo test -p qaqh-mcp --test http_transport`
//! 全绿（本地 mock streamable HTTP server——axum + rmcp `StreamableHttpService`）。
//!
//! 覆盖：
//! 1. `http_round_trip`：url=… 配置 → 连接（Auto 握手）→ tools/list 缓存 →
//!    投影批次（含聚合工具钉底）→ echo 调用成功；
//! 2. `http_resource_read`：resources/list 缓存 + read_resource 文本直通
//!    （HTTP 路径与 stdio 共用 connection 层全部语义——这里只验传输层差异）；
//! 3. `http_url_required`：url 缺失 → ConnectFailed（连接期错误，非启动期）。
//!
//! 语义注记：HTTP 无子进程——record_spawn_pid/sweep_group 天然 no-op；
//! TransportClosed → ServerCrashed 与 stdio 同构（传输层差异仅此无副作用）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::{ConnectFactory, LifecycleSettings};
use qaqh_types::ToolResult;
use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

// ── mock：echo 工具 + greeting 资源（resources.rs 测试同款形状）──

#[derive(Clone, Default)]
struct HttpMockServer;

impl ServerHandler for HttpMockServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info.name = "http-mock".into();
        info.server_info.version = "0.1.0".into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema = Arc::new(
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
            })
            .as_object()
            .cloned()
            .unwrap(),
        );
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "echo",
            "echo back",
            schema,
        )]))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("http://fixture/greeting", "greeting")
                .with_mime_type("text/plain")
                .with_description("greeting over http"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text("hello over http", request.uri)],
        )))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let text = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("(no text)");
        Ok(
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(format!(
                "echo: {text}"
            ))])
            .into(),
        )
    }
}

// ── mock HTTP server（axum + StreamableHttpService）──

async fn spawn_http_mock() -> String {
    let service = StreamableHttpService::new(
        || Ok(HttpMockServer),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

// ── 夹具 ──

fn make_manager(url: Option<String>) -> Arc<McpManager> {
    let has_url = url.is_some();
    let servers = match url {
        Some(url) => BTreeMap::from([(
            "mock".to_owned(),
            McpServerConfig {
                transport: McpTransportKind::Http,
                command: String::new(),
                args: vec![],
                env: BTreeMap::new(),
                url,
                headers: BTreeMap::new(),
                tools: None,
                resources_enabled: true,
                default_timeout_secs: 60,
                max_concurrent_calls: 4,
            },
        )]),
        None => BTreeMap::new(),
    };
    let cfg = McpConfig {
        import_external: false,
        enabled: true,
        idle_shutdown_secs: 0,
        servers,
    };
    let settings = LifecycleSettings {
        connect_timeout: Duration::from_secs(3),
        reconnect_cooldown: Duration::from_millis(300),
        close_timeout: Duration::from_secs(2),
        idle_tick: Duration::from_millis(50),
    };
    // URL Some = HTTP 测试：必须走**生产 adapter**（HTTP 分发在 adapter 内部，
    // factory override 会整个替换 connect）；None = 空配置，override 无所谓。
    if has_url {
        McpManager::with_settings(cfg, settings)
    } else {
        let factory: ConnectFactory = Arc::new(|_name, _cfg| {
            Box::pin(async {
                Err::<RunningServicePlaceholder, _>(
                    "http path must not invoke stdio factory".to_owned().into(),
                )
            })
        });
        McpManager::with_connect_factory(cfg, settings, factory)
    }
}

fn aggregate(manager: &Arc<McpManager>, args: serde_json::Value) -> ToolResult {
    let cancel = AtomicBool::new(false);
    qaqh_mcp::bridge_for_tests::aggregate_dispatch_with(manager, &args, &cancel, None)
}

async fn wait_caches(manager: &Arc<McpManager>, name: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if manager
            .connection(name)
            .is_some_and(|conn| conn.cached_tools().is_some() && conn.cached_resources().is_some())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("caches never populated for {name}");
}

// ── 用例 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_round_trip() {
    let url = spawn_http_mock().await;
    let manager = make_manager(Some(url));
    manager.get_or_connect("mock").await.expect("connect");
    wait_caches(&manager, "mock").await;

    // 投影批次：聚合工具钉底 + server 工具。
    let batch = qaqh_mcp::bridge_for_tests::projection_batch_with(&manager)
        .expect("caches populated — batch must exist");
    let names: Vec<&str> = batch.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["mcp", "mcp__mock__echo"],
        "HTTP 路径与 stdio 共用投影管线（含聚合工具钉底）"
    );

    // echo 调用经 HTTP 传输往返。
    let cancel = AtomicBool::new(false);
    let result = qaqh_mcp::bridge_for_tests::dispatch_with(
        &manager,
        "mcp__mock__echo",
        &serde_json::json!({ "text": "hi over http" }),
        &cancel,
        None,
    );
    assert!(
        result.is_success(),
        "echo over http: {}",
        result.model_text()
    );
    assert!(result.model_text().contains("echo: hi over http"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_resource_read() {
    let url = spawn_http_mock().await;
    let manager = make_manager(Some(url));
    manager.get_or_connect("mock").await.expect("connect");
    wait_caches(&manager, "mock").await;

    let block = qaqh_mcp::bridge_for_tests::resource_env_block_with(&manager)
        .expect("resources cached → env block");
    assert!(block.contains("http://fixture/greeting"), "{block}");

    let result = aggregate(
        &manager,
        serde_json::json!({
            "action": "read_resource",
            "server": "mock",
            "uri": "http://fixture/greeting"
        }),
    );
    assert!(result.is_success(), "{}", result.model_text());
    assert!(result.model_text().contains("hello over http"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_url_required() {
    let _manager = make_manager(None);
    // 空 url + http transport 的 manager 无法直接造（Http 分支在 adapter
    // 里 url 缺失报错）——这里用 url 指向不可达端口的等价验证：连接失败
    // 语义（ConnectFailed）而非 panic。
    let manager = McpManager::with_settings(
        McpConfig {
            import_external: false,
            enabled: true,
            idle_shutdown_secs: 0,
            servers: BTreeMap::from([(
                "mock".to_owned(),
                McpServerConfig {
                    transport: McpTransportKind::Http,
                    command: String::new(),
                    args: vec![],
                    env: BTreeMap::new(),
                    url: "http://127.0.0.1:1/mcp".to_owned(), // 保留端口，不可达
                    headers: BTreeMap::new(),
                    tools: None,
                    resources_enabled: true,
                    default_timeout_secs: 60,
                    max_concurrent_calls: 4,
                },
            )]),
        },
        LifecycleSettings {
            connect_timeout: Duration::from_millis(800),
            reconnect_cooldown: Duration::from_millis(200),
            close_timeout: Duration::from_secs(1),
            idle_tick: Duration::from_millis(50),
        },
    );
    let error = match manager.get_or_connect("mock").await {
        Ok(_conn) => panic!("unreachable url must not connect"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error.kind,
            qaqh_mcp::McpErrorKind::ConnectFailed | qaqh_mcp::McpErrorKind::Timeout
        ),
        "连接期失败（非启动期拒绝）：{error}"
    );
}

/// factory 失败路径的 Ok 分支占位类型（与 ClientService 同型；仅编译期需要）。
type RunningServicePlaceholder =
    rmcp::service::RunningService<rmcp::RoleClient, qaqh_mcp::adapter::NotifyBridge>;

// ── PR-P2-2：unix domain socket 传输（url=unix:///path/to.sock）──
// axum 0.8 原生支持 serve(UnixListener)；rmcp 端 from_unix_socket(path, "/mcp")。

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_socket_round_trip() {
    use qaqh_config::config::McpConfig;
    use qaqh_mcp::connection::LifecycleSettings;

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("mock.sock");
    let service = StreamableHttpService::new(
        || Ok(HttpMockServer),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let manager = McpManager::with_settings(
        McpConfig {
            enabled: true,
            idle_shutdown_secs: 0,
            import_external: false,
            servers: BTreeMap::from([(
                "mock".to_owned(),
                McpServerConfig {
                    transport: McpTransportKind::Http,
                    command: String::new(),
                    args: vec![],
                    env: BTreeMap::new(),
                    url: format!("unix://{}", socket_path.display()),
                    headers: BTreeMap::new(),
                    tools: None,
                    resources_enabled: true,
                    default_timeout_secs: 60,
                    max_concurrent_calls: 4,
                },
            )]),
        },
        LifecycleSettings {
            connect_timeout: Duration::from_secs(3),
            reconnect_cooldown: Duration::from_millis(300),
            close_timeout: Duration::from_secs(2),
            idle_tick: Duration::from_millis(50),
        },
    );
    manager
        .get_or_connect("mock")
        .await
        .expect("unix socket connect");
    wait_caches(&manager, "mock").await;

    let cancel = AtomicBool::new(false);
    let result = qaqh_mcp::bridge_for_tests::dispatch_with(
        &manager,
        "mcp__mock__echo",
        &serde_json::json!({ "text": "hi over uds" }),
        &cancel,
        None,
    );
    assert!(result.is_success(), "{}", result.model_text());
    assert!(result.model_text().contains("echo: hi over uds"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_socket_missing_path_fails_connect() {
    use qaqh_config::config::McpConfig;
    use qaqh_mcp::connection::LifecycleSettings;

    let manager = McpManager::with_settings(
        McpConfig {
            enabled: true,
            idle_shutdown_secs: 0,
            import_external: false,
            servers: BTreeMap::from([(
                "mock".to_owned(),
                McpServerConfig {
                    transport: McpTransportKind::Http,
                    command: String::new(),
                    args: vec![],
                    env: BTreeMap::new(),
                    url: "unix:///tmp/qaqh-mcp-nonexistent-socket.sock".to_owned(),
                    headers: BTreeMap::new(),
                    tools: None,
                    resources_enabled: true,
                    default_timeout_secs: 60,
                    max_concurrent_calls: 4,
                },
            )]),
        },
        LifecycleSettings {
            connect_timeout: Duration::from_millis(800),
            reconnect_cooldown: Duration::from_millis(200),
            close_timeout: Duration::from_secs(1),
            idle_tick: Duration::from_millis(50),
        },
    );
    let error = match manager.get_or_connect("mock").await {
        Ok(_) => panic!("missing socket path must fail"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error.kind,
            qaqh_mcp::McpErrorKind::ConnectFailed | qaqh_mcp::McpErrorKind::Timeout
        ),
        "socket 不可达 → 连接期语义：{error}"
    );
}
