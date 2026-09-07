//! handler — split from the v2 edit core

use crate::edit::MAX_HUNKS;
use crate::edit::hunk::Hunk;
use crate::edit::transaction::*;
use crate::file_shared::{atomic_write, content_hash, normalize_newlines};
use crate::{ToolCallCtx, ToolHandler, ToolManager, ToolPlacement, ToolResult, ToolRisk};
use serde_json::{Value, json};

pub fn exec_edit(args: &serde_json::Value) -> ToolResult {
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fail = |code: &str, message: String, retryable: bool, hint: Option<&str>| {
        // 结构化 ToolError 已携带 code/message/hint；data 字段不再重复塞
        // 同样的信封（历史遗留，Phase 1 错误协议统一时移除）。
        ToolResult::error_data(
            code,
            message.clone(),
            retryable,
            hint.map(str::to_string),
            serde_json::json!({}),
        )
    };

    let raw_path = match args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(p) => p,
        None => {
            return fail("PARSE_ERROR", "edit: missing 'path'".into(), false, None);
        }
    };
    let path = crate::resolve_workspace_path(raw_path);
    let expected_hash = args
        .get("expected_hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut notes: Vec<String> = Vec::new();
    // read 模式已移除（行号系统归 read 工具，行号匹配同步完成）——hunks
    // 缺失/空一律 PARSE_ERROR（下方分支统一处理）。
    let hunks = match args.get("hunks").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            return fail(
                "PARSE_ERROR",
                "edit: missing non-empty 'hunks' array".into(),
                false,
                None,
            );
        }
    };
    if hunks.len() > MAX_HUNKS {
        return fail(
            "PARSE_ERROR",
            format!("edit: too many hunks ({} > {MAX_HUNKS})", hunks.len()),
            false,
            None,
        );
    }
    let mut parsed: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for (i, h) in hunks.iter().enumerate() {
        match Hunk::parse(h, &mut notes) {
            Ok(hunk) => parsed.push(hunk),
            Err(e) => {
                return fail("PARSE_ERROR", format!("edit: hunks[{i}]: {e}"), false, None);
            }
        }
    }

    // ── 读文件 ──
    let mut file_was_missing = false;
    let raw = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected_hash.is_none() => {
            // 创建新文件路径：空内容进入正常管线（仅 prepend/append/overwrite 可定位）。
            file_was_missing = true;
            Vec::new()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                "FILE_NOT_FOUND",
                format!(
                    "edit: {raw_path}: file not found (expected_hash provided but nothing to verify against)"
                ),
                false,
                None,
            );
        }
        Err(e) => {
            return fail(
                "READ_FAILED",
                format!("edit: cannot read {raw_path}: {e}"),
                false,
                None,
            );
        }
    };
    let raw = match String::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => {
            return fail(
                "NOT_UTF8_TEXT",
                format!("edit: {raw_path} is not valid UTF-8 text (treated as binary)"),
                false,
                None,
            );
        }
    };
    // LF 规范视图（与 read 的展示/hash 同视图）
    let (content, was_crlf) = normalize_newlines(&raw);

    // ── hash gate ──
    match &expected_hash {
        Some(h) => {
            let current = content_hash(&content);
            if &current != h {
                let cc = truncate_content(&content);
                let text = format!(
                    "[ERROR] edit {raw_path}\n  HASH_MISMATCH: content changed since the referenced read\n  current_hash: {current}\n  current content:\n{cc}\n"
                );
                let data = json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "HASH_MISMATCH",
                    "path": raw_path,
                    "message": "File content changed since the referenced read",
                    "expected_hash": h,
                    "current_hash": current,
                    "current_content": cc,
                });
                return ToolResult::error_data(
                    "HASH_MISMATCH",
                    text,
                    true,
                    hint_for("HASH_MISMATCH").map(str::to_string),
                    data,
                );
            }
        }
        None => {
            // 无 hash：直接编辑。v2 全部是内容定位（无行号盲定位），
            // 命中即安全——与 v1 的内容定位语义一致。模型从 read 拿不到
            // hash（hash 只在 data 元数据里，模型正文里没有），门禁只会堵死。
            // 文件不存在 → 创建路径（读文件分支已处理 NotFound）。
        }
    }

    // ── 核心执行 ──
    let outcome = run_edit(&content, raw_path, &parsed, notes);

    match outcome.edited {
        None => {
            let code = outcome.code.as_deref().unwrap_or("EDIT_REJECTED");
            let text = render_text(raw_path, &outcome);
            let data = json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "path": raw_path,
                "code": code,
                "message": outcome.message,
                "hunks": outcome.reports.iter().map(hunk_report_json).collect::<Vec<_>>(),
            });
            // 文件不存在 + 内容定位失败 → 提示创建路径（否则模型会反复重试 replace）。
            let hint = if file_was_missing && code == "NO_MATCH" {
                Some("file does not exist — create it with prepend_file / append_file (or overwrite) hunks".to_string())
            } else {
                hint_for(code).map(str::to_string)
            };
            ToolResult::error_data(code, text, true, hint, data)
        }
        Some(ref edited) => {
            if dry_run {
                // 只定位+计算，不写盘；暂存参数供 confirm_apply 内存直提
                // （模型无需重发 hunks）。expected_hash 注入 dry-run 读到的
                // LF 视图 hash：重放时 hash gate 拦截期间的外部改动。
                let new_hash = outcome.new_hash.as_deref().unwrap_or_default();
                let mut pending_args = args.clone();
                if let Some(obj) = pending_args.as_object_mut() {
                    obj.remove("dry_run");
                    obj.insert("expected_hash".into(), json!(content_hash(&content)));
                }
                let pending_id = crate::pending::store("edit", &pending_args);
                let text = render_text(raw_path, &outcome);
                let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
                let total = outcome.reports.len();
                let status = if outcome.code.is_some() {
                    "partial"
                } else {
                    "ok"
                };
                let mut data = json!({
                    "timeis": crate::now_utc8(),
                    "status": status,
                    "dry_run": true,
                    "path": raw_path,
                    "pending_id": pending_id,
                    "new_hash": new_hash,
                    "applied_hunks": applied,
                    "total_hunks": total,
                    "hunks": outcome.reports.iter().map(hunk_report_json).collect::<Vec<_>>(),
                });
                if let Some(code) = &outcome.code {
                    data["code"] = json!(code);
                }
                let mut result = ToolResult::ok_data(data, text);
                result.summary.push_str(&format!(
                    "pending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}\n"
                ));
                return result;
            }
            let write_content = if was_crlf {
                edited.replace('\n', "\r\n")
            } else {
                edited.clone()
            };
            if let Err(e) = atomic_write(&path, &write_content) {
                return fail(
                    "WRITE_FAILED",
                    format!(
                        "edit: atomic write failed for {raw_path}: {e} — the file on disk was NOT modified"
                    ),
                    false,
                    None,
                );
            }
            // 台账 + 行号偏移链：局部 hunk 编辑记录偏移（read 旧行号可自动修正）；
            // 全覆盖/零偏移路径退化为 write 语义（清偏移链 = 行号全部失效）。
            if outcome.shifts.is_empty() {
                crate::file_state::record_write(&path, &write_content);
            } else {
                crate::file_state::record_edit_with_shifts(&path, &write_content, &outcome.shifts);
            }
            crate::journal::record_change(
                &crate::journal::active_session(),
                "",
                "edit",
                raw_path,
                if outcome.shifts.is_empty() {
                    "overwrite"
                } else {
                    "replace"
                },
                if file_was_missing { None } else { Some(&raw) },
                Some(&write_content),
                "ok",
            );
            let new_hash = outcome.new_hash.as_deref().unwrap_or_default();
            let text = render_text(raw_path, &outcome);
            let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
            let total = outcome.reports.len();
            let status = if outcome.code.is_some() {
                "partial"
            } else {
                "ok"
            };
            let mut data = json!({
                "timeis": crate::now_utc8(),
                "status": status,
                "path": raw_path,
                "new_hash": new_hash,
                "applied_hunks": applied,
                "total_hunks": total,
                "notes": outcome.notes,
                "hunks": outcome.reports.iter().map(hunk_report_json).collect::<Vec<_>>(),
                // PR-E-1：patch 回显进模型投影——模型下一轮持内容基线
                //（unified diff，hunk 级天然小；fold 硬顶兜底超大场景）。
                // 修复"模型盲改"循环：上轮只回 hash 元数据，old 复述全靠记忆。
                "patch": outcome.diff,
            });
            if let Some(code) = &outcome.code {
                data["code"] = json!(code);
            }
            let mut result = ToolResult::ok_data(data, text);
            if !outcome.diff.is_empty() {
                result = result.with_diff(outcome.diff);
            }
            result
        }
    }
}

