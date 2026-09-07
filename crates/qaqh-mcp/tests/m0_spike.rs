//! M0 spike 验收（PR-M0-1）：
//!
//! 1. `default-features = false` + client/transport-child-process 组合可编译（本体已由
//!    `cargo check -p qaqh-mcp` 证明）；
//! 2. in-memory 双工（dev 侧 transport-io/server）上完成 `initialize → tools/list`
//!    完整握手——证明 SDK 接入可行，协议面工作正常；
//! 3. 已知发现回填：rmcp 3.2.0 的 child-process 传输**不设置进程组**
//!    （全 crate 无 ProcessGroup 使用，见 PLAN §7 实测记录）——adapter 层在 M1 补。
//!
//! 本测试不触碰任何全局状态，无需 TEST_RUNTIME_SERIAL。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml 说明）

use std::time::Duration;

use rmcp::RoleServer;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool};
use rmcp::service::{RequestContext, ServiceExt};
use serde_json::json;

/// 最小 mock server：只声明 tools 能力并返回一个 echo 工具。
#[derive(Debug, Default, Clone)]
struct MockServer;

impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "m0-mock".into();
        info.server_info.version = "0.1.0".into();
        info.instructions = Some("M0 spike mock server".into());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        });
        let tool = Tool::new(
            "echo",
            "echo back the input text",
            Arc::new(schema.as_object().expect("schema is object").clone()),
        );
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }
}

use std::sync::Arc;

#[tokio::test]
async fn initialize_and_list_tools_over_in_memory_transport()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 全测试 10s 超时兜底：握手挂死直接失败，不悬挂 cargo test。
    tokio::time::timeout(Duration::from_secs(10), async {
        // in-memory 双工：client 半端 / server 半端，各自 split 成 (read, write)
        let (client_half, server_half) = tokio::io::duplex(64 * 1024);
        let (c_read, c_write) = tokio::io::split(client_half);
        let (s_read, s_write) = tokio::io::split(server_half);

        // server 侧：serve 完成握手后进入 waiting（显式错误归一，避免 JoinError 推断冲突）
        let server_task = tokio::spawn(async move {
            let service = match MockServer.serve((s_read, s_write)).await {
                Ok(s) => s,
                Err(e) => return Err(format!("server init failed: {e}")),
            };
            match service.waiting().await {
                Ok(_reason) => Ok(()),
                Err(e) => Err(format!("server wait failed: {e}")),
            }
        });

        // client 侧：legacy initialize 生命周期（mock server 走默认握手）
        let client = ().serve((c_read, c_write)).await?;

        // M0 核心验收：tools/list 打通
        let tools = client.list_all_tools().await?;
        assert_eq!(tools.len(), 1, "mock server 应暴露恰好 1 个工具");
        assert_eq!(tools[0].name, "echo", "工具名应为 echo");
        assert!(
            tools[0].input_schema.contains_key("properties"),
            "input_schema 应为 JSON Schema object"
        );

        // 优雅收尾：client 取消 → server waiting() 返回
        client.cancel().await?;
        let outcome = server_task
            .await
            .map_err(|e| format!("server task panicked: {e}"))?;
        assert!(outcome.is_ok(), "server side error: {outcome:?}");

        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .expect("spike 在 10s 内未完成——握手挂死")?;
    Ok(())
}
