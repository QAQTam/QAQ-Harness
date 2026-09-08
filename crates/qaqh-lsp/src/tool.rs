//! `lsp` 聚合工具（M1 决策 L1：单个聚合工具 + `action` 枚举，mcp `mcp` 同款）。
//!
//! - **无前缀名** `lsp`：不经 `mcp__` D5 快路径（它是 QAQH 内置只读工具而非
//!   server 声明），category=`Read` 走常规审批（M1 决策 L6）；
//! - M1 五操作：`definition / references / hover / documentSymbol /
//!   workspaceSymbol`；`implementation/callHierarchy` 进 M2；
//! - 坐标系：模型面 **1-based**（M1 决策 L2），内部转 0-based；
//! - 文档同步 M1 薄版（M1 决策 L4）：调用时读磁盘 → `didOpen`（version 自增）
//!   → 查；常驻 open 不做增量。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_types::{
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolParams,
    GotoDefinitionParams, HoverParams, PartialResultParams, Position, ReferenceContext,
    ReferenceParams, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Url,
    WorkDoneProgressParams, WorkspaceSymbolParams,
};

use qaqh_types::{ToolDef, ToolFunction, ToolResult};
use qaqh_workspace::{DynamicTool, ToolCallCtx, ToolPlacement, ToolRisk};

use crate::bridge::DEFAULT_TIMEOUT_SECS;
use crate::connection::ServerConnection;
use crate::error::{LspError, LspErrorKind};
use crate::manager::LspManager;
use crate::{manager_slot, projection};

/// 聚合工具的模型面名（无 `mcp__` 前缀；mcp `mcp` 同款）。
pub const AGGREGATE_TOOL_NAME: &str = "lsp";

/// M1 五操作（M2 追加 implementation/callHierarchy 时扩展此表 + schema enum）。
pub const ACTIONS: &[&str] = &[
    "list_servers",
    "definition",
    "references",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
];

/// 构造聚合工具的投影条目（批次头部钉入；enabled 即在场）。
pub fn aggregate_entry(timeout: std::time::Duration) -> (String, DynamicTool) {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ACTIONS,
                "description": "list_servers = LSP server 状态总览；definition = 跳转定义（filePath+line+character 必填，1-based）；references = 查找引用（同上）；hover = 悬停类型/文档（同上）；documentSymbol = 文件符号树（filePath 必填）；workspaceSymbol = 工作区符号搜索（query 必填）"
            },
            "filePath": {
                "type": "string",
                "description": "目标文件路径（相对 root 或绝对；definition/references/hover/documentSymbol 时必填）"
            },
            "line": {
                "type": "integer",
                "description": "1-based 行号（definition/references/hover 时必填）"
            },
            "character": {
                "type": "integer",
                "description": "1-based 列号（definition/references/hover 时必填）"
            },
            "query": {
                "type": "string",
                "description": "workspaceSymbol 时必填的搜索串"
            },
            "server": {
                "type": "string",
                "description": "显式指定 server（缺省按文件扩展名路由；未知扩展名时必填）"
            }
        },
        "required": ["action"]
    });
    let def = ToolDef {
        call_type: "function".to_owned(),
        function: ToolFunction {
            name: AGGREGATE_TOOL_NAME.to_owned(),
            description: "LSP 精确代码导航。definition/references/hover/documentSymbol/workspaceSymbol 五操作；\
              坐标 1-based；按文件扩展名自动路由到配置的 language server。"
                .to_owned(),
            parameters: schema,
        },
    };
    let entry = DynamicTool {
        def,
        handler_fn: aggregate_dispatch,
        placement: ToolPlacement::HostOnly,
        category: qaqh_workspace::ToolCategory::Read,
        // mcp `mcp` 同款：risk 恒 Administrative，真实风险由 category 裁决。
        risk: ToolRisk::Administrative,
        default_timeout: timeout,
    };
    (AGGREGATE_TOOL_NAME.to_owned(), entry)
}

/// 生产 dispatcher（经全局槽位；E-5 单一 fn 指针）。
pub fn aggregate_dispatch(ctx: ToolCallCtx) -> ToolResult {
    aggregate_dispatch_with(
        &manager_slot(),
        &ctx.args,
        ctx.cancel.as_ref(),
        ctx.timeout_secs,
        &current_root(),
    )
}

