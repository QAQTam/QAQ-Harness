//! MCP → 模型面投影适配层（设计 §5.3；PR-M1-4）。
//!
//! 职责边界：本模块只做"rmcp 类型 → [`qaqh_workspace`] 投影纯函数"的翻译
//! 与 per-server 白名单解析。命名/截断/schema 直通/碰撞拒绝都在
//! qaqh-workspace（出口测试 `dynamic_registration` 所在层）——本 crate 的
//! 测试覆盖适配语义（白名单过滤/category 映射/timeout 透传）。
//!
//! 注册时机（设计 §5.3）：仅回合边界由 actor 调用 `ToolManager::register_dynamic`
//! ——本模块只产出批次，注册动作的接线在 M1-5（bridge/refresh 路径）。

use std::time::Duration;

use qaqh_config::config::{McpServerConfig, McpTransportKind};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{DynamicTool, ToolCallCtx, ToolPlacement, ToolResult, build_dynamic_tool};

/// server 的 `tools/list` 结果 → 可注册的动态工具批次。
///
/// - **白名单**（设计 §5.3 模型面体积治理）：`server_cfg.tools = Some([...])`
///   时只投影名单内工具（不配 = 全暴露）；名单中 server 实际未暴露的名字
///   → 日志 warn（连接后校验；调用时报 `MCP_NOT_FOUND` 归 M1-5）；
/// - **category**（S3）：stdio → `Exec`；http → `Net`——子代理沙箱按
///   category 自动拒绝（actor.rs 旗标路径，零特判）；
/// - **timeout**：`server_cfg.default_timeout_secs` 透传（配置层已校验
///   1..=3600）；
/// - **dispatcher**：全体工具共用同一个 fn 指针（E-5 单一 dispatcher）。
pub fn project_tools(
    server: &str,
    server_cfg: &McpServerConfig,
    tools: &[rmcp::model::Tool],
    dispatcher: fn(ToolCallCtx) -> ToolResult,
) -> Vec<(String, DynamicTool)> {
    let category = match server_cfg.transport {
        McpTransportKind::Stdio => ToolCategory::Exec,
        McpTransportKind::Http => ToolCategory::Net,
    };
    let timeout = Duration::from_secs(server_cfg.default_timeout_secs);

    let mut out = Vec::new();
    for tool in tools {
        let tool_name = tool.name.to_string();
        if let Some(allowed) = &server_cfg.tools
            && !allowed.iter().any(|name| name == &tool_name)
        {
            continue;
        }
        let description = tool.description.as_deref().unwrap_or_default().to_owned();
        // schema 直通：MCP inputSchema 与 QAQH ToolFunction.parameters 同为
        // JSON Schema，零转换（设计 §5.3）。
        let schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        let (name, entry) = build_dynamic_tool(
            server,
            &tool_name,
            &description,
            schema,
            dispatcher,
            ToolPlacement::HostOnly,
            category,
            timeout,
        );
        out.push((name, entry));
    }

    // 连接后名单校验：白名单里 server 未暴露的名字 → warn（不阻止注册）。
    if let Some(allowed) = &server_cfg.tools {
        for want in allowed {
            if !tools.iter().any(|tool| tool.name.as_ref() == want) {
                log::warn!(
                    "[mcp] server {server}: 白名单工具 {want:?} 未在 tools/list 中出现（调用时将报 MCP_NOT_FOUND）"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn noop(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok("noop")
    }

    fn server_cfg(
        transport: McpTransportKind,
        tools: Option<Vec<String>>,
        timeout_secs: u64,
    ) -> McpServerConfig {
        McpServerConfig {
            transport,
            command: "npx".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            tools,
            resources_enabled: true,
            default_timeout_secs: timeout_secs,
            max_concurrent_calls: 1,
            cwd: String::new(),
        }
    }

    fn rmcp_tool(name: &str, description: &str) -> rmcp::model::Tool {
        let schema = serde_json::json!({ "type": "object", "properties": {} });
        rmcp::model::Tool::new(
            name.to_owned(),
            description.to_owned(),
            Arc::new(schema.as_object().unwrap().clone()),
        )
    }

    #[test]
    fn whitelist_filters_and_keeps_unconfigured_full_exposure() {
        // 不配白名单 = 全暴露。
        let cfg = server_cfg(McpTransportKind::Stdio, None, 60);
        let tools = vec![rmcp_tool("echo", "a"), rmcp_tool("slow", "b")];
        let projected = project_tools("demo", &cfg, &tools, noop);
        let names: Vec<&str> = projected.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["mcp__demo__echo", "mcp__demo__slow"]);

        // 白名单只放行 echo（missing 名不在 tools/list → warn 路径触发不炸）。
        let cfg = server_cfg(
            McpTransportKind::Stdio,
            Some(vec!["echo".to_owned(), "missing".to_owned()]),
            60,
        );
        let projected = project_tools("demo", &cfg, &tools, noop);
        let names: Vec<&str> = projected.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["mcp__demo__echo"], "白名单外的工具不投影");
    }

    #[test]
    fn category_maps_stdio_exec_and_http_net() {
        let stdio = server_cfg(McpTransportKind::Stdio, None, 60);
        let projected = project_tools("s", &stdio, &[rmcp_tool("t", "")], noop);
        assert_eq!(
            projected[0].1.category,
            ToolCategory::Exec,
            "S3：stdio→Exec"
        );

        let http = server_cfg(McpTransportKind::Http, None, 60);
        let projected = project_tools("h", &http, &[rmcp_tool("t", "")], noop);
        assert_eq!(projected[0].1.category, ToolCategory::Net, "S3：http→Net");
    }

    #[test]
    fn timeout_and_schema_passthrough() {
        let cfg = server_cfg(McpTransportKind::Stdio, None, 120);
        let long_desc = "很长的描述".repeat(500); // 3000 bytes > 2KB
        let schema =
            serde_json::json!({ "type": "object", "properties": { "text": { "type": "string" } } });
        let tool = rmcp::model::Tool::new(
            "echo",
            long_desc,
            Arc::new(schema.as_object().unwrap().clone()),
        );
        let projected = project_tools("demo", &cfg, &[tool], noop);
        assert_eq!(projected.len(), 1);
        let (_, entry) = &projected[0];
        assert_eq!(
            entry.default_timeout,
            Duration::from_secs(120),
            "server 配置的 default_timeout_secs 透传"
        );
        assert_eq!(
            entry.def.function.parameters["properties"]["text"]["type"], "string",
            "schema 直通零转换"
        );
        assert!(
            entry.def.function.description.len() <= 2048
                && entry.def.function.description.ends_with(" [truncated]"),
            "2KB 截断在 rmcp 链路下生效"
        );
    }
}
