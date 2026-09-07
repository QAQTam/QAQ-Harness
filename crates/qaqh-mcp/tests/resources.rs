//! PR-M2-1 验收（PLAN §4 出口）：`cargo test -p qaqh-mcp --test resources` 全绿。
//!
//! 用例映射（in-memory mock，经 `aggregate_dispatch_with`/`projection_batch_with`
//! 显式注入 manager——不触碰全局槽位，用例可并行；mock 形状沿用 call_path.rs）：
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `aggregate_tool_pinned_in_batch_head` | 批次[0]=`mcp`（enabled 即在场）；零 server 配置也含聚合工具 |
//! | `list_servers_empty_config` | 零配置 → 提示语 |
//! | `list_servers_shows_connection_state` | 未连接 disconnected → 连接后 connected + 计数 |
//! | `list_resources_unknown_server_maps_not_found` | `MCP_NOT_FOUND`（附名单） |
//! | `list_resources_before_connect_shows_placeholder` | 未连接 → 占位行（指引 lazy connect） |
//! | `list_resources_after_connect_lists_cache_and_templates` | 缓存清单 + URI 模板段 |
//! | `read_resource_text_passthrough` | 文本直通（mime 头） |
//! | `read_resource_blob_placeholder` | `[blob mime=... size=3 uri=...]` |
//! | `read_resource_unknown_uri_maps_tool_error` | server 侧报错 → `MCP_TOOL_ERROR` |
//! | `read_resource_unknown_server_maps_not_found` | `MCP_NOT_FOUND` |
//! | `read_resource_missing_params_maps_protocol` | 缺 uri → `MCP_PROTOCOL` |
//! | `invalid_action_maps_protocol` | 非法/缺 action → `MCP_PROTOCOL` |
//! | `disabled_manager_rejects_aggregate` | enabled=false → `MCP_DISABLED` |
//!
//! 红线呼应：本文件不含 `set_cancel(false)`。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::{ConnectFactory, LifecycleSettings};
use qaqh_types::ToolResult;
use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RequestContext, ServiceExt};
use serde_json::json;

// ── mock：echo 工具 + greeting/logo 资源 + item/{id} 模板 ──

struct ResourceServer;

impl ServerHandler for ResourceServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info.name = "resources-mock".into();
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
            Resource::new("fixture://greeting", "greeting")
                .with_mime_type("text/plain")
                .with_description("a friendly text greeting"),
            Resource::new("fixture://logo.png", "logo")
                .with_mime_type("image/png")
                .with_size(3)
                .with_description("tiny binary blob"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("fixture://item/{id}", "item")
                .with_mime_type("text/plain")
                .with_description("expand {id} yourself, then read_resource"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        match request.uri.as_str() {
            "fixture://greeting" => Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                vec![ResourceContents::text(
                    "hello from fixture resource",
                    request.uri,
                )],
            ))),
            "fixture://logo.png" => {
                let mut blob = ResourceContents::blob("AAAA", request.uri);
                blob = blob.with_mime_type("image/png");
                Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                    vec![blob],
                )))
            }
            // 慢读靶：cancel/超时用例（结果未到才可被取消丢弃）。
            "fixture://slow" => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                    vec![ResourceContents::text("slow resource", request.uri)],
                )))
            }
            other => Err(rmcp::ErrorData::invalid_params(
                format!("unknown resource {other:?}"),
                None,
            )),
        }
    }
}

// ── mock 工厂（call_path.rs 模式）──

