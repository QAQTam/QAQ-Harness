use qaqh_types::{FunctionCall, ToolCall, ToolDef};

pub fn has_dsml(content: &str) -> bool {
    // Trigger only when <｜DSML｜tool_calls> wrapper is present.
    // (Parser already requires the wrapper; this just avoids wasted scans.)
    let lower = content.to_lowercase();
    lower.contains("dsml") && lower.contains("tool_calls")
}

/// Strip markdown code fences from content so tool call examples
/// inside ``` blocks are not accidentally parsed as real tool calls.
pub fn strip_fenced_code(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_fence = false;
    for line in content.lines() {
        if line.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
        } else if in_fence {
            out.push('\n'); // preserve line count, discard content
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Parse DeepSeek v4 XML-style tool calls from text content.
/// Caller MUST strip markdown code fences before passing content.
pub fn parse_xml_tool_calls(content: &str, tool_names: &[String]) -> (String, Vec<ToolCall>) {
    let mut cleaned = String::with_capacity(content.len());
    let mut tool_calls = Vec::new();
    let mut tc_index = 0u32;

    let mut tag_map: Vec<(String, String)> =
        tool_names.iter().map(|n| (n.clone(), n.clone())).collect();
    for (alias, name) in &[
        ("read", "file"),
        ("read", "file"),
        ("write", "file"),
        ("edit_file", "file"),
    ] {
        if tool_names.iter().any(|n| n == name) {
            tag_map.push((alias.to_string(), name.to_string()));
        }
    }

    let mut remaining = content;

    'outer: while let Some(tc_start) = remaining.find('<') {
        if remaining[tc_start..].starts_with("<tool_use>") {
            if let Some((name, args_text, rest)) = parse_tool_use_block(&remaining[tc_start..]) {
                cleaned.push_str(&remaining[..tc_start]);
                let arguments = normalize_args(&args_text);
                tool_calls.push(ToolCall {
                    id: format!("xml_tc_{tc_index}"),
                    call_type: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
                tc_index += 1;
                remaining = rest;
                continue 'outer;
            }
        }

        if remaining[tc_start..].starts_with("<invoke ") {
            if let Some((name, args_text, rest)) = parse_invoke_block(&remaining[tc_start..]) {
                cleaned.push_str(&remaining[..tc_start]);
                let arguments = normalize_args(&args_text);
                tool_calls.push(ToolCall {
                    id: format!("xml_tc_{tc_index}"),
                    call_type: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
                tc_index += 1;
                remaining = rest;
                continue 'outer;
            }
        }

        let after_lt = &remaining[tc_start..];
        if let Some(end) = after_lt.find('>') {
            let tag_name = after_lt
                .strip_prefix('<')
                .and_then(|s| s.get(..end - 1))
                .unwrap_or("");
            let closing = format!("</{tag_name}>");

            if let Some((_, tool_name)) = tag_map.iter().find(|(tag, _)| *tag == tag_name) {
                if let Some(close_pos) = after_lt[end..].find(&closing) {
                    let block_content = &after_lt[end + 1..end + close_pos];
                    let full_xml_len = end + close_pos + closing.len();

                    cleaned.push_str(&remaining[..tc_start]);

                    let args = extract_child_args(block_content);
                    let arguments = normalize_args(&args);

                    tool_calls.push(ToolCall {
                        id: format!("xml_tc_{tc_index}"),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: tool_name.to_string(),
                            arguments,
                        },
                    });
                    tc_index += 1;
                    remaining = &after_lt[full_xml_len..];
                    continue 'outer;
                }
            }
        }

        cleaned.push_str(&remaining[..tc_start + 1]);
        remaining = &remaining[tc_start + 1..];
    }

    cleaned.push_str(remaining);
    (cleaned.trim().to_string(), tool_calls)
}

fn parse_tool_use_block(s: &str) -> Option<(String, String, &str)> {
    let prefix = "<tool_use>";
    let suffix = "</tool_use>";
    let s = s.strip_prefix(prefix)?;
    let end = s.find(suffix)?;
    let block = &s[..end];
    let rest = &s[end + suffix.len()..];

    let n_open = "<tool_name>";
    let n_close = "</tool_name>";
    let ns = block.find(n_open)?;
    let after_ns = &block[ns + n_open.len()..];
    let ne = after_ns.find(n_close)?;
    let name = after_ns[..ne].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let args_text = after_ns[ne + n_close.len()..].trim().to_string();
    Some((name, args_text, rest))
}

fn parse_invoke_block(s: &str) -> Option<(String, String, &str)> {
    let s = s.strip_prefix("<invoke ")?;
    let name = extract_attr_value(s, "name")?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let close_tag = s.find('>')?;
    let after_open = &s[close_tag + 1..];
    let end_tag = "</invoke>";
    let end = after_open.find(end_tag)?;
    let body = &after_open[..end];
    let rest = &after_open[end + end_tag.len()..];

    let param_tag = "<parameter ";
    let param_close = "</parameter>";
    let mut args_map = serde_json::Map::new();
    let mut rem = body;
    loop {
        let Some(p) = rem.find(param_tag) else { break };
        let after_p = &rem[p + param_tag.len()..];
        let param_name = extract_attr_value(after_p, "name")?.trim().to_string();
        let str_attr = extract_attr_value(after_p, "string").unwrap_or_default();
        let Some(gt) = after_p.find('>') else { break };
        let content_start = &after_p[gt + 1..];
        let Some(close) = content_start.find(param_close) else {
            break;
        };
        let value = content_start[..close].trim();
        rem = &content_start[close + param_close.len()..];

        let json_val = if str_attr == "true" {
            serde_json::json!(value)
        } else {
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value))
        };
        args_map.insert(param_name, json_val);
    }

    let args = serde_json::to_string(&args_map).unwrap_or_else(|_| "{}".into());
    Some((name, args, rest))
}

fn extract_child_args(xml: &str) -> String {
    let mut map = serde_json::Map::new();
    let mut s = xml.trim();
    while let Some(lt) = s.find('<') {
        if s[lt..].starts_with("</") || s[lt..].starts_with("<![") {
            s = &s[lt + 1..];
            continue;
        }
        if let Some(gt) = s[lt..].find('>') {
            let tag = &s[lt + 1..lt + gt];
            let closing = format!("</{tag}>");
            if let Some(close) = s[lt + gt + 1..].find(&closing) {
                let value = s[lt + gt + 1..lt + gt + 1 + close].trim();
                map.insert(tag.to_string(), serde_json::json!(value));
                s = &s[lt + gt + 1 + close + closing.len()..];
            } else {
                s = &s[lt + 1..];
            }
        } else {
            break;
        }
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

fn normalize_args(args_text: &str) -> String {
    let trimmed = args_text.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return trimmed.to_string();
        }
    }
    serde_json::json!({"query": trimmed}).to_string()
}

/// Parse DeepSeek DSML tool calls from content.
/// Schema-aware: uses tool_defs for type-safe parameter conversion.
/// Format: <\u{ff5c}DSML\u{ff5c}tool_calls>...<｜DSML｜invoke name="fn">...</｜DSML｜invoke>...
/// Format: <\u{ff5c}DSML\u{ff5c}tool_calls>...<｜DSML｜invoke name="fn">...</｜DSML｜invoke>...
/// Caller MUST strip markdown code fences before passing content.
pub fn parse_dsml_tool_calls(content: &str, tool_defs: &[ToolDef]) -> (String, Vec<ToolCall>) {
    let owned;
    let content = {
        let s = content
            .replace("|DSML|", "\u{ff5c}DSML\u{ff5c}")
            .replace("|DSML", "\u{ff5c}DSML")
            .replace("DSML|", "DSML\u{ff5c}");
        owned = s;
        owned.as_str()
    };

    // Normalize double-bar variant ￌￌDSMLￌￌ → ￌDSMLￌ
    let normalized;
    let content = if content.contains("\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}") {
        normalized = content.replace(
            "\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}",
            "\u{ff5c}DSML\u{ff5c}",
        );
        normalized.as_str()
    } else {
        content
    };

    let dsml = "\u{ff5c}DSML\u{ff5c}";
    let tc_start = format!("<{dsml}tool_calls>");
    let tc_end = format!("</{dsml}tool_calls>");
    let invoke_tag = format!("<{dsml}invoke ");
    let invoke_close = format!("</{dsml}invoke>");
    let param_tag = format!("<{dsml}parameter ");
    let param_close = format!("</{dsml}parameter>");

    // Build schema lookup: function_name → property_name → {type}
    let mut schema_map: std::collections::HashMap<
        &str,
        &serde_json::Map<String, serde_json::Value>,
    > = std::collections::HashMap::new();
    for td in tool_defs {
        if let Some(props) = td
            .function
            .parameters
            .get("properties")
            .and_then(|v| v.as_object())
        {
            schema_map.insert(td.function.name.as_str(), props);
        }
    }

    let mut cleaned = String::new();
    let mut tool_calls = Vec::new();
    let mut idx = 0u32;

    let mut remaining = content;
    loop {
        let Some(tc_pos) = remaining.find(&tc_start) else {
            cleaned.push_str(remaining);
            break;
        };
        cleaned.push_str(&remaining[..tc_pos]);
        let after_tc = &remaining[tc_pos + tc_start.len()..];

        let Some(tc_end_pos) = after_tc.find(&tc_end) else {
            cleaned.push_str(&remaining[tc_pos..]);
            break;
        };
        let block = &after_tc[..tc_end_pos];
        remaining = &after_tc[tc_end_pos + tc_end.len()..];

        let mut bq = block;
        loop {
            let Some(inv_pos) = bq.find(&invoke_tag) else {
                break;
            };
            let after_inv = &bq[inv_pos + invoke_tag.len()..];
            let name = extract_attr_value(after_inv, "name").unwrap_or_default();
            let name = name.trim().to_string();
            if name.is_empty() {
                break;
            }

            let Some(inv_end) = after_inv.find(&invoke_close) else {
                break;
            };
            let body = &after_inv[..inv_end];
            bq = &after_inv[inv_end + invoke_close.len()..];

            let param_types = schema_map.get(name.as_str()).copied();
            let args = extract_dsml_params_typed(body, &param_tag, &param_close, param_types);

            tool_calls.push(ToolCall {
                id: format!("dsml_tc_{idx}"),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name,
                    arguments: args,
                },
            });
            idx += 1;
        }
    }

    (cleaned.trim().to_string(), tool_calls)
}

fn extract_attr_value(s: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let pos = s.find(&pattern)?;
    let after = &s[pos + pattern.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn repair_param_dict(
    map: &serde_json::Map<String, serde_json::Value>,
    param_types: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let allowed: std::collections::HashSet<&str> = param_types.keys().map(|k| k.as_str()).collect();
    for wrapper in &["arguments", "input"] {
        if map.len() == 1 && !allowed.contains(wrapper) {
            if let Some(inner) = map.get(*wrapper) {
                if let Some(obj) = inner.as_object() {
                    let obj_keys: std::collections::HashSet<&str> =
                        obj.keys().map(|k| k.as_str()).collect();
                    if obj_keys.is_subset(&allowed) {
                        return obj.clone();
                    }
                }
            }
        }
    }
    map.clone()
}

fn convert_param_value(value: &str, param_type: Option<&str>) -> serde_json::Value {
    if value.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }
    match param_type {
        Some("integer") | Some("int") => match value.parse::<i64>() {
            Ok(n) => serde_json::json!(n),
            Err(_) => serde_json::json!(value),
        },
        Some("number") | Some("float") => value
            .parse::<f64>()
            .map(|f| {
                if f == f.trunc() && f.is_finite() {
                    serde_json::json!(f as i64)
                } else {
                    serde_json::json!(f)
                }
            })
            .unwrap_or_else(|_| serde_json::json!(value)),
        Some("boolean") | Some("bool") => {
            let v = value.trim().to_lowercase();
            serde_json::json!(v == "true" || v == "1")
        }
        Some("array") | Some("object") => {
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value))
        }
        _ => {
            // unknown type → try JSON, fallback to string
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value))
        }
    }
}

