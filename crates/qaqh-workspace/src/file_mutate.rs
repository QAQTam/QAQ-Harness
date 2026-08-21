//! Mutation tools: write, delete（统一编辑入口见 file_edit_v2.rs 的 edit）。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::file_shared::{
    atomic_write, content_hash, diff_stats_between, normalize_newlines, unified_diff,
    verify_expected_hash,
};
use crate::{
    JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler, handler_from_string,
};
use serde_json::json;

// ── Shared helpers ──

/// 成功摘要行：模型视角默认不回传 diff 正文（省上下文），只给可验证的
/// 变更统计（路径、首行、+N -M，真实增减行数）。需要预览时用 dry_run=true 单独请求。
fn format_write_result(
    prefix: &str,
    path: &str,
    added: u32,
    removed: u32,
    first_line: u32,
    label: &str,
) -> String {
    format!("[{prefix}] {path}:{first_line} +{added} -{removed} | {label}")
}

/// write 失败消息：按 io 错误种类给针对性 hint（模型可直接执行，不猜测）。
fn write_error(path: &str, error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let hint = match error.kind() {
        ErrorKind::NotFound => {
            "The parent directory may not exist. Use exec with argv [\"ls\", \"-la\"] to inspect it, and create the directory first."
        }
        ErrorKind::PermissionDenied => {
            "The target is not writable (read-only attribute or missing permissions). Check with exec argv [\"ls\", \"-la\"], and remove the read-only flag if needed."
        }
        ErrorKind::IsADirectory => {
            "The target path is a directory, not a file. Use delete first, or write to a file path instead."
        }
        ErrorKind::StorageFull => "The disk is full. Free up space or choose another location.",
        _ => "Check disk space, file locks (another process may hold the file), and permissions.",
    };
    format!("[ERROR] Cannot write {path}: {error} [HINT] {hint}")
}

// ── Helpers from file_edit ──

// ── exec_write_file (from file_write.rs) ──

