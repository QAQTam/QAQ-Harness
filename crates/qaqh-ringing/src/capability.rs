//! 客户端 open 握手（`POST /ringing/v1/clients/open` 的 payload 类型）。
//!
//! 版本协商由 `schema`/`version` 字段承担；不再有独立的能力矩阵——
//! 同仓库发布的客户端与 daemon 版本由打包链路保证，协议演进走
//! `version` 比对（不兼容时 `unsupported_version` 426）。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 客户端 open 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ClientOpenRequest {
    pub schema: String,
    pub version: u32,
    /// 客户端实例 id（后续 lease 绑定该身份）。
    pub client_instance_id: String,
}

impl ClientOpenRequest {
    pub fn new(client_instance_id: impl Into<String>) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            client_instance_id: client_instance_id.into(),
        }
    }
}

/// 服务端 open 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ClientOpenResponse {
    pub schema: String,
    pub version: u32,
    pub accepted: bool,
    /// 服务端签发的 client session id（lease 与命令绑定该身份）。
    pub client_session_id: String,
    /// 服务端 epoch（SSE stream_seq 基准）。
    pub server_epoch: String,
    /// 逻辑 lease 的 TTL（毫秒）。
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub lease_ttl_ms: u64,
    /// 建议的 lease renew 间隔（毫秒，< TTL）。
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub renew_interval_ms: u64,
}