fn extract_dsml_params_typed(
    body: &str,
    param_tag: &str,
    param_close: &str,
    param_types: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let mut map = serde_json::Map::new();
    let mut rem = body;
    loop {
        let Some(p) = rem.find(param_tag) else { break };
        let after = &rem[p + param_tag.len()..];
        let name = extract_attr_value(after, "name")
            .unwrap_or_default()
            .trim()
            .to_string();
        let str_attr = extract_attr_value(after, "string").unwrap_or_default();

        let Some(gte) = after.find('>') else { break };
        let after_gt = &after[gte + 1..];
        let Some(end) = after_gt.find(param_close) else {
            break;
        };
        let value_text = after_gt[..end].trim();
        rem = &after_gt[end + param_close.len()..];

        let value = if str_attr == "true" {
            serde_json::json!(value_text)
        } else {
            let param_type = param_types
                .and_then(|pt| pt.get(&name))
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str());
            convert_param_value(value_text, param_type)
        };
        map.insert(name, value);
    }

    if let Some(pt) = param_types {
        map = repair_param_dict(&map, pt);
    }

    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

/// Convert HP tool_calls JSON (flat or nested) to ToolCall vec.
pub fn parse_tool_calls(tcs: &serde_json::Value) -> Vec<ToolCall> {
    let arr = match tcs.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let (name, arguments) = if let Some(func) = tc.get("function") {
                let n = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let a = func
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                (n, a)
            } else {
                let n = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let a = tc.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                (n, a)
            };
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(ToolCall {
                id: id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: arguments.to_string(),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::{ToolDef, ToolFunction};

    #[test]
    fn test_parse_invoke_inside_tool_calls() {
        let content = "<tool_calls>\n<invoke name=\"read\">\n<parameter name=\"end_line\" string=\"false\">450</parameter>\n<parameter name=\"path\" string=\"true\">D:\\project\\QAQ-Harness\\foo.rs</parameter>\n<parameter name=\"start_line\" string=\"false\">300</parameter>\n</invoke>\n</tool_calls>";

        let tool_names: Vec<String> = vec!["read".into(), "exec".into()];
        let (_cleaned, tcs) = parse_xml_tool_calls(content, &tool_names);

        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "read");
        let args: serde_json::Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["end_line"], 450);
        assert_eq!(args["path"], "D:\\project\\QAQ-Harness\\foo.rs");
        assert_eq!(args["start_line"], 300);
    }

    #[test]
    fn test_parse_invoke_detection() {
        let content = "<tool_calls>\n<invoke name=\"read\">\n<parameter name=\"path\" string=\"true\">test.rs</parameter>\n</invoke>\n</tool_calls>";
        assert!(content.contains("<invoke "));
        assert!(content.contains("<tool_calls>"));
    }

    #[test]
    fn test_fenced_code_blocks_ignored() {
        let raw = "Here is an example:\n```xml\n<tool_calls>\n<invoke name=\"read\">\n<parameter name=\"path\" string=\"true\">test.rs</parameter>\n</invoke>\n</tool_calls>\n```\n\nBut this one is real:\n<invoke name=\"read\">\n<parameter name=\"path\" string=\"true\">real.rs</parameter>\n</invoke>";
        let content = strip_fenced_code(raw);

        let tool_names: Vec<String> = vec!["read".into()];
        let (_cleaned, tcs) = parse_xml_tool_calls(&content, &tool_names);

        // Should only extract the REAL one (outside fence), not the example
        assert_eq!(
            tcs.len(),
            1,
            "Expected 1 real tool call, got {}: {:?}",
            tcs.len(),
            tcs
        );
        let args: serde_json::Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["path"], "real.rs");
    }

    #[test]
    fn test_dsml_fenced_code_blocks_ignored() {
        let raw = "Example:\n```\n<\u{ff5c}DSML\u{ff5c}tool_calls>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"read\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"path\" string=\"true\">test.rs</\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n</\u{ff5c}DSML\u{ff5c}tool_calls>\n```\n\nReal call:\n<\u{ff5c}DSML\u{ff5c}tool_calls>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"read\">\n<\u{ff5c}DSML\u{ff5c}parameter name=\"path\" string=\"true\">real.rs</\u{ff5c}DSML\u{ff5c}parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n</\u{ff5c}DSML\u{ff5c}tool_calls>";
        let content = strip_fenced_code(raw);

        let tool_defs: Vec<ToolDef> = vec![ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: "read".into(),
                description: "Read file".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }),
            },
        }];

        let (_cleaned, tcs) = parse_dsml_tool_calls(&content, &tool_defs);
        assert_eq!(
            tcs.len(),
            1,
            "Expected 1 real tool call, got {}: {:?}",
            tcs.len(),
            tcs
        );
        let args: serde_json::Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["path"], "real.rs");
    }

    #[test]
    fn test_has_dsml_detection() {
        assert!(has_dsml("use <\u{ff5c}DSML\u{ff5c}tool_calls>"));
        assert!(has_dsml("dsml tool_calls"));

        assert!(!has_dsml("plain text without markers"));
        assert!(!has_dsml(""));
        assert!(!has_dsml("dsml only"));
        assert!(!has_dsml("use <|DSML|invoke name=\"read\">"));
        assert!(!has_dsml("DSML invoke read"));
        assert!(!has_dsml("dsml parameter path"));
        assert!(!has_dsml("just invoke tool_calls parameter"));
    }

    #[test]
    fn test_halfwidth_dsml_parsing() {
        let content = "I'll read the file.\n\n<|DSML|tool_calls>\n<|DSML|invoke name=\"read\">\n<|DSML|parameter name=\"path\" string=\"true\">/tmp/test.txt\n</|DSML|parameter>\n<|DSML|parameter name=\"start_line\" string=\"false\">1\n</|DSML|parameter>\n</|DSML|invoke>\n</|DSML|tool_calls>";

        assert!(
            has_dsml(content),
            "Fuzzy detection should catch halfwidth DSML"
        );

        let tool_defs: Vec<ToolDef> = vec![ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: "read".into(),
                description: "Read file".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer" }
                    }
                }),
            },
        }];

        let (cleaned, tcs) = parse_dsml_tool_calls(content, &tool_defs);
        assert_eq!(
            tcs.len(),
            1,
            "Expected 1 tool call, got {}: {:?}",
            tcs.len(),
            tcs
        );
        assert_eq!(tcs[0].function.name, "read");
        let args: serde_json::Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["path"], "/tmp/test.txt");
        assert_eq!(args["start_line"], 1);
        assert!(
            !cleaned.contains("DSML"),
            "Cleaned content should not contain DSML tags"
        );
    }

    #[test]
    fn test_mixed_pipe_dsml_parsing() {
        let content = "Text.\n<|DSML|tool_calls>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"exec\">\n<|DSML|parameter name=\"command\" string=\"true\">ls\n</|DSML|parameter>\n</\u{ff5c}DSML\u{ff5c}invoke>\n</|DSML|tool_calls>";

        assert!(has_dsml(content));

        let tool_defs: Vec<ToolDef> = vec![ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: "exec".into(),
                description: "Execute command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    }
                }),
            },
        }];

        let (cleaned, tcs) = parse_dsml_tool_calls(content, &tool_defs);
        assert_eq!(
            tcs.len(),
            1,
            "Expected 1 tool call, got {}: {:?}",
            tcs.len(),
            tcs
        );
        assert_eq!(tcs[0].function.name, "exec");
        let args: serde_json::Value = serde_json::from_str(&tcs[0].function.arguments).unwrap();
        assert_eq!(args["command"], "ls");
        assert!(!cleaned.contains("DSML"));
    }
}