pub(super) fn exec_write_file(args: &serde_json::Value) -> ToolResult {
    let raw_path = args.s("path");
    let path = crate::resolve_workspace_path(&raw_path);
    let content = args.s("content");
    let append = args.opt_bool("append").unwrap_or(false);
    let dry_run = args.opt_bool("dry_run").unwrap_or(false);
    let expected_hash = args.s("expected_hash");
    if !dry_run
        && let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    let line_count = content.lines().count();

    // Read old content if file exists (for diff stats / dry-run preview)
    let old_content = std::fs::read_to_string(&path).ok();
    let normalized_old = old_content
        .as_deref()
        .map(normalize_newlines)
        .map(|(content, _)| content)
        .unwrap_or_default();
    if let Err(error) = verify_expected_hash(&path, &normalized_old, Some(&expected_hash)) {
        return ToolResult::error(error);
    }
    // 工具侧账本自动防漂移（模型无需回传 hash）：未显式带 expected_hash 时，
    // 用最近一次 read/edit/write 记录的指纹校验。失配 = 文件在工具外被修改，
    // 覆盖会丢掉外部改动 → 拒绝并提示重新 read（read 后账本自动刷新）。
    if expected_hash.is_empty()
        && let Some(known) = crate::file_state::last_hash(&path) {
            let disk_lf_hash = crate::file_shared::content_hash(&normalized_old);
            if known != disk_lf_hash {
                return ToolResult::error(
                    serde_json::json!({
                        "timeis": crate::now_utc8(), "status": "error", "code": "STALE_FILE", "path": path,
                        "message": "File was modified outside the tool since the last read/edit",
                        "expected_hash": known, "actual_hash": disk_lf_hash,
                        "hint": "Use read to refresh the tool's view of the file, then retry the write."
                    })
                    .to_string(),
                );
            }
        }

    // 统一在 LF 视图计算 diff（dry_run 文本预览 + 展示平面共用，不重复计算）。
    let (old_norm, _) = normalize_newlines(old_content.as_deref().unwrap_or(""));
    let preview = if append {
        format!("{normalized_old}{content}")
    } else {
        content.clone()
    };
    let (new_norm, _) = normalize_newlines(&preview);
    let diff = unified_diff(&old_norm, &new_norm, &path);
    let diff_text = (!diff.trim().is_empty()).then(|| diff.trim_end().to_string());

    // dry_run: 只预览 diff（与 edit 的 dry_run 语义一致），不写盘。
    // 通过 → 暂存参数供 confirm_apply 内存直提（模型无需重发 content）。
    if dry_run {
        let mut pending_args = args.clone();
        if let Some(obj) = pending_args.as_object_mut() {
            obj.remove("dry_run");
            // 注入 dry-run 时读到的磁盘 hash：重放时校验文件未被外部改动。
            obj.insert("expected_hash".into(), json!(content_hash(&normalized_old)));
        }
        let pending_id = crate::pending::store("write", &pending_args);
        let hint = format!(
            "\npending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}"
        );
        if diff.is_empty() {
            return ToolResult::ok_data(
                json!({"status": "ok", "dry_run": true, "pending_id": pending_id}),
                format!(
                    "[DRY RUN] {path} — {} bytes, {line_count} lines (no changes would be made){hint}",
                    content.len()
                ),
            );
        }
        let (added, removed, first_line) = diff_stats_between(&old_norm, &new_norm);
        return ToolResult::ok_data(
            json!({"status": "ok", "dry_run": true, "pending_id": pending_id}),
            format!(
                "{}\n\n{}{hint}",
                format_write_result("DRY RUN", &path, added, removed, first_line, "write"),
                diff.trim_end()
            ),
        );
    }

    if append {
        use std::io::Write;
        let mut file = match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                return ToolResult::error(write_error(&path, &e));
            }
        };
        match file.write_all(content.as_bytes()) {
            Ok(_) => {
                // 账本记录 append 后**整个文件**的指纹（磁盘上的真实状态）。
                let full = match &old_content {
                    Some(old) => format!("{old}{content}"),
                    None => content.clone(),
                };
                crate::file_state::record_write(&path, &full);
                crate::journal::record_change(
                    &crate::journal::active_session(),
                    "",
                    "write",
                    &raw_path,
                    "append",
                    old_content.as_deref(),
                    Some(&full),
                    "ok",
                );
                let text = if let Some(ref old) = old_content {
                    let old_line_count = old.lines().count();
                    let first_line = if old_line_count == 0 {
                        1u32
                    } else {
                        old_line_count as u32 + 1
                    };
                    // append 成功：只回摘要行，不回显追加内容（省上下文）。
                    format_write_result("OK", &path, line_count as u32, 0, first_line, "write")
                } else {
                    format!(
                        "[OK] {} — appended {} bytes, {} lines (new file)",
                        path,
                        content.len(),
                        line_count
                    )
                };
                let mut result = ToolResult::ok(text);
                if let Some(diff_text) = diff_text {
                    result = result.with_diff(diff_text);
                }
                result
            }
            Err(e) => ToolResult::error(write_error(&path, &e)),
        }
    } else {
        match atomic_write(&path, &content) {
            Ok(_) => {
                crate::file_state::record_write(&path, &content);
                crate::journal::record_change(
                    &crate::journal::active_session(),
                    "",
                    "write",
                    &raw_path,
                    "overwrite",
                    old_content.as_deref(),
                    Some(&content),
                    "ok",
                );
                let text = if let Some(ref old) = old_content {
                    // Overwrite：用 similar ops() 直接算统计（+N -M / 首个变更行），
                    // 正文默认不回传（diff 走展示平面，不占模型上下文）。
                    let (old_norm, _) = normalize_newlines(old);
                    let (new_norm, _) = normalize_newlines(&content);
                    if old_norm == new_norm {
                        format!(
                            "[OK] {} — {} bytes, {} lines (no changes)",
                            path,
                            content.len(),
                            line_count
                        )
                    } else {
                        let (added, removed, first_line) = diff_stats_between(&old_norm, &new_norm);
                        format_write_result("OK", &path, added, removed, first_line, "write")
                    }
                } else {
                    format!(
                        "[OK] {} — {} bytes, {} lines (new file)",
                        path,
                        content.len(),
                        line_count
                    )
                };
                let mut result = ToolResult::ok(text);
                if let Some(diff_text) = diff_text {
                    result = result.with_diff(diff_text);
                }
                result
            }
            Err(e) => ToolResult::error(write_error(&path, &e)),
        }
    }
}

