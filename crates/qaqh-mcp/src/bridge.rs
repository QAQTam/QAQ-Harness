//! 同步工具环 ↔ MCP 连接的桥接（设计 §3/§5.4；PR-M1-5）。
//!
//! 职责（设计 §5.4 契约）：
//! - E-5 单一 dispatcher：[`dispatch`] 是全体 MCP 工具共用的唯一 fn 指针；
//! - mpsc req/resp：同步 handler 线程不做 `block_on`（嵌套 runtime 风险），
//!   改为向 qaqh-mcp 专属 runtime（[`runtime_handle`]，"rmcp 客户端住在
//!   专属 actor"）派生调用任务，再以 250ms 粒度 `recv_timeout` 等待；
//! - 取消/超时：等待期间只读 `ctx.cancel`（L1/L2 已接线的 `Arc<AtomicBool>`，
//!   禁触碰全局 `qaqh_workspace::CANCEL`，禁以任何字面形式复位取消旗标——
//!   契约红线1），
//!   命中即 best-effort 发送 `notifications/cancelled`、丢弃结果、返回
//!   `MCP_CANCELLED`；超时链 = `ctx.timeout_secs`（来自 server 配置的
//!   `default_timeout_secs`）→ 缺省 60s → 封顶 3600s；
//! - 错误码映射：`McpError`/`CallToolResult::is_error` → 设计 §7 的
//!   ToolResult JSON（timeis/status/code/message/hint）；
//! - 投影批次：连接层工具缓存变化（[`crate::connection::ServerConnection`]
//!   置脏）后，[`take_projection_batch`] 产出全量重建批次，runtime 在回合
//!   边界 `clear_dynamic` + 逐条 `register_dynamic`（refresh 语义）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use qaqh_workspace::{DynamicTool, MCP_DYNAMIC_PREFIX, ToolCallCtx, ToolResult, now_utc8};
use rmcp::model::{CallToolResult, ContentBlock};

use crate::McpManager;
use crate::error::{McpError, McpErrorKind};
use crate::manager_slot;
use crate::projection::project_tools;

/// 桥接轮询粒度（设计 §5.4：~250ms）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// ctx 超时缺省（设计 §4：默认 60s，封顶 3600）。
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 3600;
/// qaqh-mcp 专属 runtime worker 数：rmcp 客户端 actor + IO，不需要更多。
const RUNTIME_WORKERS: usize = 2;

fn dedicated_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(RUNTIME_WORKERS)
            .enable_all()
            .build()
            .expect("qaqh-mcp dedicated runtime must build")
    })
}

/// qaqh-mcp 专属 runtime 句柄（惰性构建，进程级单例）。
///
/// 工具执行线程是裸线程（无 ambient runtime），rmcp 的 RPC/关闭任务必须
/// 落在一个持续被驱动的 runtime 上——专属 runtime 与 daemon 自身的 runtime
/// 形态解耦（设计 §3："rmcp 客户端住在专属 actor（tokio runtime 线程）里"）。
pub fn runtime_handle() -> &'static tokio::runtime::Handle {
    dedicated_runtime().handle()
}

/// 已构建的专属 runtime 句柄（不触发惰性构建；manager `Drop` 兜底网络用）。
pub fn try_runtime_handle() -> Option<&'static tokio::runtime::Handle> {
    dedicated_runtime_checked().map(tokio::runtime::Runtime::handle)
}

fn dedicated_runtime_checked() -> Option<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get()
}

/// daemon 退出路径的优雅关闭入口：block_on 全局 manager 的 `shutdown_all`。
///
/// 只允许从非异步线程调用（daemon main 收尾）；block_on 驱动专属 runtime
/// 完成逐连接 cancel → transport close → 子进程组杀兜底（设计 §5.1）。
pub fn shutdown_global() {
    let manager = manager_slot();
    runtime_handle().block_on(manager.shutdown_all());
}

/// 投影预热（fire-and-forget）：装配点调用一次，后台逐 server 连接并缓存
/// tools/list（置脏）。
///
/// 解决 lazy 连接与投影可见性的先有鸡还是先有蛋：连接唯一触发点是工具
/// 执行，而工具要先投影进工具表才会被调用——全新 daemon 上模型首回合
/// 看不到任何 MCP 工具，调用永不发生，投影永远为空。预热在后台完成
/// 连接与缓存，不阻塞装配；未启用时不开专属 runtime、直接 no-op；
/// 长期无调用的连接由 idle 回收机制收敛。
pub fn prime_all_async() {
    let manager = manager_slot();
    if !manager.config().enabled {
        return;
    }
    runtime_handle().spawn(async move { manager.prime_all().await });
}

/// E-5 单一 dispatcher：全体 MCP 工具共用的 fn 指针（注册进 `DynamicTool`）。
pub fn dispatch(ctx: ToolCallCtx) -> ToolResult {
    dispatch_with(
        &manager_slot(),
        &ctx.name,
        &ctx.args,
        &ctx.cancel,
        ctx.timeout_secs,
    )
}