/// 当前工作区 root（会话 cwd；mcp 无此概念——LSP 连接键必需）。
fn current_root() -> String {
    let ws = qaqh_workspace::current_workspace();
    if ws.is_empty() || ws == "." {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_owned())
    } else {
        ws
    }
}

/// [`aggregate_dispatch`] 的可测形态（manager/args/cancel/root 显式注入）。
#[doc(hidden)]
pub fn aggregate_dispatch_with(
    manager: &Arc<LspManager>,
    args: &serde_json::Value,
    cancel: &AtomicBool,
    timeout_hint: Option<u64>,
    root: &str,
) -> ToolResult {
    if !manager.config().enabled {
        return error_result(
            LspErrorKind::Disabled,
            "[lsp].enabled=false — enable LSP in config.toml to use LSP tools".to_owned(),
        );
    }
    let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
        return error_result(
            LspErrorKind::Protocol,
            format!("missing required parameter `action` ({})", ACTIONS.join(" | ")),
        );
    };
    if cancel.load(Ordering::Relaxed) {
        return error_result(LspErrorKind::Cancelled, "cancelled before LSP dispatch".to_owned());
    }
    // timeout 链：ctx.hint → server default → 缺省 30s（mcp 60s；LSP 请求更轻）。
    let timeout = std::time::Duration::from_secs(timeout_hint.unwrap_or(DEFAULT_TIMEOUT_SECS));
    match action {
        "list_servers" => list_servers(manager),
        "definition" | "references" | "hover" | "documentSymbol" => {
            position_action(manager, action, args, cancel, timeout, root)
        }
        "workspaceSymbol" => workspace_symbol(manager, args, cancel, timeout, root),
        other => error_result(
            LspErrorKind::Protocol,
            format!("unknown action {other:?} (expected {})", ACTIONS.join(" | ")),
        ),
    }
}

fn list_servers(manager: &Arc<LspManager>) -> ToolResult {
    let lines = manager.server_status_lines();
    if lines.is_empty() {
        return ToolResult::ok("no LSP servers configured in config.toml ([lsp.servers])");
    }
    ToolResult::ok(lines.join("\n"))
}

/// 需要文件+坐标的操作：路由 → 连接 → didOpen → 请求 → 渲染。
fn position_action(
    manager: &Arc<LspManager>,
    action: &str,
    args: &serde_json::Value,
    cancel: &AtomicBool,
    timeout: std::time::Duration,
    root: &str,
) -> ToolResult {
    let Some(path) = args.get("filePath").and_then(|v| v.as_str()) else {
        return error_result(
            LspErrorKind::Protocol,
            format!("{action} requires `filePath`"),
        );
    };
    let server = match args.get("server").and_then(|v| v.as_str()) {
        Some(explicit) => explicit.to_owned(),
        None => match manager.server_for_extension(path) {
            Some(name) => name,
            None => {
                let cfg = manager.config();
                let available: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
                return error_result(
                    LspErrorKind::NotFound,
                    format!(
                        "no LSP server routes extension of {path:?}; pass explicit `server` (configured: {available:?})"
                    ),
                );
            }
        },
    };
    if !manager.config().servers.contains_key(&server) {
        let cfg = manager.config();
        let available: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
        return error_result(
            LspErrorKind::NotFound,
            format!("unknown LSP server {server:?}; configured: {available:?}"),
        );
    }
    let (line, character) = if action == "documentSymbol" {
        (0, 0)
    } else {
        let (Some(l), Some(c)) = (
            args.get("line").and_then(|v| v.as_u64()),
            args.get("character").and_then(|v| v.as_u64()),
        ) else {
            return error_result(
                LspErrorKind::Protocol,
                format!("{action} requires 1-based `line` + `character`"),
            );
        };
        if l == 0 || c == 0 {
            return error_result(
                LspErrorKind::Protocol,
                format!("{action}: line/character are 1-based (got {l}:{c})"),
            );
        }
        (l, c)
    };
    let default_timeout = manager
        .config()
        .servers
        .get(&server)
        .map(|c| c.default_timeout_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout = std::time::Duration::from_secs(default_timeout.min(timeout.as_secs().max(1)));
    let handle = crate::bridge::runtime_handle();
    let req = PositionRequest {
        manager,
        server: &server,
        root,
        action,
        path,
        line,
        character,
        cancel,
        timeout,
    };
    handle.block_on(async { run_position_action(req).await })
}

/// position 系操作的参数束（clippy 7 参上限：9 参收拢为结构体）。
struct PositionRequest<'a> {
    manager: &'a Arc<LspManager>,
    server: &'a str,
    root: &'a str,
    action: &'a str,
    path: &'a str,
    line: u64,
    character: u64,
    cancel: &'a AtomicBool,
    timeout: std::time::Duration,
}

