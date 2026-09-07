//! PR-M2-1：`mcp` 聚合资源工具（设计 §5.4.1，最小侵入方案）。
//!
//! 设计要点：
//! - **单一聚合工具**（D6：不新增 per-server 工具，防模型面膨胀）——`action`
//!   参数分发 `list_servers` / `list_resources` / `read_resource`；
//! - **无前缀名** `mcp`：不经 `mcp__` D5 快路径（它是 QAQH 内置只读工具而非
//!   server 声明，D4"声明即信任"不适用），category=`Read` 走常规审批
//!   （level≥2 自动放行，level 1 弹确认）；
//! - 文本资源直通；二进制以 `[blob mime=<mt> size=<n> uri=<uri>]` 占位
//!   （size 为 base64 解码后字节数的近似：`len*3/4`，标准填充误差 ≤2 字节）；
//! - **资源模板**（uriTemplate）单独列出模板串，由模型自行展开构造 uri 后
//!   调 `read_resource`（本工具不做模板展开）；
//! - **不做**（设计 §5.4 明确排除）：资源订阅/更新推送（Phase 2）、自动
//!   内联资源内容到系统提示（M2-2 只注入清单摘要）。
//!
//! 注册路径：不走 [`crate::projection`] 的 per-server 投影——经
//! [`crate::bridge::projection_batch_with`] 在每个批次头部钉入（enabled 即在
//! 场，与连接状态解耦：零 server 配置时 `list_servers` 也能答"无配置"）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use qaqh_config::config::McpServerConfig;
use qaqh_types::{ToolDef, ToolFunction, ToolResult};
use qaqh_workspace::{DynamicTool, ToolCallCtx, ToolPlacement, ToolRisk};

use crate::bridge::{DEFAULT_TIMEOUT_SECS, error_result};
use crate::error::{McpError, McpErrorKind};
use crate::manager::McpManager;

/// 聚合工具的模型面名（无 `mcp__` 前缀）。
pub const AGGREGATE_TOOL_NAME: &str = "mcp";

/// `read_resource` 等待桥接结果的轮询间隔（与 bridge::wait_response 同款）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 构造聚合工具的投影条目（批次头部钉入；timeout 与 per-server 工具默认值
/// 同源——资源读取是快速 RPC，60s 封顶足够）。
pub fn aggregate_entry(timeout: Duration) -> (String, DynamicTool) {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list_servers", "list_resources", "read_resource"],
                "description": "list_servers = MCP server 状态总览；list_resources = 列出资源清单（server 可选，缺省全部）；read_resource = 读取单个资源内容（server+uri 必填）"
            },
            "server": {
                "type": "string",
                "description": "list_resources 时限定单个 server；read_resource 时必填"
            },
            "uri": {
                "type": "string",
                "description": "read_resource 时必填；可从 list_resources 结果或 URI 模板展开得到"
            }
        },
        "required": ["action"]
    });
    let def = ToolDef {
        call_type: "function".to_owned(),
        function: ToolFunction {
            name: AGGREGATE_TOOL_NAME.to_owned(),
            description: "MCP 资源总览与读取。list_servers 列出已配置的 MCP server 及状态；\
              list_resources 列出某 server（或全部）可读资源与 URI 模板；read_resource 按 uri 读取资源内容\
              （文本直通，二进制返回占位行）。"
                .to_owned(),
            parameters: schema,
        },
    };
    let entry = DynamicTool {
        def,
        handler_fn: aggregate_dispatch,
        placement: ToolPlacement::HostOnly,
        category: qaqh_workspace::ToolCategory::Read,
        // 与 per-server 工具同构（§5.5）：risk 恒 Administrative（无条件
        // Allow 的档位字段），真实风险由 category=Read 驱动的权限层裁决。
        risk: ToolRisk::Administrative,
        default_timeout: timeout,
    };
    (AGGREGATE_TOOL_NAME.to_owned(), entry)
}

