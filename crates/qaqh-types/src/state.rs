use serde::{Deserialize, Serialize};

/// Agent debug/trace verbosity level.
///
/// Controls how much internal detail is included in the frontend debug panel
/// and log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DebugLevel {
    /// Minimal output — errors only.
    Low,
    /// Moderate output — includes turn summaries and tool call info.
    #[default]
    Medium,
    /// Verbose output — full message dumps, SSE raw data, and timing details.
    High,
}

impl DebugLevel {
    /// Parse a debug level from a string. Case-insensitive.
    /// Unknown values default to `Medium`.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" => DebugLevel::Low,
            "high" => DebugLevel::High,
            _ => DebugLevel::Medium,
        }
    }
}
