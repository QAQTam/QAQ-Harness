//! service::stats — dashboard/activity/config/context_stats 查询函数。

use serde_json::{Value, json};

use std::io::BufRead;

use super::common::err;

pub(crate) fn dashboard(seed: &str) -> Result<Value, String> {
    let dir = qaqh_types::platform::sessions_dir().join(seed);
    let tasks: Vec<Value> = qaqh_workspace::todo::todo_status_json(seed)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("items")?.as_array().map(|arr| {
                arr.iter()
                    .map(|item| {
                        json!({
                            "id": item["id"],
                            "subject": item["title"],
                            "description": item["description"],
                            "status": item["status"],
                            "evidence": item["evidence"],
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let mut edits = std::fs::File::open(dir.join("code_stats.jsonl"))
        .ok()
        .into_iter()
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    edits.reverse();
    edits.dedup();
    edits.truncate(10);
    Ok(json!({"tasks":tasks,"recent_edits":edits}))
}

pub(crate) fn activity(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
) -> Result<Value, String> {
    let (_, messages) = sessions
        .load(seed)
        .ok_or_else(|| "session not found".to_string())?;
    let mut tools = std::collections::HashMap::new();
    for message in &messages {
        if message.role == "assistant" {
            for block in &message.content {
                if let qaqh_types::ContentBlock::ToolUse { id, name, input } = block {
                    tools.insert(id.clone(), (name.clone(), input.to_string()));
                }
            }
        }
    }
    let mut result = Vec::new();
    for message in &messages {
        if message.role == "tool" {
            for block in &message.content {
                if let qaqh_types::ContentBlock::ToolResult {
                    tool_use_id,
                    result: tool_result,
                } = block
                {
                    let (name, args) = tools.get(tool_use_id).cloned().unwrap_or_default();
                    result.push(json!({"tool_name":name,"summary":tool_result.summary,"status":serde_json::to_value(tool_result.status).unwrap_or_default(),"time":message.msg_id.map(|v|v.to_string()).unwrap_or_default(),"args":args}));
                }
            }
        }
    }
    result.reverse();
    Ok(Value::Array(result))
}

pub(crate) fn load_config() -> Result<Value, String> {
    let cfg = qaqh_config::Config::load().map_err(err)?;
    // P1-C2：读模型统一走 ConfigDto（camelCase wire + providers 目录内聚到
    // qaqh-config::dto），service 层不再手拼 json。
    serde_json::to_value(qaqh_config::dto::to_dto(&cfg)).map_err(err)
}
pub(crate) fn context_stats(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
) -> Result<Value, String> {
    // 统一数据源：meta.json 的 context_stats 字段（原独立文件退役）。
    // 旧 context_stats.json 为可再生缓存，忽略不迁移。
    if let Some(meta) = sessions.load_meta(seed)
        && let Some(stats) = meta.context_stats
    {
        return Ok(stats);
    }
    Ok(
        json!({"messages":0,"chat_text":0,"thinking":0,"tool_calls":0,"tool_results":0,"tools_schema":0,"system_prompt":0,"thinking_blocks":0,"tool_call_blocks":0}),
    )
}
