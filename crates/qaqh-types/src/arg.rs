//! Unified JSON argument parsing for tool call arguments.
//!
//! All qaqh crates should use these functions instead of duplicating
//! serde_json parsing for the common fields (action, path, command, ...).

use serde_json::Value;

/// Extract a string field from a JSON arguments string.
///
/// # Arguments
/// * `args` — A JSON object string (e.g. `{"path": "/foo", "action": "read"}`).
/// * `key` — The field name to look up.
///
/// # Returns
/// `Some(value)` if the field exists and is a string, `None` otherwise
/// (including if `args` is not valid JSON).
pub fn parse_arg(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| v.get(key)?.as_str().map(|s| s.to_string()))
}

/// Extract a string field with a default fallback.
pub fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    parse_arg(args, key).unwrap_or_else(|| default.to_string())
}

/// Extract an optional string field (alias for `parse_arg`).
pub fn parse_opt(args: &str, key: &str) -> Option<String> {
    parse_arg(args, key)
}