/// E-5 第二根 dispatcher 指针：`mcp` 聚合工具（生产入口，经全局槽位）。
pub fn aggregate_dispatch(ctx: ToolCallCtx) -> ToolResult {
    aggregate_dispatch_with(
        &crate::manager_slot(),
        &ctx.args,
        ctx.cancel.as_ref(),
        ctx.timeout_secs,
    )
}

/// [`aggregate_dispatch`] 的可测形态（manager/args/cancel 显式注入，测试
/// 可并行；经 [`crate::bridge_for_tests`] 垫片暴露——doc(hidden)，非公共 API
/// 契约，与 `dispatch_with` 同款约束）。
#[doc(hidden)]
pub fn aggregate_dispatch_with(
    manager: &Arc<McpManager>,
    args: &serde_json::Value,
    cancel: &AtomicBool,
    timeout_hint: Option<u64>,
) -> ToolResult {
    if !manager.config().enabled {
        return error_result(
            McpErrorKind::Disabled,
            "[mcp].enabled=false — enable MCP in config.toml to use MCP resources".to_owned(),
        );
    }
    let Some(action) = args.get("action").and_then(|value| value.as_str()) else {
        return error_result(
            McpErrorKind::Protocol,
            "missing required parameter `action` (list_servers | list_resources | read_resource)"
                .to_owned(),
        );
    };
    match action {
        "list_servers" => list_servers(manager),
        "list_resources" => list_resources(manager, args.get("server")),
        "read_resource" => read_resource(manager, cancel, timeout_hint, args),
        other => error_result(
            McpErrorKind::Protocol,
            format!(
                "unknown action {other:?} (expected list_servers | list_resources | read_resource)"
            ),
        ),
    }
}

/// `list_servers`：状态总览（不触发连接）。
fn list_servers(manager: &Arc<McpManager>) -> ToolResult {
    let lines = manager.server_status_lines();
    if lines.is_empty() {
        return ToolResult::ok("no MCP servers configured in config.toml ([mcp.servers])");
    }
    ToolResult::ok(lines.join("\n"))
}

/// `list_resources`：资源清单 + URI 模板（缓存快照，不触发连接）。
///
/// `server` 缺省 = 全部 server 遍历；未连接/未拉取的 server 标注占位行
/// （指引模型先看 list_servers 或直接 read_resource 触发 lazy connect）。
fn list_resources(manager: &Arc<McpManager>, server: Option<&serde_json::Value>) -> ToolResult {
    let wanted = server.and_then(|value| value.as_str());
    if let Some(name) = wanted
        && !manager.config().servers.contains_key(name)
    {
        let available: Vec<&str> = manager
            .config()
            .servers
            .keys()
            .map(String::as_str)
            .collect();
        return error_result(
            McpErrorKind::NotFound,
            format!("unknown MCP server {name:?}; configured: {available:?}"),
        );
    }
    let mut sections = Vec::new();
    for (name, cfg) in &manager.config().servers {
        if wanted.is_some_and(|wanted| wanted != name) {
            continue;
        }
        let conn = manager.connection(name);
        let (resources, templates) = match &conn {
            Some(conn) => (conn.cached_resources(), conn.cached_resource_templates()),
            None => (None, None),
        };
        let mut section = format!("## {name} ({transport})", transport = transport_label(cfg));
        match resources.filter(|resources| !resources.is_empty()) {
            Some(resources) => {
                for resource in resources.iter() {
                    section.push_str(&format_resource_line(resource));
                }
            }
            None => section.push_str(
                "\n  (no resource list available — server not connected yet; call read_resource \
                 to connect, or check list_servers)",
            ),
        }
        if let Some(templates) = templates.filter(|templates| !templates.is_empty()) {
            section.push_str(
                "\n  URI templates (expand the placeholders yourself, then read_resource):",
            );
            for template in templates.iter() {
                section.push_str(&format!(
                    "\n  - {} ({})",
                    template.uri_template, template.name
                ));
            }
        }
        sections.push(section);
    }
    ToolResult::ok(sections.join("\n\n"))
}