handler!(handle_write_file, exec_write_file);

// ── exec_delete_file (from file_delete.rs) ──

fn trash_dir() -> std::path::PathBuf {
    let dir = crate::workspace::qaqh_dir().join("trash");
    let _ = std::fs::create_dir_all(&dir); // ensure exists
    dir
}

pub(super) fn exec_delete_file(args: &serde_json::Value) -> String {
    let raw_path = args.s("path");
    let path = crate::resolve_workspace_path(&raw_path);
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "NOT_FOUND",
            "path": path,
            "message": format!("{} does not exist", path),
            "hint": "Use exec with argv [\"ls\", \"-la\"] to verify."
        })
        .to_string();
    }

    // 工具侧账本防漂移：删除是破坏性操作——若文件在工具外被修改，
    // 模型基于过期认知删除会丢失外部改动 → 拒绝并提示重新 read。
    // 读失败（二进制/权限）则跳过校验（账本里也不会有对应记录）。
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let (lf, _) = normalize_newlines(&raw);
        if let Some(known) = crate::file_state::last_hash(&path) {
            let disk_lf_hash = crate::file_shared::content_hash(&lf);
            if known != disk_lf_hash {
                return serde_json::json!({
                    "timeis": crate::now_utc8(), "status": "error", "code": "STALE_FILE", "path": path,
                    "message": "File was modified outside the tool since the last read/edit",
                    "expected_hash": known, "actual_hash": disk_lf_hash,
                    "hint": "Use read to refresh the tool's view of the file, then retry the delete."
                }).to_string();
            }
        }
    }

    let trash_root = trash_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ws = crate::current_workspace();
    let project_root = if !ws.is_empty() && ws != "." {
        Path::new(&ws)
    } else {
        Path::new(".")
    };
    let rel = if let Ok(stripped) = p.strip_prefix(project_root) {
        stripped.to_string_lossy().to_string()
    } else if let Some(name) = p.file_name() {
        name.to_string_lossy().to_string()
    } else {
        path.replace(['/', '\\', ':'], "__")
    };
    let safe_name = rel.replace(['/', '\\', ':'], "__");
    let trash_path = trash_root.join(format!("{}.{}", safe_name, ts));

    if let Some(parent) = trash_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let journal_before = std::fs::read_to_string(&path).ok();

    match std::fs::rename(p, &trash_path) {
        Ok(_) => {
            crate::file_state::record_delete(&path);
            crate::journal::record_change(
                &crate::journal::active_session(),
                "",
                "delete",
                &raw_path,
                "delete",
                journal_before.as_deref(),
                None,
                "ok",
            );
            serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "ok",
            "path": path,
            "trash_path": format!(".deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
            "content": format!("Moved to trash: .deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
            "hint": format!("Restore with exec argv [\"mv\", \"{}\", \"{}\"]", trash_path.display(), path),
        }).to_string()
        }
        Err(_e) => {
            if p.is_dir() {
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "CROSS_DEVICE_DIR",
                    "path": path,
                    "message": "Cannot trash directory across devices",
                    "hint": format!("Use exec with argv [\"rm\", \"-rf\", \"{}\"] for cross-device deletion.", path),
                }).to_string()
            } else if let Err(e2) = std::fs::copy(p, &trash_path) {
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "COPY_FAILED",
                    "path": path,
                    "message": e2.to_string(),
                    "hint": "Check permissions and disk space."
                })
                .to_string()
            } else {
                match std::fs::remove_file(p) {
                    Ok(_) => {
                        crate::file_state::record_delete(&path);
                        crate::journal::record_change(
                            &crate::journal::active_session(),
                            "",
                            "delete",
                            &raw_path,
                            "delete",
                            journal_before.as_deref(),
                            None,
                            "ok",
                        );
                        serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "path": path,
                        "trash_path": format!(".deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
                        "content": format!("Moved to trash (cross-device): .deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
                        "hint": format!("Restore with exec argv [\"cp\", \"{}\", \"{}\"]", trash_path.display(), path),
                }).to_string()
                    }
                    Err(e2) => serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "path": path,
                        "warning": format!("Copied to trash but could not remove original: {}", e2),
                        "content": format!("Copied to trash, original still at {}", path),
                    })
                    .to_string(),
                }
            }
        }
    }
}

