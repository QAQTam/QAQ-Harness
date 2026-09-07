//! service::params — handle 参数提取小函数（snake/camel 双键 + tool_mode 校验）。

use serde_json::Value;

pub(crate) fn value2<'a>(params: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    params.get(snake).or_else(|| params.get(camel))
}
pub(crate) fn pstr(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {key}"))
}
pub(crate) fn pstr2(params: &Value, snake: &str, camel: &str) -> Result<String, String> {
    value2(params, snake, camel)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {snake}"))
}
pub(crate) fn pbool(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}
pub(crate) fn pu64(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or_default()
}
pub(crate) fn pstrings(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// 可选工具模式预置：缺省 = None（保持旧行为）；显式空串 = standard 零迁移。
/// 供 create 路径（session.new）在 spawn 前落盘使用。
pub(crate) fn optional_tool_mode(params: &Value) -> Result<Option<(String, Vec<String>)>, String> {
    let Some(tool_mode) = params.get("tool_mode").and_then(Value::as_str) else {
        return Ok(None);
    };
    // 显式空串 = 未指定（与 SessionMeta.tool_mode 的空串零迁移语义一致）。
    if tool_mode.is_empty() {
        return Ok(None);
    }
    validate_tool_mode(tool_mode)?;
    let custom_tools = pstrings(params, "custom_tools");
    if tool_mode == "custom" && custom_tools.is_empty() {
        return Err("custom tool mode requires at least one tool in custom_tools".to_string());
    }
    Ok(Some((tool_mode.to_string(), custom_tools)))
}

/// 工具模式白名单校验（session.new 预置与 session.set_tool_mode 共用）。
/// 白名单由 `qaqh_types::tool_mode::KNOWN_MODES` 单一契约提供（BUG-013）。
pub(crate) fn validate_tool_mode(tool_mode: &str) -> Result<(), String> {
    if qaqh_types::is_known(tool_mode) {
        Ok(())
    } else {
        Err(format!(
            "invalid tool_mode '{tool_mode}' (expected {})",
            qaqh_types::KNOWN_MODES.join(" | ")
        ))
    }
}