/// `read_resource`：读取内容（server+uri 必填；lazy connect；不缓存）。
fn read_resource(
    manager: &Arc<McpManager>,
    cancel: &AtomicBool,
    timeout_hint: Option<u64>,
    args: &serde_json::Value,
) -> ToolResult {
    let Some(server) = args.get("server").and_then(|value| value.as_str()) else {
        return error_result(
            McpErrorKind::Protocol,
            "read_resource requires `server` (and `uri`)".to_owned(),
        );
    };
    let Some(uri) = args.get("uri").and_then(|value| value.as_str()) else {
        return error_result(
            McpErrorKind::Protocol,
            format!("read_resource requires `uri` (see list_resources for {server:?})"),
        );
    };
    let (server, uri) = (server.to_owned(), uri.to_owned());
    if !manager.config().servers.contains_key(&server) {
        let available: Vec<&str> = manager
            .config()
            .servers
            .keys()
            .map(String::as_str)
            .collect();
        return error_result(
            McpErrorKind::NotFound,
            format!("unknown MCP server {server:?}; configured: {available:?}"),
        );
    }
    let timeout_secs = timeout_hint.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, 3600);
    let timeout = Duration::from_secs(timeout_secs);

    // 与 per-server 工具的 dispatch_with 同款 async→sync 桥（250ms 轮询
    // cancel/deadline）；read 不是 tools/call，取消只丢弃结果、不发通知。
    let (tx, rx) = std::sync::mpsc::channel();
    let manager_task = Arc::clone(manager);
    let server_task = server.clone();
    let uri_task = uri.clone();
    crate::bridge::runtime_handle().spawn(async move {
        let result = run_read(&manager_task, &server_task, &uri_task, timeout).await;
        // 桥接方超时/取消后提前放弃等待 → rx 已 drop，send 失败属预期。
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(result)) => return render_read_result(&result),
            Ok(Err(error)) => return error_result(error.kind, error.message),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    return error_result(
                        McpErrorKind::Cancelled,
                        format!(
                            "server {server}: read {uri:?} cancelled; in-flight result discarded \
                             (resource reads are not notified — the server may still be processing)"
                        ),
                    );
                }
                if Instant::now() >= deadline {
                    return error_result(
                        McpErrorKind::Timeout,
                        format!(
                            "server {server}: read {uri:?} timed out after {timeout_secs}s — \
                             server may still be processing"
                        ),
                    );
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return error_result(
                    McpErrorKind::Protocol,
                    "bridge task dropped the result channel unexpectedly".to_owned(),
                );
            }
        }
    }
}

/// 读取任务主体：lazy connect → `resources/read`（连接层超时/错误映射）。
async fn run_read(
    manager: &McpManager,
    server: &str,
    uri: &str,
    timeout: Duration,
) -> Result<rmcp::model::ReadResourceResult, McpError> {
    let conn = manager.get_or_connect(server).await?;
    conn.read_resource(uri, timeout).await
}

/// `ReadResourceResult` → 模型文本：Text 直通；Blob 占位（设计 §5.4.1）。
fn render_read_result(result: &rmcp::model::ReadResourceResult) -> ToolResult {
    if result.contents.is_empty() {
        return ToolResult::ok("(empty resource contents)");
    }
    let parts: Vec<String> = result
        .contents
        .iter()
        .map(|content| match content {
            rmcp::model::ResourceContents::TextResourceContents {
                text, mime_type, ..
            } => match mime_type {
                Some(mime) if !mime.is_empty() => format!("[{mime}]\n{text}"),
                _ => text.clone(),
            },
            rmcp::model::ResourceContents::BlobResourceContents {
                blob,
                mime_type,
                uri,
                ..
            } => {
                let mime = mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_owned());
                // base64 解码字节数近似（标准填充误差 ≤2 字节；不引解码依赖）。
                let size = blob.len() * 3 / 4;
                format!("[blob mime={mime} size={size} uri={uri}]")
            }
            // rmcp non_exhaustive：未来新增变体以占位行降级。
            _ => "[unsupported resource content variant]".to_owned(),
        })
        .collect();
    ToolResult::ok(parts.join("\n\n"))
}