async fn run_position_action(req: PositionRequest<'_>) -> ToolResult {
    let conn = match req.manager.get_or_connect(req.server, req.root).await {
        Ok(conn) => conn,
        Err(e) => return error_result(e.kind, e.message),
    };
    let abs = absolutize(req.root, req.path);
    // M1 薄版文档同步：读磁盘 → didOpen（version 自增；已 open 则重开）。
    let text = match std::fs::read_to_string(&abs) {
        Ok(t) => t,
        Err(e) => {
            return error_result(
                LspErrorKind::Protocol,
                format!("read {abs:?} failed: {e}"),
            );
        }
    };
    let uri = match Url::from_file_path(&abs) {
        Ok(u) => u,
        Err(_) => {
            return error_result(
                LspErrorKind::Protocol,
                format!("{abs:?} is not a valid file path"),
            );
        }
    };
    if cancelled(req.cancel) {
        return error_result(LspErrorKind::Cancelled, "cancelled before didOpen".to_owned());
    }
    if let Err(e) = did_open_file(&conn, &uri, &text, req.server).await {
        return error_result(e.kind, e.message);
    }
    let pos = Position::new(
        u32::try_from(req.line.saturating_sub(1)).unwrap_or(u32::MAX),
        u32::try_from(req.character.saturating_sub(1)).unwrap_or(u32::MAX),
    );
    let out: Result<String, LspError> = match req.action {
        "definition" => {
            request_with_cancel(&conn, req.cancel, req.timeout, |socket| async move {
                let result = socket
                    .request::<lsp_types::request::GotoDefinition>(GotoDefinitionParams {
                        text_document_position_params: TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier { uri },
                            position: pos,
                        },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .await?;
                Ok(projection::render_goto_result(result))
            })
            .await
        }
        "references" => {
            request_with_cancel(&conn, req.cancel, req.timeout, |socket| async move {
                let result = socket
                    .request::<lsp_types::request::References>(ReferenceParams {
                        text_document_position: TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier { uri },
                            position: pos,
                        },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                        context: ReferenceContext {
                            include_declaration: true,
                        },
                    })
                    .await?;
                Ok(projection::render_references(result))
            })
            .await
        }
        "hover" => {
            request_with_cancel(&conn, req.cancel, req.timeout, |socket| async move {
                let result = socket
                    .request::<lsp_types::request::HoverRequest>(HoverParams {
                        text_document_position_params: TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier { uri },
                            position: pos,
                        },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                    })
                    .await?;
                Ok(projection::render_hover(result))
            })
            .await
        }
        "documentSymbol" => {
            request_with_cancel(&conn, req.cancel, req.timeout, |socket| async move {
                let result = socket
                    .request::<lsp_types::request::DocumentSymbolRequest>(DocumentSymbolParams {
                        text_document: TextDocumentIdentifier { uri },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .await?;
                Ok(projection::render_document_symbols(result))
            })
            .await
        }
        _ => unreachable!("dispatch 已校验 action 白名单"),
    };
    match out {
        Ok(text) => ToolResult::ok(text),
        Err(e) => {
            if matches!(e.kind, LspErrorKind::ServerCrashed) {
                conn.mark_crashed();
            }
            error_result(e.kind, e.message)
        }
    }
}

fn workspace_symbol(
    manager: &Arc<LspManager>,
    args: &serde_json::Value,
    cancel: &AtomicBool,
    timeout: std::time::Duration,
    root: &str,
) -> ToolResult {
    let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
        return error_result(LspErrorKind::Protocol, "workspaceSymbol requires `query`".to_owned());
    };
    // 路由：显式 server 优先；否则首个已配置 server（workspace 级搜索无文件轴）。
    let server = match args.get("server").and_then(|v| v.as_str()) {
        Some(explicit) => explicit.to_owned(),
        None => match manager.config().servers.keys().next().cloned() {
            Some(name) => name,
            None => {
                return error_result(
                    LspErrorKind::NotFound,
                    "no LSP servers configured".to_owned(),
                );
            }
        },
    };
    if !manager.config().servers.contains_key(&server) {
        let cfg = manager.config();
        let available: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
        return error_result(
            LspErrorKind::NotFound,
            format!("unknown LSP server {server:?}; configured: {available:?}"),
        );
    }
    let default_timeout = manager
        .config()
        .servers
        .get(&server)
        .map(|c| c.default_timeout_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout = std::time::Duration::from_secs(default_timeout.min(timeout.as_secs().max(1)));
    let query = query.to_owned();
    let handle = crate::bridge::runtime_handle();
    handle.block_on(async {
        let conn = match manager.get_or_connect(&server, root).await {
            Ok(conn) => conn,
            Err(e) => return error_result(e.kind, e.message),
        };
        let out: Result<String, LspError> = request_with_cancel(&conn, cancel, timeout, |socket| {
            let query = query.clone();
            async move {
                let result = socket
                    .request::<lsp_types::request::WorkspaceSymbolRequest>(WorkspaceSymbolParams {
                        query,
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .await?;
                Ok(projection::render_workspace_symbols(result))
            }
        })
        .await;
        match out {
            Ok(text) => ToolResult::ok(text),
            Err(e) => {
                if matches!(e.kind, LspErrorKind::ServerCrashed) {
                    conn.mark_crashed();
                }
                error_result(e.kind, e.message)
            }
        }
    })
}

/// didOpen 薄版同步：已 open 则先 didClose 再重开（保证查的是落盘态）。
async fn did_open_file(
    conn: &Arc<ServerConnection>,
    uri: &Url,
    text: &str,
    server: &str,
) -> Result<(), LspError> {
    let session = conn.session_snapshot().ok_or_else(|| {
        LspError::new(
            LspErrorKind::ServerCrashed,
            format!("lsp server {server} is not connected"),
        )
    })?;
    let mut guard = session.lock().await;
    let key = uri.as_str().to_owned();
    let version = guard.opened.get(&key).map_or(0, |v| v + 1);
    if guard.opened.contains_key(&key) {
        guard
            .socket
            .notify::<lsp_types::notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })?;
    }
    let language_id = language_id_for(uri);
    guard.socket.notify::<lsp_types::notification::DidOpenTextDocument>(
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id,
                version,
                text: text.to_owned(),
            },
        },
    )?;
    guard.opened.insert(key, version);
    Ok(())
}

