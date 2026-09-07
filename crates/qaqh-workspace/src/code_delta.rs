//! Code-delta calculation for successful file mutations.

pub(crate) fn compute(
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<qaqh_domain::CodeDeltaRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or(tool_name);
    let file_path = args.get("path").and_then(|value| value.as_str());

    // Compute text-based line counts from args (cheap, no git2 pathspec bug).
    let mut delta = match (tool_name, action) {
        ("file", "write") => {
            let content = args
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(qaqh_domain::CodeDeltaRecord {
                timestamp: now,
                lines_added: content.lines().count(),
                lines_removed: 0,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        ("delete", _) => Some(qaqh_domain::CodeDeltaRecord {
            timestamp: now,
            lines_added: 0,
            lines_removed: 0,
            files_created: 0,
            files_deleted: 1,
            file: file_path.map(String::from),
        }),
        ("edit", _) => {
            // v2：单文件 path + hunks（内容锚定：`old` 删除行数、`new` 新增行数）。
            let mut added = 0usize;
            let mut removed = 0usize;
            if let Some(hunks) = args.get("hunks").and_then(|x| x.as_array()) {
                for h in hunks {
                    if let Some(new) = h.get("new").and_then(|x| x.as_str()) {
                        added += new.lines().count();
                    }
                    if let Some(old) = h.get("old").and_then(|x| x.as_str()) {
                        removed += old.lines().count();
                    }
                }
            }
            Some(qaqh_domain::CodeDeltaRecord {
                timestamp: now,
                lines_added: added,
                lines_removed: removed,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        _ => None,
    };

    // Override files_created / files_deleted from git when available
    // (git2::Repository::open is a cheap metadata op — no diff, no
    // pathspec bug since we only check HEAD tree existence).
    if let (Some(path), Some(d)) = (file_path, &mut delta)
        && let Some(git) = git_file_meta(path)
    {
        d.files_created = git.files_created;
        d.files_deleted = git.files_deleted;
    }

    delta
}

/// Lightweight git file metadata — only checks HEAD tree existence, no diff.
/// Avoids the git2 pathspec bug that inflated lines_added / lines_removed.
struct GitFileMeta {
    files_created: usize,
    files_deleted: usize,
}

fn git_file_meta(file_path: &str) -> Option<GitFileMeta> {
    crate::current_session()?;
    // PR-3-3：cwd 由宿主注入（daemon 会话初始化 / serve 请求体均已 set）。
    // 读注入值本身：空 / "." 视为无有效工作区（与旧只读磁盘解析的
    // “无 cwd → 无 git meta”语义对齐）。
    let workspace = crate::current_workspace();
    if workspace.is_empty() || workspace == "." {
        return None;
    }
    let repo = git2::Repository::open(workspace).ok()?;
    let head_tree = repo.head().ok()?.peel_to_tree().ok()?;
    let is_new = head_tree.get_path(std::path::Path::new(file_path)).is_err();
    Some(GitFileMeta {
        files_created: usize::from(is_new),
        files_deleted: 0,
    })
}
