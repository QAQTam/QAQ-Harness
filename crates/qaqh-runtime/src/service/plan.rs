//! service::plan — token 统计 + plan 读写自由函数。

use serde_json::{Value, json};

use std::io::BufRead;

use super::common::err;

use super::fs_git::qaqh_dir;

pub(crate) fn token_stats(days: u32) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let days = days.max(1);
    let cutoff = days_before_today(days);
    let mut daily: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(file) =
        std::fs::File::open(qaqh_types::platform::data_dir().join("token_stats.jsonl"))
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let date = entry["date"].as_str().unwrap_or_default().to_string();
            if date < cutoff {
                continue;
            }
            let day=daily.entry(date).or_insert_with(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_hit",
                "cache_miss",
            ] {
                day[key] = json!(day[key].as_u64().unwrap_or(0) + entry[key].as_u64().unwrap_or(0));
            }
            day["calls"] = json!(day["calls"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut values = Vec::new();
    let mut prompt = 0;
    let mut completion = 0;
    let mut hit = 0;
    let mut miss = 0;
    let mut calls = 0;
    for offset in (0..days).rev() {
        let date = days_before_today(offset);
        let entry=daily.get(&date).cloned().unwrap_or_else(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
        prompt += entry["prompt_tokens"].as_u64().unwrap_or(0);
        completion += entry["completion_tokens"].as_u64().unwrap_or(0);
        hit += entry["cache_hit"].as_u64().unwrap_or(0);
        miss += entry["cache_miss"].as_u64().unwrap_or(0);
        calls += entry["calls"].as_u64().unwrap_or(0);
        values.push(json!({"date":date,"prompt_tokens":entry["prompt_tokens"],"completion_tokens":entry["completion_tokens"],"cache_hit":entry["cache_hit"],"cache_miss":entry["cache_miss"],"calls":entry["calls"]}));
    }
    let pct = if hit + miss > 0 {
        (hit as f64 / (hit + miss) as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };
    Ok(
        json!({"daily":values,"totals":{"prompt_tokens":prompt,"completion_tokens":completion,"calls":calls,"cache_hit_pct":pct}}),
    )
}
pub(crate) fn days_before_today(days: u32) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d) = qaqh_types::platform::civil_from_days((seconds / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn read_plan(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
) -> Result<Value, String> {
    let content = match std::fs::read_to_string(qaqh_dir(sessions, seed).join("PLAN.md")) {
        Ok(value) => value,
        Err(_) => return Ok(json!([])),
    };
    let items = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("- [") {
                return None;
            }
            let end = line.find(']')?;
            let status = line.get(3..end)?.trim();
            let rest = line.get(end + 1..)?.trim();
            let (id, title) = rest.split_once(": ")?;
            Some(json!({"id":id,"title":title,"status":status,"comment":"","actions":[]}))
        })
        .collect();
    Ok(Value::Array(items))
}
pub(crate) fn plan_action(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
    item_id: &str,
    action: &str,
    comment: &str,
) -> Result<(), String> {
    let path = qaqh_dir(sessions, seed).join("PLAN.md");
    let content = std::fs::read_to_string(&path).map_err(err)?;
    let mut found = false;
    let output = content
        .lines()
        .filter_map(|line| {
            if !found && line.trim().starts_with("- [") && line.contains(&format!(" {item_id}: ")) {
                found = true;
                if action == "delete" {
                    return None;
                }
                let end = line.find(']')?;
                // ']' 为单字节 ASCII，end+1 必为 char boundary。
                let rest = line.split_at(end + 1).1;
                let base = format!("- [ ]{rest}");
                return Some(match action {
                    "approve" => base.replacen("- [ ]", "- [✓]", 1),
                    "reject" => {
                        let value = base.replacen("- [ ]", "- [-]", 1);
                        if comment.is_empty() {
                            value
                        } else {
                            format!("{value} | {comment}")
                        }
                    }
                    "ask" => base.replacen("- [ ]", "- [?]", 1),
                    _ => line.to_string(),
                });
            }
            Some(line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !found {
        return Err(format!("plan item {item_id} not found"));
    }
    std::fs::write(path, output).map_err(err)
}
