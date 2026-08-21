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

/// Extract a u64 field from a JSON arguments object.
///
/// Supports both numeric (`{"count": 5}`) and string (`{"count": "5"}`) formats.
pub fn parse_opt_u64(args: &str, key: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(args).ok()?;
    let val = v.get(key)?;
    val.as_u64()
        .or_else(|| val.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// Extract the "action" field from tool arguments.
pub fn tool_action(args: &str) -> String {
    parse_arg_or(args, "action", "")
}

/// Extract the "path" field from tool arguments.
pub fn parse_file_arg(args: &str) -> Option<String> {
    parse_arg(args, "path")
}

/// Extract the "command" field from tool arguments.
pub fn parse_cmd_arg(args: &str) -> Option<String> {
    parse_arg(args, "command")
}
