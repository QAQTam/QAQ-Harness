//! Error type shared across the client.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("discovery error: {0}")]
    Discovery(String),

    #[error("negotiation error: {0}")]
    Negotiation(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("protocol violation: {0}")]
    Protocol(String),

    /// Timeline SSE journal no longer covers the client cursor. The stream
    /// recovers by re-fetching the authoritative snapshot and advancing the
    /// cursor to its watermark (mirrors TS `TimelineGapError`).
    #[error("timeline SSE gap: expected seq {expected}, received {received}")]
    TimelineGap { expected: u64, received: u64 },

    #[error("HTTP {status}: {path}")]
    Http { status: u16, path: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ClientError>;
