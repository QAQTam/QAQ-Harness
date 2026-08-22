//! Code-delta calculation for successful file mutations.

pub(crate) fn compute(
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<qaqh_proto::CodeDeltaRecord> {
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
            Some(qaqh_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: content.lines().count(),
                lines_removed: 0,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        ("delete", _) => Some(qaqh_proto::CodeDeltaRecord {
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
            Some(qaqh_proto::CodeDeltaRecord {
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
    let seed = crate::current_session()?;
    // 无 SessionManager 环境（serve 进程）用只读版；meta.cwd 优先，
    // 旧 workspace.txt 兜底（存量，不迁移）。
    let workspace = qaqh_session::workspace::session_workspace_from_disk(&seed)?;
    let repo = git2::Repository::open(workspace).ok()?;
    let head_tree = repo.head().ok()?.peel_to_tree().ok()?;
    let is_new = head_tree.get_path(std::path::Path::new(file_path)).is_err();
    Some(GitFileMeta {
        files_created: usize::from(is_new),
        files_deleted: 0,
    })
}