/// [`dispatch`] 的可测形态：manager 显式注入（集成测试不经全局槽位，可并行）。
/// 仅经 [`crate::bridge_for_tests`] 垫片暴露（doc(hidden)，非公共 API 契约）。
#[doc(hidden)]
pub fn dispatch_with(
    manager: &Arc<McpManager>,
    name: &str,
    args: &serde_json::Value,
    cancel: &AtomicBool,
    timeout_secs: Option<u64>,
) -> ToolResult {
    let (server, tool) = match resolve_call(manager, name) {
        Ok(pair) => pair,
        Err(error) => return mcp_error_to_tool_result(error),
    };
    let timeout_secs = timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    let timeout = Duration::from_secs(timeout_secs);

    let (tx, rx): (Sender<Result<CallToolResult, McpError>>, _) = std::sync::mpsc::channel();
    let server_name = server.clone();
    let tool_name = tool.clone();
    let args_value = args.clone();
    let manager_task = Arc::clone(manager);
    runtime_handle().spawn(async move {
        let result = run_call(&manager_task, &server_name, &tool_name, args_value, timeout).await;
        // 桥接方超时/取消后提前放弃等待 → rx 已 drop，send 失败属预期。
        let _ = tx.send(result);
    });

    wait_response(manager, &rx, cancel, &server, &tool, timeout_secs)
}

/// 解析 `mcp__{server}__{tool}` → (server, tool)。
///
/// server 名含 `_` 时存在前缀歧义（`mcp__a__b__t`），按**最长 server 前缀**
/// 消解；注册侧碰撞拒绝保证同一完整名至多注册一次，因此最长前缀即注册时
/// 的真实 (server, tool) 对。未匹配任何已配置 server → `MCP_NOT_FOUND`
/// （报错附可用名单，设计 §7）。
fn resolve_call(manager: &McpManager, name: &str) -> Result<(String, String), McpError> {
    let rest = name.strip_prefix(MCP_DYNAMIC_PREFIX).ok_or_else(|| {
        McpError::new(
            McpErrorKind::NotFound,
            format!(
                "malformed MCP tool name {name:?} (expected {MCP_DYNAMIC_PREFIX}<server>__<tool>)"
            ),
        )
    })?;
    let configured = manager.config().servers.keys().cloned().collect::<Vec<_>>();
    let mut best: Option<(String, String)> = None;
    for server in &configured {
        let candidate = format!("{server}__");
        if let Some(tool) = rest.strip_prefix(candidate.as_str())
            && !tool.is_empty()
            && best
                .as_ref()
                .is_none_or(|(current, _)| server.len() > current.len())
        {
            best = Some((server.clone(), tool.to_owned()));
        }
    }
    best.ok_or_else(|| {
        McpError::new(
            McpErrorKind::NotFound,
            format!("unknown MCP server for tool {name:?}; configured: {configured:?}"),
        )
    })
}

/// 桥接任务主体：lazy connect → `tools/call`（连接层完成超时硬顶与错误映射）。
async fn run_call(
    manager: &McpManager,
    server: &str,
    tool: &str,
    args: serde_json::Value,
    timeout: Duration,
) -> Result<CallToolResult, McpError> {
    let conn = manager.get_or_connect(server).await?;
    let json_args = match args {
        serde_json::Value::Object(map) => Some(map),
        // 模型侧无参调用（schema 无 required 字段）→ 空 object，对 server 友好。
        serde_json::Value::Null => Some(serde_json::Map::new()),
        other => {
            return Err(McpError::new(
                McpErrorKind::Protocol,
                format!(
                    "tool {tool:?} args must be a JSON object, got {}: {other}",
                    type_name_of(&other)
                ),
            ));
        }
    };
    conn.call_tool(tool, json_args, timeout).await
}

fn type_name_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Null => "null",
        serde_json::Value::Object(_) => "object",
    }
}

/// 阻塞等待桥接结果（设计 §5.4 `bridge::wait_response`）：250ms `recv_timeout`
/// 轮询 cancel 与 deadline；命中即发取消通知、丢弃结果、按 §7 返回。
fn wait_response(
    manager: &McpManager,
    rx: &Receiver<Result<CallToolResult, McpError>>,
    cancel: &AtomicBool,
    server: &str,
    tool: &str,
    timeout_secs: u64,
) -> ToolResult {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(result)) => return call_result_to_tool_result(result),
            Ok(Err(error)) => return mcp_error_to_tool_result(error),
            Err(RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    send_cancelled(manager, server, tool, "caller cancelled the tool call");
                    return error_result(
                        McpErrorKind::Cancelled,
                        format!(
                            "tool {tool:?} on server {server:?} cancelled by caller; result discarded"
                        ),
                    );
                }
                if Instant::now() >= deadline {
                    // 超时也尽力取消（§7 hint：未取消成功时勿盲目重发）。
                    send_cancelled(manager, server, tool, "client-side timeout");
                    return error_result(
                        McpErrorKind::Timeout,
                        format!(
                            "tool {tool:?} on server {server:?} timed out after {timeout_secs}s — server may still be executing; do not blindly resend until it settles"
                        ),
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // worker panic / runtime 消亡而未 send：协议层异常，如实上报。
                return error_result(
                    McpErrorKind::Protocol,
                    format!("bridge worker for tool {tool:?} terminated without a response"),
                );
            }
        }
    }
}