fn mock_factory() -> ConnectFactory {
    Arc::new(move |name, _cfg| {
        let name = name.to_owned();
        Box::pin(async move {
            let (client_half, server_half) = tokio::io::duplex(64 * 1024);
            let (c_read, c_write) = tokio::io::split(client_half);
            let (s_read, s_write) = tokio::io::split(server_half);
            let server_task = tokio::spawn(async move {
                let service = ResourceServer.serve((s_read, s_write)).await.map_err(
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
            // 显式 drop（JoinHandle 是 future，`let _` 触发 let_underscore_future）。
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

fn server_cfg() -> McpServerConfig {
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
        max_concurrent_calls: 4,
    }
}

fn make_manager_named(server: Option<&str>) -> Arc<McpManager> {
    make_manager_multi(server.map(|name| vec![name.to_owned()]).unwrap_or_default())
}

/// 多 server 变体（env block 封顶用例：n × 3 资源条目）。
fn make_manager_multi(names: Vec<String>) -> Arc<McpManager> {
    let servers: BTreeMap<_, _> = names.into_iter().map(|name| (name, server_cfg())).collect();
    let cfg = McpConfig {
        enabled: true,
        idle_shutdown_secs: 0,
        servers,
    };
    let settings = LifecycleSettings {
        connect_timeout: Duration::from_millis(500),
        reconnect_cooldown: Duration::from_millis(300),
        close_timeout: Duration::from_secs(2),
        idle_tick: Duration::from_millis(50),
    };
    McpManager::with_connect_factory(cfg, settings, mock_factory())
}

fn disabled_manager() -> Arc<McpManager> {
    McpManager::disabled()
}

fn aggregate(manager: &Arc<McpManager>, args: serde_json::Value) -> ToolResult {
    let cancel = AtomicBool::new(false);
    qaqh_mcp::bridge_for_tests::aggregate_dispatch_with(manager, &args, &cancel, None)
}

fn aggregate_cancelled(
    manager: &Arc<McpManager>,
    args: serde_json::Value,
    cancel: &AtomicBool,
) -> ToolResult {
    qaqh_mcp::bridge_for_tests::aggregate_dispatch_with(manager, &args, cancel, None)
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

/// 连接并等待资源缓存出现（连接后 resources/list 拉取是异步的）。
async fn connect_and_wait(manager: &Arc<McpManager>, name: &str) {
    manager.get_or_connect(name).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(conn) = manager.connection(name)
            && conn.cached_resources().is_some()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("resources cache never populated for {name}");
}

// ── 用例 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregate_tool_pinned_in_batch_head() {
    // 有 server：批次[0] = 聚合工具，其余为 server 工具。
    let manager = make_manager_named(Some("mock"));
    manager.get_or_connect("mock").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if manager
            .connection("mock")
            .is_some_and(|conn| conn.cached_tools().is_some())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let batch = qaqh_mcp::bridge_for_tests::projection_batch_with(&manager)
        .expect("cache populated — batch must exist");
    assert_eq!(batch[0].0, "mcp", "聚合工具必须钉在批次头部（PR-M2-1）");
    assert_eq!(batch[0].1.category, qaqh_workspace::ToolCategory::Read);
    assert_eq!(batch[0].1.def.function.name, "mcp");

    // 批次消费后置脏复位；零 server 配置的新 manager 首次置脏（prime/连接）
    // 时批次也含聚合工具。
    let empty = make_manager_named(None);
    qaqh_mcp::bridge_for_tests::mark_dirty(&empty);
    let batch = qaqh_mcp::bridge_for_tests::projection_batch_with(&empty)
        .expect("dirty set — batch must exist even with zero servers");
    assert_eq!(
        batch
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp"],
        "零 server 配置：批次只含聚合工具（enabled 即在场）"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_servers_empty_config() {
    let manager = make_manager_named(None);
    let result = aggregate(&manager, json!({ "action": "list_servers" }));
    assert!(
        result.is_success(),
        "empty config must still answer: {}",
        result.model_text()
    );
    assert!(result.model_text().contains("no MCP servers configured"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_servers_shows_connection_state() {
    let manager = make_manager_named(Some("mock"));
    let before = aggregate(&manager, json!({ "action": "list_servers" }));
    assert!(
        before
            .model_text()
            .contains("mock: disconnected, 0 tool(s), 0 resource(s)")
    );

    connect_and_wait(&manager, "mock").await;
    let after = aggregate(&manager, json!({ "action": "list_servers" }));
    let text = after.model_text();
    assert!(text.contains("mock: connected"), "got: {text}");
    assert!(text.contains("1 tool(s)"), "got: {text}");
    assert!(text.contains("2 resource(s)"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_resources_unknown_server_maps_not_found() {
    let manager = make_manager_named(Some("mock"));
    let result = aggregate(
        &manager,
        json!({ "action": "list_resources", "server": "nope" }),
    );
    let (code, message) = error_code_of(&result);
    assert_eq!(code, "MCP_NOT_FOUND", "{message}");
    assert!(message.contains("configured"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_resources_before_connect_shows_placeholder() {
    let manager = make_manager_named(Some("mock"));
    let result = aggregate(&manager, json!({ "action": "list_resources" }));
    assert!(result.is_success(), "{}", result.model_text());
    let text = result.model_text();
    assert!(text.contains("## mock (stdio)"), "got: {text}");
    assert!(text.contains("no resource list available"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_resources_after_connect_lists_cache_and_templates() {
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let result = aggregate(&manager, json!({ "action": "list_resources" }));
    let text = result.model_text();
    assert!(text.contains("fixture://greeting"), "got: {text}");
    assert!(text.contains("fixture://logo.png"), "got: {text}");
    assert!(text.contains("a friendly text greeting"), "got: {text}");
    assert!(
        text.contains("fixture://item/{id}"),
        "templates must be listed: {text}"
    );
    // 单 server 过滤。
    let scoped = aggregate(
        &manager,
        json!({ "action": "list_resources", "server": "mock" }),
    );
    assert!(scoped.is_success(), "{}", scoped.model_text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_text_passthrough() {
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let result = aggregate(
        &manager,
        json!({
            "action": "read_resource",
            "server": "mock",
            "uri": "fixture://greeting"
        }),
    );
    assert!(result.is_success(), "{}", result.model_text());
    assert!(result.model_text().contains("hello from fixture resource"));
    assert!(
        result.model_text().contains("[text/plain]"),
        "mime 头标注: {}",
        result.model_text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_blob_placeholder() {
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let result = aggregate(
        &manager,
        json!({
            "action": "read_resource",
            "server": "mock",
            "uri": "fixture://logo.png"
        }),
    );
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(
        result.model_text(),
        "[blob mime=image/png size=3 uri=fixture://logo.png]",
        "二进制占位行（设计 §5.4.1；base64 len 4 → 解码 3 字节）"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_unknown_uri_maps_tool_error() {
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let result = aggregate(
        &manager,
        json!({
            "action": "read_resource",
            "server": "mock",
            "uri": "fixture://nope"
        }),
    );
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_TOOL_ERROR", "server 侧错误透传（设计 §7）");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_unknown_server_maps_not_found() {
    let manager = make_manager_named(Some("mock"));
    let result = aggregate(
        &manager,
        json!({
            "action": "read_resource",
            "server": "nope",
            "uri": "fixture://greeting"
        }),
    );
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_missing_params_maps_protocol() {
    let manager = make_manager_named(Some("mock"));
    let result = aggregate(&manager, json!({ "action": "read_resource" }));
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_PROTOCOL_ERROR");
    let result = aggregate(
        &manager,
        json!({ "action": "read_resource", "server": "mock" }),
    );
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_PROTOCOL_ERROR");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_action_maps_protocol() {
    let manager = make_manager_named(Some("mock"));
    let result = aggregate(&manager, json!({}));
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_PROTOCOL_ERROR", "缺 action");
    let result = aggregate(&manager, json!({ "action": "explode" }));
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_PROTOCOL_ERROR", "未知 action");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_manager_rejects_aggregate() {
    let manager = disabled_manager();
    let result = aggregate(&manager, json!({ "action": "list_servers" }));
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_DISABLED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_resource_honours_cancel() {
    // 慢读（3s）：cancel 在结果到达前命中 → MCP_CANCELLED（结果丢弃，不发
    // 通知——read 非 tools/call，无 request id 可取消，语义 §5.4/§7）。
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let cancel = AtomicBool::new(true);
    let result = aggregate_cancelled(
        &manager,
        json!({
            "action": "read_resource",
            "server": "mock",
            "uri": "fixture://slow"
        }),
        &cancel,
    );
    let (code, _message) = error_code_of(&result);
    assert_eq!(code, "MCP_CANCELLED");
}

// ═══════ PR-M2-2：资源清单注入块渲染（resource_env_block_with）═══════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_block_none_when_disabled_or_empty() {
    assert_eq!(
        qaqh_mcp::bridge_for_tests::resource_env_block_with(&disabled_manager()),
        None,
        "disabled → None"
    );
    let empty = make_manager_named(None);
    assert_eq!(
        qaqh_mcp::bridge_for_tests::resource_env_block_with(&empty),
        None,
        "零 server → None"
    );
    let manager = make_manager_named(Some("mock"));
    assert_eq!(
        qaqh_mcp::bridge_for_tests::resource_env_block_with(&manager),
        None,
        "配置了但未连接 → None（不注入占位文本）"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_block_renders_resources_and_templates() {
    let manager = make_manager_named(Some("mock"));
    connect_and_wait(&manager, "mock").await;
    let block = qaqh_mcp::bridge_for_tests::resource_env_block_with(&manager)
        .expect("resources cached → block");
    assert!(block.contains("[MCP resources]"), "{block}");
    assert!(block.contains("- mock (stdio):"), "{block}");
    assert!(block.contains("fixture://greeting"), "{block}");
    assert!(block.contains("fixture://logo.png"), "{block}");
    assert!(block.contains("[template] fixture://item/{id}"), "{block}");
    assert!(block.contains("a friendly text greeting"), "{block}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_block_caps_at_20_entries() {
    // 15 server × 3 条（2 资源 + 1 模板）= 45 条 → 封顶 20 条 + 截断行。
    let names: Vec<String> = (0..15).map(|index| format!("srv{index:02}")).collect();
    let manager = make_manager_multi(names.clone());
    for name in &names {
        connect_and_wait(&manager, name).await;
    }
    let block = qaqh_mcp::bridge_for_tests::resource_env_block_with(&manager)
        .expect("many resources → block");
    let entries = block
        .lines()
        .filter(|line| line.starts_with("  - "))
        .count();
    assert_eq!(entries, 20, "条目封顶 20（server 头行不计）：{block}");
    assert!(
        block.contains("truncated at 20 items"),
        "截断提示缺失: {block}"
    );
}