pub(crate) fn hunk_report_json(r: &HunkReport) -> Value {
    let mut v = json!({
        "index": r.index,
        "kind": r.kind,
        "status": r.status,
    });
    if let Some(t) = r.tier {
        v["tier"] = json!(t);
    }
    if let Some(s) = r.score {
        v["score"] = json!(s);
    }
    if let Some((a, b)) = r.line_range {
        v["line_range"] = json!([a, b]);
    }
    // hint_line 兜底命中：透明回传 提示行号 / 实际行号 / 偏差（实际 - 提示）。
    if let Some(h) = r.used_hint {
        v["used_hint"] = json!(h);
        if let Some((a, _)) = r.line_range {
            v["actual_line"] = json!(a);
            v["line_offset"] = json!(a as i64 - h as i64);
        }
    }
    if let Some(n) = &r.note {
        v["note"] = json!(n);
    }
    if let Some(c) = &r.code {
        v["code"] = json!(c);
    }
    if let Some(d) = &r.detail {
        v["detail"] = json!(d);
    }
    if let Some(cands) = &r.candidates {
        v["candidates"] = json!(
            cands
                .iter()
                .map(|c| json!({
                    "line_range": [c.line_range.0, c.line_range.1],
                    "snippet": c.snippet,
                    "score": c.score,
                    "tier": c.tier,
                    "diff": c.diff,
                }))
                .collect::<Vec<_>>()
        );
    }
    v
}

pub(crate) fn handle_edit(ctx: ToolCallCtx) -> ToolResult {
    exec_edit(&ctx.args)
}

pub fn register(mgr: &mut ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "edit".to_string(),
            description: "File editor (hunk-based, content-matched, supports replace_all). Kinds: replace(old/new), insert_after/insert_before(anchor/new), replace_inline(anchor/old/new), prepend/append_file(new). Use shortest unique old/anchor; supports expected_hash, dry_run+confirm_apply.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Target file"},
                    "expected_hash": {
                        "type": "string",
                        "description": "Hash from prior read; omit to skip verification"
                    },
                    "hunks": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Hunks: replace/insert_after/insert_before/replace_inline/prepend_file/append_file (at least one)"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "default": false,
                        "description": "Preview only, return pending_id for confirm_apply"
                    }
                },
                "required": ["path", "hunks"],
                "additionalProperties": false
            }),
            handler: handle_edit,
            risk: ToolRisk::Write,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(60),
        },
        ToolPlacement::Workspace,
    );
}