fn send_cancelled(manager: &McpManager, server: &str, tool: &str, reason: &str) {
    if let Some(conn) = manager.connection(server) {
        conn.try_send_cancelled(&format!("{tool}: {reason}"));
    }
}

/// `CallToolResult` → ToolResult（§7：`isError=true` → `MCP_TOOL_ERROR` 且
/// 透传 content；成功 → 文本拼接直通，输出上限由 [`ToolResult::ok`] 既有
/// 截断承担）。
fn call_result_to_tool_result(result: CallToolResult) -> ToolResult {
    let text = content_text(&result.content);
    if result.is_error.unwrap_or(false) {
        error_result(
            McpErrorKind::ToolError,
            format!("tool reported failure:\n{text}"),
        )
    } else {
        ToolResult::ok(text)
    }
}

/// 全部 content blocks 拼接（设计 §5.5）；非文本块以占位行保留存在感——
/// 二进制/资源块的真实消费是 M2 资源路径的事。
fn content_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(_) => parts.push("[image content omitted]".to_owned()),
            ContentBlock::Audio(_) => parts.push("[audio content omitted]".to_owned()),
            ContentBlock::Resource(_) => parts.push("[embedded resource omitted]".to_owned()),
            ContentBlock::ResourceLink(_) => parts.push("[resource link omitted]".to_owned()),
            // rmcp 标注 non_exhaustive：未来新增块类型以占位行降级。
            _ => parts.push("[non-text content block omitted]".to_owned()),
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    }
}

/// `McpError` → 设计 §7 ToolResult JSON（timeis/status=error/code/message/hint，
/// `WORKSPACE_EXEC_FAILED` 风格）；hint 按码给行动指引，env/secret 值永不落
/// （E-6：message 由连接层构造，不含 env 值）。
fn mcp_error_to_tool_result(error: McpError) -> ToolResult {
    error_result(error.kind, error.message)
}

fn error_result(kind: McpErrorKind, message: String) -> ToolResult {
    ToolResult::error(
        serde_json::json!({
            "timeis": now_utc8(),
            "status": "error",
            "code": kind.code(),
            "message": message,
            "hint": hint_for(kind),
        })
        .to_string(),
    )
}

fn hint_for(kind: McpErrorKind) -> &'static str {
    match kind {
        McpErrorKind::Disabled => "Set [mcp] enabled=true in config.toml to use MCP tools.",
        McpErrorKind::ConnectFailed => {
            "Check the server command is executable; connect is retried after the cooldown window."
        }
        McpErrorKind::ConnectTimeout => {
            "Server did not complete the handshake in time; check command/args and retry later."
        }
        McpErrorKind::ServerCrashed => {
            "The server will be restarted on the next call; this call was NOT retried (no duplicate side effects)."
        }
        McpErrorKind::Timeout => {
            "A cancellation notification was sent (best-effort); the server may still be executing — do not blindly resend until it settles."
        }
        McpErrorKind::Protocol => {
            "Inspect the server message above; it may indicate a schema/protocol mismatch."
        }
        McpErrorKind::ToolError => {
            "The tool ran but reported failure; its content is passed through above."
        }
        McpErrorKind::NotFound => {
            "Check the tool/server name against the configured MCP servers in config.toml."
        }
        McpErrorKind::Cancelled => {
            "Cancellation notification was sent (best-effort); the in-flight result was discarded."
        }
        McpErrorKind::Busy => {
            "The server's concurrency cap is reached; wait for in-flight calls to drain, then retry."
        }
        McpErrorKind::Shutdown => "The daemon is shutting down; no new MCP work is accepted.",
    }
}

/// 回合边界投影批次（runtime 侧接线入口，设计 §5.3）：
///
/// 全局 dirty 标记未被置位 → `None`（零开销快路径）；置位 → 全量重建批次
/// （所有已连接 server 的缓存 → [`project_tools`]）。调用方（qaqh-runtime
/// 的 run_lap）随后 `clear_dynamic` + 逐条 `register_dynamic` 并重建
/// `tool_defs`。crash/idle 回收会清缓存并置脏 → 模型面与连接状态一致。
pub fn take_projection_batch() -> Option<Vec<(String, DynamicTool)>> {
    projection_batch_with(&manager_slot())
}

/// [`take_projection_batch`] 的可测形态：manager 显式注入。
/// 仅经 [`crate::bridge_for_tests`] 垫片暴露（doc(hidden)，非公共 API 契约）。
#[doc(hidden)]
pub fn projection_batch_with(manager: &Arc<McpManager>) -> Option<Vec<(String, DynamicTool)>> {
    if !manager.take_dirty() {
        return None;
    }
    let mut batch = Vec::new();
    for conn in manager.connections() {
        let Some(tools) = conn.cached_tools() else {
            continue;
        };
        batch.extend(project_tools(
            conn.name(),
            conn.server_config(),
            &tools,
            dispatch,
        ));
    }
    Some(batch)
}