/// 单条资源行：`- uri (mime, name): description`。
fn format_resource_line(resource: &rmcp::model::Resource) -> String {
    let mime = resource
        .mime_type
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let mut line = format!("\n  - {} ({mime}, {})", resource.uri, resource.name);
    if let Some(description) = &resource.description {
        line.push_str(": ");
        line.push_str(description);
    }
    line
}

fn transport_label(cfg: &McpServerConfig) -> &'static str {
    use qaqh_config::config::McpTransportKind;
    match cfg.transport {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
    }
}

// ═══════ PR-M2-2：系统提示注入块渲染（设计 §5.4.2）═══════

/// 注入块封顶：跨 server 合计最多列 20 条资源（资源条目计，server 头行不占额）。
const ENV_BLOCK_MAX_ITEMS: usize = 20;
/// 单条描述截断（字符）。
const ENV_ITEM_MAX_CHARS: usize = 120;

/// MCP 资源清单注入块（生产入口，全局槽位；disabled/无资源 → `None`）。
///
/// runtime 在回合边界拉取并与上次比对，变化才经 ContextFlow 物化
/// （prefix cache 友好）。连接状态解耦：未连接的 server 不占条目，
/// 全部为空时返回 None（不注入占位文本）。
pub fn resource_env_block() -> Option<String> {
    resource_env_block_with(&crate::manager_slot())
}

/// [`resource_env_block`] 的可测形态（manager 显式注入；测试垫片暴露）。
#[doc(hidden)]
pub fn resource_env_block_with(manager: &Arc<McpManager>) -> Option<String> {
    if !manager.config().enabled {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for (name, cfg) in &manager.config().servers {
        let Some(conn) = manager.connection(name) else {
            continue; // 从未拉起过：不占条目，清单为空时不注入
        };
        let Some(resources) = conn.cached_resources().filter(|r| !r.is_empty()) else {
            continue;
        };
        lines.push(format!("- {} ({}):", name, transport_label(cfg)));
        for resource in resources.iter() {
            if entries_so_far(&lines) >= ENV_BLOCK_MAX_ITEMS {
                lines.push(format!(
                    "  …(resource list truncated at {ENV_BLOCK_MAX_ITEMS} items)"
                ));
                return Some(render_env_block(&lines));
            }
            lines.push(format_resource_env_line(resource));
        }
        if let Some(templates) = conn
            .cached_resource_templates()
            .filter(|templates| !templates.is_empty())
        {
            for template in templates.iter() {
                if entries_so_far(&lines) >= ENV_BLOCK_MAX_ITEMS {
                    lines.push(format!(
                        "  …(resource list truncated at {ENV_BLOCK_MAX_ITEMS} items)"
                    ));
                    return Some(render_env_block(&lines));
                }
                let mime = template
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                lines.push(format!(
                    "  - [template] {} ({mime}, {})",
                    template.uri_template, template.name
                ));
            }
        }
    }
    if lines.iter().all(|line| line.starts_with("- ")) {
        return None; // 只有 server 头没有条目 → 不注入空块
    }
    Some(render_env_block(&lines))
}

/// 已列资源条目数（server 头行与截断行不计）。
fn entries_so_far(lines: &[String]) -> usize {
    lines.iter().filter(|line| line.starts_with("  - ")).count()
}

fn format_resource_env_line(resource: &rmcp::model::Resource) -> String {
    let mime = resource
        .mime_type
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let description = resource
        .description
        .as_deref()
        .map(|text| {
            if text.chars().count() > ENV_ITEM_MAX_CHARS {
                let truncated: String = text.chars().take(ENV_ITEM_MAX_CHARS).collect();
                format!("{truncated}…")
            } else {
                text.to_owned()
            }
        })
        .map(|text| format!(" {text}"))
        .unwrap_or_default();
    format!(
        "  - {} ({mime}, {}){description}",
        resource.uri, resource.name
    )
}

fn render_env_block(lines: &[String]) -> String {
    let mut block = String::from(
        "[MCP resources] 可读资源清单（经 `mcp` 工具 read_resource 读取；URI 模板自行展开后读取）：\n",
    );
    block.push_str(&lines.join("\n"));
    block
}