handler_from_string!(handle_delete_file, exec_delete_file);

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "write".to_string(),
        description: "Create, overwrite, or append to a file. Success returns a summary line only (path:first_line +N -M), no diff echo — set dry_run=true to preview the full diff without writing. Use for whole-file creation/overwrite/append; use edit for targeted changes.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"content":{"type":"string","description":"Content to write"},"append":{"type":"boolean","description":"If true, append to file instead of overwriting","default":false},"dry_run":{"type":"boolean","description":"Preview only (with full diff), do not write","default":false},"expected_hash":{"type":"string","description":"Optional. When omitted, the tool auto-verifies against its own last-known state (from read/edit/write) and rejects overwrites of externally-modified files — no need to pass the hash back"}},"required":["path","content"],"additionalProperties":false}),
        handler: handle_write_file,
        risk: ToolRisk::Write,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(30),
    },
    crate::ToolPlacement::Workspace,
);
    mgr.register_with_placement(ToolHandler {
        key: "delete".to_string(),
        description: "Move file to trash (.deepx/trash/) instead of permanent deletion.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path to delete"}},"required":["path"],"additionalProperties":false}),
        handler: handle_delete_file,
        risk: ToolRisk::Destructive,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(15),
    },
    crate::ToolPlacement::Workspace,
);
}
#[cfg(test)]
mod tests {
    use super::*;
    fn write(args: serde_json::Value) -> String {
        exec_write_file(&args).model_text().to_string()
    }
    #[test]
    fn overwrite_returns_summary_without_diff_body() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "line1\nline2\nline3\n").unwrap();
        let out = write(serde_json::json!({
            "path": p, "content": "line1\nCHANGED\nline3\n"
        }));
        // 摘要行：含路径与 +N -M 统计
        assert!(out.starts_with("[OK] "), "got: {out}");
        assert!(out.contains("+1 -1"), "got: {out}");
        assert!(out.contains("| write"), "got: {out}");
        // 默认不回传 diff 正文
        assert!(!out.contains("--- a/"), "diff body leaked: {out}");
        assert!(!out.contains("+++ b/"), "diff body leaked: {out}");
        assert!(!out.contains("CHANGED"), "content echo leaked: {out}");
        // 文件确实被写
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "line1\nCHANGED\nline3\n"
        );
    }
    #[test]
    fn dry_run_previews_diff_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.txt");
        std::fs::write(&p, "old\n").unwrap();
        let out = write(serde_json::json!({
            "path": p, "content": "new\n", "dry_run": true
        }));
        assert!(out.starts_with("[DRY RUN] "), "got: {out}");
        assert!(out.contains("--- a/"), "dry_run must include diff: {out}");
        assert!(out.contains("+++ b/"), "dry_run must include diff: {out}");
        // 未写盘
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old\n");
    }
    #[test]
    fn append_returns_summary_without_content_echo() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.txt");
        std::fs::write(&p, "a\nb\n").unwrap();
        let out = write(serde_json::json!({
            "path": p, "content": "appended-line\n", "append": true
        }));
        assert!(out.starts_with("[OK] "), "got: {out}");
        assert!(out.contains("+1 -0"), "got: {out}");
        // 不回显追加内容
        assert!(!out.contains("appended-line"), "content echo leaked: {out}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "a\nb\nappended-line\n"
        );
    }
    #[test]
    fn write_error_classifies_by_io_kind() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such dir");
        let out = write_error("x/y.txt", &err);
        assert!(
            out.starts_with("[ERROR] Cannot write x/y.txt"),
            "got: {out}"
        );
        assert!(out.contains("[HINT]"), "got: {out}");
        assert!(
            out.contains("parent directory"),
            "kind-specific hint missing: {out}"
        );
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let out = write_error("x/y.txt", &err);
        assert!(
            out.contains("read-only"),
            "kind-specific hint missing: {out}"
        );
    }
    #[test]
    fn write_to_directory_path_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub");
        std::fs::create_dir(&p).unwrap();
        let out = write(serde_json::json!({
            "path": p.to_string_lossy(), "content": "x"
        }));
        assert!(out.starts_with("[ERROR]"), "got: {out}");
        assert!(out.contains("[HINT]"), "got: {out}");
    }
}