fn language_id_for(uri: &Url) -> String {
    let path = uri.path().to_ascii_lowercase();
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "c" => "c",
        "h" | "hpp" | "cc" | "cpp" => "cpp",
        "java" => "java",
        other => other,
    }
    .to_owned()
}

fn absolutize(root: &str, path: &str) -> String {
    if std::path::Path::new(path).is_absolute() {
        path.to_owned()
    } else {
        format!("{root}/{path}")
    }
}

fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// 带 cancel 轮询 + 超时的请求执行（mcp bridge 250ms 轮询同款语义，LSP 简化版：
/// async-lsp 请求 future 直接 tokio::select 超时/cancel——无 mpsc 桥接层）。
async fn request_with_cancel<F, Fut>(
    conn: &Arc<ServerConnection>,
    cancel: &AtomicBool,
    timeout: std::time::Duration,
    run: F,
) -> Result<String, LspError>
where
    F: FnOnce(async_lsp::ServerSocket) -> Fut,
    Fut: std::future::Future<Output = Result<String, LspError>>,
{
    let _guard = conn.begin_call();
    let session = conn.session_snapshot().ok_or_else(|| {
        LspError::new(
            LspErrorKind::ServerCrashed,
            "lsp server is not connected".to_owned(),
        )
    })?;
    let socket = session.lock().await.socket.clone();
    let fut = run(socket);
    tokio::pin!(fut);
    let cancel_tick = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        }
    };
    tokio::pin!(cancel_tick);
    tokio::select! {
        out = &mut fut => out,
        _ = &mut cancel_tick => Err(LspError::new(
            LspErrorKind::Cancelled,
            "cancelled during LSP request".to_owned(),
        )),
        _ = tokio::time::sleep(timeout) => Err(LspError::new(
            LspErrorKind::Timeout,
            format!("LSP request timed out after {timeout:?}"),
        )),
    }
}

