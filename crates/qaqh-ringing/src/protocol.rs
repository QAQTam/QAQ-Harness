//! 线协议标识与常量（PLAN 固定命名）。

/// 线协议 schema 标识。
pub const RINGING_SCHEMA: &str = "qaqh.Ringing";

/// 线协议版本。
pub const RINGING_VERSION: u32 = 1;

/// Ringing v1 的统一 HTTP 前缀。
pub const RINGING_BASE_PATH: &str = "/ringing/v1";

/// open 成功后所有请求使用的连接级身份 header。
pub const CLIENT_SESSION_HEADER: &str = "X-QAQH-Client-Session-Id";

/// JSON 能被 JavaScript 精确表示的最大无符号整数。
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn is_safe_integer(value: u64) -> bool {
    value <= MAX_SAFE_INTEGER
}

/// SSE 事件流帧格式：`id: <server_epoch>:<channel>:<stream_seq>`。
pub const SSE_EVENT_ID_SEPARATOR: char = ':';
