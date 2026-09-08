//! PR-M2-2 验收（PLAN §4 出口，原写 qaqh-msgloop——该 crate 已并入
//! qaqh-runtime，出口修正见 PLAN）：`cargo test -p qaqh-runtime --test
//! mcp_env_block` 全绿。
//!
//! 覆盖（AgentState 注入管线，qaqh-mcp 侧渲染语义由 --test resources 覆盖）：
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `injection_lands_as_trailing_developer_message` | 回合边界注入 → trailing developer 消息含 `[MCP resources]` |
//! | `injection_is_content_gated` | 内容不变 → 二次 sync 不重复注入（prefix cache 稳定） |
//!
//! disabled 负例（`resource_env_block_with(disabled) → None` → sync 无操作）
//! 由 qaqh-mcp `--test resources::env_block_none_when_disabled_or_empty`
//! 权威覆盖——全局槽位 OnceLock 不可重装，负例必须落在不 install 的 crate。
//!
//! 全局槽位约束：`install_manager` 是进程级 OnceLock——本文件用 OnceLock
//! 共享一个 mock manager（三用例共用，并行安全），disabled 语义的负例放
//! qaqh-mcp（`resource_env_block_with(disabled) → None`）与独立测试文件，
//! 避免 install 顺序耦合。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;
use qaqh_mcp::connection::{ConnectFactory, LifecycleSettings};
use qaqh_runtime::agent::state::agent::AgentState;
use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RequestContext, ServiceExt};

// ── mock（resources.rs 集成测试同款形状；tools/list 供连接后缓存拉取）──

struct EnvMockServer;

impl ServerHandler for EnvMockServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info.name = "envblock-mock".into();
        info.server_info.version = "0.1.0".into();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema = Arc::new(
            serde_json::json!({ "type": "object", "properties": {} })
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
            Resource::new("fixture://notes", "notes")
                .with_mime_type("text/plain")
                .with_description("session notes resource"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        Ok(ListResourceTemplatesResult::default())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text("notes content", request.uri)],
        )))
    }
}

fn mock_factory() -> ConnectFactory {
    Arc::new(move |name, _cfg| {
        let name = name.to_owned();
        Box::pin(async move {
            let (client_half, server_half) = tokio::io::duplex(64 * 1024);
            let (c_read, c_write) = tokio::io::split(client_half);
            let (s_read, s_write) = tokio::io::split(server_half);
            let server_task = tokio::spawn(async move {
                let service = EnvMockServer.serve((s_read, s_write)).await.map_err(
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

/// 进程级共享 mock manager（OnceLock：首用者 install，后续用例直取）。
fn shared_manager() -> &'static Arc<McpManager> {
    static MANAGER: OnceLock<Arc<McpManager>> = OnceLock::new();
    MANAGER.get_or_init(|| {
        let cfg = McpConfig {
            import_external: false,
            enabled: true,
            idle_shutdown_secs: 0,
            servers: BTreeMap::from([(
                "mock".to_owned(),
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
                    cwd: String::new(),
                },
            )]),
        };
        let settings = LifecycleSettings {
            connect_timeout: Duration::from_millis(500),
            reconnect_cooldown: Duration::from_millis(300),
            close_timeout: Duration::from_secs(2),
            idle_tick: Duration::from_millis(50),
        };
        McpManager::with_connect_factory(cfg, settings, mock_factory())
    })
}

/// install 全局槽位并等待资源缓存（OnceLock 语义：首用者连接，后续直等）。
fn prime_global_manager() -> &'static Arc<McpManager> {
    static PRIMED: OnceLock<()> = OnceLock::new();
    PRIMED.get_or_init(|| {
        qaqh_mcp::install_manager(Arc::clone(shared_manager()));
        let manager = shared_manager();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            manager.get_or_connect("mock").await.unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if manager
                    .connection("mock")
                    .is_some_and(|conn| conn.cached_resources().is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    });
    shared_manager()
}

fn trailing_texts(agent: &mut AgentState) -> Vec<String> {
    agent
        .msg
        .trailing_messages()
        .iter()
        .filter_map(|message| {
            message.content.iter().find_map(|block| match block {
                qaqh_types::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .collect()
}

#[test]
fn injection_lands_as_trailing_developer_message() {
    prime_global_manager();
    let mut agent = AgentState::new(qaqh_config::Config::default());
    let mut flow = qaqh_message::ContextFlow::new();
    qaqh_message::builtin::register_all(&mut flow);
    agent.sync_mcp_resource_injection(&mut flow);
    let texts = trailing_texts(&mut agent);
    assert_eq!(texts.len(), 1, "恰好一条注入: {texts:?}");
    assert!(texts[0].contains("[MCP resources]"), "{texts:?}");
    assert!(texts[0].contains("fixture://notes"), "{texts:?}");
}

#[test]
fn injection_is_content_gated() {
    prime_global_manager();
    let mut agent = AgentState::new(qaqh_config::Config::default());
    let mut flow = qaqh_message::ContextFlow::new();
    qaqh_message::builtin::register_all(&mut flow);
    agent.sync_mcp_resource_injection(&mut flow);
    assert_eq!(trailing_texts(&mut agent).len(), 1);
    // 内容不变 → 幂等（prefix cache 稳定）。
    agent.sync_mcp_resource_injection(&mut flow);
    agent.sync_mcp_resource_injection(&mut flow);
    assert_eq!(trailing_texts(&mut agent).len(), 1, "内容未变不得重复注入");
}