fn error_result(kind: LspErrorKind, message: String) -> ToolResult {
    ToolResult::error(
        serde_json::json!({
            "timeis": qaqh_workspace::now_utc8(),
            "status": "error",
            "code": kind.code(),
            "message": message,
            "hint": hint_for(kind),
        })
        .to_string(),
    )
}

fn hint_for(kind: LspErrorKind) -> &'static str {
    match kind {
        LspErrorKind::Disabled => "enable LSP in config.toml [lsp]",
        LspErrorKind::NotFound => "check [lsp.servers] names and extensions routing",
        LspErrorKind::ConnectFailed | LspErrorKind::ConnectTimeout => {
            "check server command is executable; cooling down, retry later"
        }
        LspErrorKind::ServerCrashed => "server crashed; next call auto-reconnects",
        LspErrorKind::Timeout => "server may still be working; avoid blind resend",
        LspErrorKind::Protocol => "check filePath/line/character params (1-based)",
        LspErrorKind::Cancelled => "cancellation requested",
        LspErrorKind::Shutdown => "daemon is shutting down",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_cover_m1_five_ops() {
        for op in ["definition", "references", "hover", "documentSymbol", "workspaceSymbol"] {
            assert!(ACTIONS.contains(&op), "M1 操作 {op} 必须在列");
        }
        assert!(!ACTIONS.contains(&"implementation"), "M2 操作不得提前");
    }

    #[test]
    fn missing_action_reports_protocol() {
        let manager = LspManager::new(qaqh_config::config::LspConfig {
            enabled: true,
            ..Default::default()
        });
        let out = aggregate_dispatch_with(
            &manager,
            &serde_json::json!({}),
            &AtomicBool::new(false),
            None,
            "/tmp",
        );
        assert!(!out.is_success());
        assert!(out.model_text().contains("LSP_PROTOCOL_ERROR"));
    }

    #[test]
    fn disabled_reports_disabled_code() {
        let manager = LspManager::disabled();
        let out = aggregate_dispatch_with(
            &manager,
            &serde_json::json!({"action": "list_servers"}),
            &AtomicBool::new(false),
            None,
            "/tmp",
        );
        assert!(out.model_text().contains("LSP_DISABLED"));
    }

    #[test]
    fn unknown_extension_without_server_reports_not_found() {
        let mut servers = std::collections::BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            qaqh_config::config::LspServerConfig {
                command: "rust-analyzer".to_owned(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                extensions: vec!["rs".to_owned()],
                startup_timeout_secs: 30,
                default_timeout_secs: 30,
            },
        );
        let manager = LspManager::new(qaqh_config::config::LspConfig {
            enabled: true,
            idle_shutdown_secs: 0,
            servers,
        });
        let out = aggregate_dispatch_with(
            &manager,
            &serde_json::json!({"action": "definition", "filePath": "a.xyz", "line": 1, "character": 1}),
            &AtomicBool::new(false),
            None,
            "/tmp",
        );
        assert!(out.model_text().contains("LSP_NOT_FOUND"), "{}", out.model_text());
    }

    #[test]
    fn zero_coordinate_rejected_as_not_one_based() {
        let mut servers = std::collections::BTreeMap::new();
        servers.insert(
            "rust".to_owned(),
            qaqh_config::config::LspServerConfig {
                command: "rust-analyzer".to_owned(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
                extensions: vec!["rs".to_owned()],
                startup_timeout_secs: 30,
                default_timeout_secs: 30,
            },
        );
        let manager = LspManager::new(qaqh_config::config::LspConfig {
            enabled: true,
            idle_shutdown_secs: 0,
            servers,
        });
        let out = aggregate_dispatch_with(
            &manager,
            &serde_json::json!({"action": "hover", "filePath": "a.rs", "line": 0, "character": 1}),
            &AtomicBool::new(false),
            None,
            "/tmp",
        );
        assert!(out.model_text().contains("1-based"), "{}", out.model_text());
    }
}
