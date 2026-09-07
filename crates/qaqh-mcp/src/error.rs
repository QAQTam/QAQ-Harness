//! qaqh-mcp 错误模型（设计 `docs/mcp-client-design.md` §7）。
//!
//! 全量语义码对齐设计 §7 表格（M1-5 落地）；`Shutdown` 是 `shutting_down`
//! 闸的拒绝码——设计 §7 未单列，作为闸门码保留；`Busy` 为
//! `max_concurrent_calls` 执行点新增（§7 表格已同步补行）。

use std::fmt;

/// MCP 错误分类；`code()` 产出 ToolResult JSON 的 `code` 字段（设计 §7 命名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpErrorKind {
    /// `[mcp].enabled=false`（或 manager 以禁用配置构建）。
    Disabled,
    /// 连接失败：spawn/握手出错，或当前处于重连冷却期（设计 §5.1）。
    ConnectFailed,
    /// 连接超时（默认 10s；设计 §5.1）。
    ConnectTimeout,
    /// 执行中 transport 断连/子进程退出；当前调用不重试（设计 §5.1）。
    ServerCrashed,
    /// 协议层错误（JSON-RPC error 透传）。
    Protocol,
    /// 工具跑完但业务失败（`isError=true`；§7：透传 content）。
    ToolError,
    /// 调用级超时（§7 hint：server 可能仍在执行，未取消成功勿盲目重发）。
    Timeout,
    /// `ctx.cancel` 命中（§7 hint：已尽力发送取消通知）。
    Cancelled,
    /// server 在飞调用数达到 `max_concurrent_calls` 上限（桥接层拒绝新调用）。
    Busy,
    /// server/工具/资源不存在。
    NotFound,
    /// daemon 关闭闸已落下：拒绝 lazy connect / 重连 / idle 重启（设计 §5.1）。
    Shutdown,
}

impl McpErrorKind {
    /// ToolResult JSON 中的稳定错误码字符串。
    pub fn code(self) -> &'static str {
        match self {
            Self::Disabled => "MCP_DISABLED",
            Self::ConnectFailed => "MCP_CONNECT_FAILED",
            Self::ConnectTimeout => "MCP_CONNECT_TIMEOUT",
            Self::ServerCrashed => "MCP_SERVER_CRASHED",
            Self::Protocol => "MCP_PROTOCOL_ERROR",
            Self::ToolError => "MCP_TOOL_ERROR",
            Self::Timeout => "MCP_TIMEOUT",
            Self::Cancelled => "MCP_CANCELLED",
            Self::Busy => "MCP_BUSY",
            Self::NotFound => "MCP_NOT_FOUND",
            Self::Shutdown => "MCP_SHUTDOWN",
        }
    }
}

impl fmt::Display for McpErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// qaqh-mcp 生命周期错误：分类 + 人类可读细节。
///
/// M1-5 桥接层负责把它转成 `ToolResult::error` 的 JSON 结构
/// （timeis / status=error / code / message / hint）。
#[derive(Debug, Clone)]
pub struct McpError {
    /// 错误分类。
    pub kind: McpErrorKind,
    /// 人类可读细节（server 名 + 现场；不含 env/secret 值——E-6 红线）。
    pub message: String,
}

impl McpError {
    /// 构造一个带分类与细节的错误。
    pub fn new(kind: McpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 稳定错误码（如 `MCP_CONNECT_TIMEOUT`）。
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message)
    }
}

impl std::error::Error for McpError {}
