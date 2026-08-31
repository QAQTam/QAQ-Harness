//! Write-conflict detection for tool execution.
//!
//! Detects same-file write conflicts among pending tool calls and groups them
//! into serial execution sets to avoid race conditions.

use std::collections::{HashMap, HashSet};

use serde_json;

/// Extract file paths that a tool writes to (mutates).
/// Returns empty vec for read-only and non-file tools.
pub(crate) fn file_write_paths(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    let action = if tool_name == "file" {
        args.get("action").and_then(|v| v.as_str()).unwrap_or("")
    } else {
        tool_name
    };

    match action {
        "patch" | "write" | "edit_file" | "edit" | "edit_diff" | "delete" => {
            collect_paths(args, &mut paths);
        }
        "move" | "copy" => {
            // Both source and dest are affected; dest is the write target
            if let Some(p) = args.get("dest").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            if let Some(p) = args.get("source").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        }
        "todo" => {
            // Todo is session state rather than a workspace file, but multiple
            // model calls in one tool round still share one ordered ID stream.
            // A synthetic conflict key preserves model call order even when a
            // provider ignores the guidance to use one create(items=[...]).
            paths.push("__qaqh_session_todo__".to_string());
        }
        "copy_range" => {
            // M1：写目标是 target_path（source_path 为读端，不参与写冲突）。
            if let Some(p) = args.get("target_path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        }
        "apply_patch" => {
            // M1：目标在 patch 文本里（Codex 格式头），解析后按同文件分组；
            // 原名单只匹配 "patch" 等旧名，apply_patch 双调用曾并行竞态。
            if let Some(patch) = args.get("patch").and_then(|v| v.as_str()) {
                for target in qaqh_workspace::permission::patch_target_paths(patch) {
                    paths.push(target);
                }
            }
        }
        _ => {}
    }
    paths
}

fn collect_paths(args: &serde_json::Value, paths: &mut Vec<String>) {
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        paths.push(p.to_string());
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        for value in arr {
            if let Some(path) = value.as_str() {
                paths.push(path.to_string());
            }
        }
    }
}

/// Detect same-file write conflicts among pending tools and group them
/// into serial execution sets. Returns (serial_groups, serial_after_indices).
pub(crate) fn resolve_write_conflicts(
    pending: &[qaqh_message::PendingTool],
) -> (Vec<Vec<usize>>, HashSet<usize>) {
    let mut file_writers: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, tool) in pending.iter().enumerate() {
        for path in file_write_paths(&tool.name, &tool.args) {
            file_writers.entry(path).or_default().push(i);
        }
    }
    let mut serial_groups: Vec<Vec<usize>> = Vec::new();
    {
        let mut visited = vec![false; pending.len()];
        for indices in file_writers.values() {
            if indices.is_empty() {
                continue;
            }
            let rep = indices[0];
            if visited[rep] {
                continue;
            }
            let mut group_set: HashSet<usize> = HashSet::new();
            let mut stack: Vec<usize> = indices.clone();
            while let Some(idx) = stack.pop() {
                if !group_set.insert(idx) {
                    continue;
                }
                visited[idx] = true;
                for other in file_writers.values() {
                    if other.contains(&idx) {
                        for &oi in other {
                            if !group_set.contains(&oi) {
                                stack.push(oi);
                            }
                        }
                    }
                }
            }
            let mut group: Vec<usize> = group_set.into_iter().collect();
            group.sort();
            if group.len() > 1 {
                serial_groups.push(group);
            }
        }
    }
    let mut serial_after: HashSet<usize> = HashSet::new();
    for group in &serial_groups {
        for &idx in &group[1..] {
            serial_after.insert(idx);
        }
    }
    (serial_groups, serial_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_message::PendingTool;

    fn tool(id: &str, name: &str, path: &str) -> PendingTool {
        PendingTool {
            id: id.to_string(),
            name: name.to_string(),
            args: serde_json::json!({"path": path}),
        }
    }

    #[test]
    fn flat_file_mutations_on_same_path_are_serialized() {
        let pending = vec![
            tool("write-1", "write", "src/lib.rs"),
            tool("edit-1", "edit_file", "src/lib.rs"),
            tool("delete-1", "delete", "src/other.rs"),
        ];

        let (groups, serial_after) = resolve_write_conflicts(&pending);

        assert_eq!(groups, vec![vec![0, 1]]);
        assert_eq!(serial_after, HashSet::from([1]));
    }

    #[test]
    fn same_file_double_apply_patch_serialized() {
        let mk = |id: &str, target: &str| PendingTool {
            id: id.to_string(),
            name: "apply_patch".to_string(),
            args: serde_json::json!({
                "patch": format!(
                    "*** Begin Patch\n*** Update File: {target}\n@@\n-old\n+new\n*** End Patch"
                )
            }),
        };
        let pending = vec![mk("ap-1", "src/a.rs"), mk("ap-2", "src/a.rs")];
        let (groups, serial_after) = resolve_write_conflicts(&pending);
        assert_eq!(groups, vec![vec![0, 1]]);
        assert_eq!(serial_after, HashSet::from([1]));
    }

    #[test]
    fn copy_range_write_targets_are_grouped() {
        let pending = vec![
            PendingTool {
                id: "cr-1".into(),
                name: "copy_range".into(),
                args: serde_json::json!({"source_path":"a.txt","target_path":"out/b.txt"}),
            },
            PendingTool {
                id: "cr-2".into(),
                name: "copy_range".into(),
                args: serde_json::json!({"source_path":"c.txt","target_path":"out/b.txt"}),
            },
        ];
        let (groups, serial_after) = resolve_write_conflicts(&pending);
        assert_eq!(groups, vec![vec![0, 1]]);
        assert_eq!(serial_after, HashSet::from([1]));
    }

    #[test]
    fn independent_file_mutations_remain_parallel() {
        let pending = vec![
            tool("write-1", "write", "src/a.rs"),
            tool("edit-1", "edit_file", "src/b.rs"),
        ];

        let (groups, serial_after) = resolve_write_conflicts(&pending);

        assert!(groups.is_empty());
        assert!(serial_after.is_empty());
    }

    #[test]
    fn todo_mutations_are_serialized_in_model_order() {
        let pending = vec![
            tool("todo-1", "todo", ""),
            tool("todo-2", "todo", ""),
            tool("read-1", "read", ""),
            tool("todo-3", "todo", ""),
        ];

        let (groups, serial_after) = resolve_write_conflicts(&pending);

        assert_eq!(groups, vec![vec![0, 1, 3]]);
        assert_eq!(serial_after, HashSet::from([1, 3]));
    }
}
