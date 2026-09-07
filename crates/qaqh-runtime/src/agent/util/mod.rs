//! Utility functions for the message loop: calendar, token logging, tool display formatting.
//! TODO: split into `datetime.rs` (chrono_local_date, chrono_local_datetime, epoch_to_date)
//! and `format.rs` (resolve_effective_name, has_xml, format_tool_args_display).

use crate::agent::state::agent::AgentState;
use qaqh_types;

/// Convert epoch seconds to human-readable UTC date.
pub(crate) fn epoch_to_date(epoch_secs: u64) -> String {
    use qaqh_types::platform::civil_from_days;
    let total_days = (epoch_secs / 86400) as i64;
    let (y, m, d) = civil_from_days(total_days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Resolve a legacy `name`/`action` pair before policy evaluation.
pub(crate) fn resolve_effective_name(
    name: &str,
    action: &str,
    _args: &serde_json::Value,
) -> String {
    if action.is_empty() {
        name.to_string()
    } else {
        format!("{name}_{action}")
    }
}

/// Return today's date as "YYYY-MM-DD" (UTC+8).
pub(crate) fn chrono_local_date() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM".
pub(crate) fn chrono_local_datetime() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

/// Append per-turn token usage to `token_stats.jsonl` for dashboard aggregation.
pub(crate) fn record_token_usage(usage: &qaqh_types::UsageInfo, model: &str) {
    use std::io::Write;
    let dir = qaqh_types::platform::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("token_stats.jsonl");
    let today = chrono_local_date();
    let line = serde_json::json!({
        "date": today,
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "cache_hit": usage.prompt_cache_hit_tokens,
        "cache_miss": usage.prompt_cache_miss_tokens,
        "model": model,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

pub(crate) fn has_xml(s: &str) -> bool {
    // Require <tool_calls> wrapper to avoid false positives from
    // examples, explanations, or markdown containing bare <invoke> tags.
    s.contains("<tool_calls>")
}

/// Extract a short human-readable display string from a tool call's arguments.
pub(crate) fn format_tool_args_display(name: &str, input: &serde_json::Value) -> String {
    let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let display_name = if action.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", name, action)
    };

    match name {
        "exec" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| c.chars().take(80).collect())
            .unwrap_or(display_name),
        "file" => {
            let path = match action {
                "search" => input.get("pattern").and_then(|v| v.as_str()),
                "move" | "copy" => input.get("dest").and_then(|v| v.as_str()),
                "diff" => input.get("path_b").and_then(|v| v.as_str()),
                _ => input.get("path").and_then(|v| v.as_str()),
            };
            path.map(|p| p.chars().take(60).collect::<String>())
                .unwrap_or(display_name)
        }
        "todo" => input
            .get("title")
            .or_else(|| input.get("subject"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(60).collect::<String>())
            .unwrap_or(display_name),
        "web_fetch" => input
            .get("url")
            .or_else(|| input.get("query"))
            .or_else(|| input.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(80).collect())
            .unwrap_or(display_name),
        "process" => input
            .get("id")
            .and_then(|v| v.as_u64())
            .map(|id| id.to_string())
            .unwrap_or(display_name),
        "ask_user" => input
            .get("question")
            .and_then(|v| v.as_str())
            .map(|q| q.chars().take(60).collect())
            .unwrap_or(display_name),
        _ => display_name,
    }
}

// (projection family moved to qaqh-runtime::ringing::projection — PR-1-4)

pub(crate) fn parse_tool_calls_from_response(
    content: &str,
    _reasoning: &str,
    tool_calls_raw: &serde_json::Value,
    agent: &AgentState,
) -> Vec<qaqh_types::ToolCall> {
    let mut parsed = qaqh_gate::tool_parser::parse_tool_calls(tool_calls_raw);
    if parsed.is_empty() {
        let stripped = qaqh_gate::tool_parser::strip_fenced_code(content);
        if qaqh_gate::tool_parser::has_dsml(&stripped) {
            let (_, dsml) =
                qaqh_gate::tool_parser::parse_dsml_tool_calls(&stripped, &agent.tool_defs);
            if !dsml.is_empty() {
                parsed = dsml;
            }
        }
        if parsed.is_empty() && has_xml(content) {
            let names: Vec<String> = agent
                .tool_defs
                .iter()
                .map(|t| t.function.name.clone())
                .collect();
            let stripped2 = qaqh_gate::tool_parser::strip_fenced_code(content);
            let (_, xml) = qaqh_gate::tool_parser::parse_xml_tool_calls(&stripped2, &names);
            if !xml.is_empty() {
                parsed = xml;
            }
        }
    }
    parsed
}

pub(crate) fn build_assistant_message(
    content: &str,
    reasoning: &str,
    parsed: &[qaqh_types::ToolCall],
) -> qaqh_types::Message {
    use qaqh_types::{ContentBlock, Message};
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning.to_string(),
        });
    }
    if !content.is_empty() {
        blocks.push(ContentBlock::Text {
            text: content.to_string(),
        });
    }
    for tc in parsed {
        let input: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
        blocks.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            input,
        });
    }
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    }
}

/// Emitter-trait version of emit_round_complete for the new Loop architecture.
pub(crate) fn emit_round_complete_via_emitter(
    emitter: &dyn crate::agent::types::Emitter,
    turn_id: &str,
    round_num: u32,
    assistant_msg: &qaqh_types::Message,
    _content: &str,
    _reasoning: &str,
    _parsed: &[qaqh_types::ToolCall],
) {
    use qaqh_types::ContentBlock;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for cb in &assistant_msg.content {
        match cb {
            ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                blocks.push(qaqh_domain::RoundBlock::Reasoning {
                    content: reasoning.clone(),
                });
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(qaqh_domain::RoundBlock::Text {
                    content: text.clone(),
                });
            }
            ContentBlock::ToolUse { id, name, input } => {
                let display = format_tool_args_display(name, input);
                tool_calls.push(qaqh_domain::ToolCallDef {
                    id: id.clone(),
                    name: name.clone(),
                    args_display: display.clone(),
                    args_json: input.to_string(),
                });
                blocks.push(qaqh_domain::RoundBlock::Tool {
                    card: qaqh_domain::ToolCallDef {
                        id: id.clone(),
                        name: name.clone(),
                        args_display: display,
                        args_json: input.to_string(),
                    },
                });
            }
            ContentBlock::WebSearchCall { action, .. } => {
                blocks.push(qaqh_domain::RoundBlock::WebSearch {
                    action: action.to_string(),
                });
            }
            _ => {}
        }
    }
    // Ringing 双发：RoundCompleted（该 round 的权威全量终态，携带全量
    // thinking/answer）。daemon 收到后折叠该 round 的 RoundDelta；否则每个
    // token 增量都会永久累积在 conversation journal 中（磁盘与内存无界
    // 增长——曾实测 3 个 turn 积累 36k 条 delta / 18.5MB）。
    emitter.emit_domain(qaqh_domain::DomainEvent::Conversation(
        qaqh_domain::ConversationEvent::RoundCompleted {
            turn_id: turn_id.into(),
            round_num,
            thinking: if _reasoning.is_empty() {
                None
            } else {
                Some(_reasoning.into())
            },
            answer: if _content.is_empty() {
                None
            } else {
                Some(_content.into())
            },
            output_ref: None,
            is_final: tool_calls.is_empty(),
        },
    ));
}
