//! read — split from file_edit_v2.rs

use crate::ToolResult;
use crate::edit::CONTENT_CAP;
use crate::edit::locate::locate_anchor;
use crate::edit::matching::Candidate;
use crate::edit::matching::LocateError;
use crate::edit::transaction::truncate_content;
use crate::edit::view::FileView;
use crate::edit::{READ_MAX_CHARS, READ_MAX_CONTEXT, READ_MAX_LINES};
use crate::file_shared::{content_hash, normalize_newlines};
use serde_json::json;

pub(crate) fn read_range(
    content: &str,
    total_lines: usize,
    raw_path: &str,
    hash: &str,
    start: Option<usize>,
    end: Option<usize>,
    fail: &dyn Fn(&str, String, bool, Option<&str>) -> ToolResult,
) -> ToolResult {
    let first = start.unwrap_or(1).saturating_sub(1);
    if first > total_lines || end.is_some_and(|e| e > total_lines) {
        return ToolResult::error_data(
            "LINE_OUT_OF_RANGE",
            format!("edit: requested lines are outside '{raw_path}' ({total_lines} total lines)"),
            false,
            Some(str::to_string("Use the total_lines value and retry.")),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "LINE_OUT_OF_RANGE",
                "path": raw_path,
                "message": format!("requested lines are outside '{raw_path}' ({total_lines} total lines)"),
                "total_lines": total_lines,
                "hash": hash,
            }),
        );
    }
    let end_0 = end.unwrap_or(total_lines).min(total_lines);
    let view = FileView::new(content);
    let body_chars: usize = view.lines[first..end_0]
        .iter()
        .map(|l| l.chars().count() + 8)
        .sum();
    if body_chars > READ_MAX_CHARS {
        return fail(
            "RANGE_TOO_LARGE",
            "edit: requested range exceeds the model output budget".into(),
            false,
            Some("Split the range into smaller contiguous reads."),
        );
    }
    let body = view.lines[first..end_0]
        .iter()
        .enumerate()
        .map(|(off, line)| format!("L{}: {line}", first + off + 1))
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::ok_data(
        json!({
            "status": "ok",
            "read_only": true,
            "path": raw_path,
            "start_line": first + 1,
            "end_line": end_0,
            "total_lines": total_lines,
            "hash": hash,
            "truncated": false,
        }),
        body,
    )
}

/// 锚定读：复用 hunk 的四层定位引擎——读时的 anchor 命中 = 改时的 hunk 定位，
/// 读到的上下文就是将要编辑的上下文。模糊/未命中返回候选（与 hunk 同款）。
pub(crate) fn read_anchored(
    content: &str,
    total_lines: usize,
    raw_path: &str,
    hash: &str,
    anchor: &str,
    ctx_before: usize,
    ctx_after: usize,
    fail: &dyn Fn(&str, String, bool, Option<&str>) -> ToolResult,
) -> ToolResult {
    let view = FileView::new(content);
    let loc = match locate_anchor(&view, anchor) {
        Ok(l) => l,
        Err(LocateError::Ambiguous { candidates, detail }) => {
            return ToolResult::error_data(
                "ANCHOR_AMBIGUOUS",
                render_read_candidates(raw_path, "ANCHOR_AMBIGUOUS", &detail, &candidates),
                true,
                Some(str::to_string(
                    "Add context_before/context_after to narrow the window, or read by line range (start_line/end_line).",
                )),
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "ANCHOR_AMBIGUOUS",
                    "path": raw_path,
                    "message": detail,
                    "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
                    "hash": hash,
                }),
            );
        }
        Err(LocateError::NoMatch { candidates, detail }) => {
            return ToolResult::error_data(
                "NO_MATCH",
                render_read_candidates(raw_path, "NO_MATCH", &detail, &candidates),
                true,
                Some(str::to_string(
                    "Refine the anchor from the candidates, or locate with grep first.",
                )),
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "NO_MATCH",
                    "path": raw_path,
                    "message": detail,
                    "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
                    "hash": hash,
                }),
            );
        }
        Err(LocateError::Underspecified) => {
            return fail(
                "UNDERSPECIFIED",
                "edit: anchor is empty (underspecified read)".into(),
                false,
                Some("Provide a non-empty anchor, or use start_line/end_line."),
            );
        }
        // locate_anchor 不编译正则，InvalidRegex 只可能来自 replace_inline 的 locate_hunk。
        Err(LocateError::InvalidRegex(_)) => unreachable!("locate_anchor never compiles a regex"),
    };
    let s = loc.start_line;
    let w = loc.win_lines.max(1);
    let before = ctx_before.min(s);
    let after = ctx_after.min(total_lines.saturating_sub(s + w));
    let first = s - before;
    let end_0 = (s + w + after).min(total_lines);
    let body_chars: usize = view.lines[first..end_0]
        .iter()
        .map(|l| l.chars().count() + 8)
        .sum();
    if body_chars > READ_MAX_CHARS {
        return fail(
            "RANGE_TOO_LARGE",
            "edit: anchored window exceeds the model output budget".into(),
            false,
            Some("Reduce context_before/context_after."),
        );
    }
    let body = view.lines[first..end_0]
        .iter()
        .enumerate()
        .map(|(off, line)| format!("L{}: {line}", first + off + 1))
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult::ok_data(
        json!({
            "status": "ok",
            "read_only": true,
            "path": raw_path,
            "anchor_line": s + 1,
            "anchor_lines": w,
            "start_line": first + 1,
            "end_line": end_0,
            "total_lines": total_lines,
            "tier": loc.tier,
            "score": loc.score,
            "note": loc.note,
            "hash": hash,
            "truncated": false,
        }),
        body,
    )
}

pub(crate) fn render_read_candidates(
    path: &str,
    code: &str,
    detail: &str,
    cands: &[Candidate],
) -> String {
    let mut out = format!("[ERROR] edit {path}\n  {code}: {detail}\n");
    for (i, c) in cands.iter().enumerate() {
        out.push_str(&format!(
            "    candidate #{} L{}-L{} score {:.2} tier{}\n",
            i + 1,
            c.line_range.0,
            c.line_range.1,
            c.score,
            c.tier
        ));
        if !c.diff.is_empty() {
            out.push_str(&c.diff);
            out.push('\n');
        }
    }
    out
}

pub(crate) fn candidate_json(c: &Candidate) -> serde_json::Value {
    json!({
        "line_range": [c.line_range.0, c.line_range.1],
        "snippet": c.snippet,
        "score": c.score,
        "tier": c.tier,
        "diff": c.diff,
    })
}

/// 读路径入口（hunks 缺失/空）：整文件 / start_line..end_line / anchor 三种形态。
pub(crate) fn read_path(path: &str, raw_path: &str, args: &serde_json::Value) -> ToolResult {
    let read_fail = |code: &str, message: String, retryable: bool, hint: Option<&str>| {
        ToolResult::error_data(
            code,
            message.clone(),
            retryable,
            hint.map(str::to_string),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": code,
                "message": message,
            }),
        )
    };
    let fail = &read_fail;

    let start = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let end = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let anchor = args
        .get("anchor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let has_context = args.get("context_before").is_some() || args.get("context_after").is_some();
    let ctx_before = args
        .get("context_before")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);
    let ctx_after = args
        .get("context_after")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(10);

    let has_range = start.is_some() || end.is_some();
    if has_range && (anchor.is_some() || has_context) {
        return fail(
            "PARSE_ERROR",
            "edit: start_line/end_line and anchor are mutually exclusive".into(),
            false,
            None,
        );
    }
    if anchor.is_none() && has_context {
        return fail(
            "PARSE_ERROR",
            "edit: context_before/context_after require 'anchor'".into(),
            false,
            None,
        );
    }
    if start == Some(0) || end == Some(0) {
        return fail(
            "PARSE_ERROR",
            "edit: read line numbers start at 1".into(),
            false,
            None,
        );
    }
    if let (Some(s), Some(e)) = (start, end) {
        if e < s {
            return fail(
                "PARSE_ERROR",
                "edit: end_line must be >= start_line".into(),
                false,
                None,
            );
        }
        if e - s + 1 > READ_MAX_LINES {
            return fail(
                "RANGE_TOO_LARGE",
                format!("edit: requested range exceeds {READ_MAX_LINES} lines"),
                false,
                Some("Use smaller contiguous ranges."),
            );
        }
    }
    if anchor.is_some() && (ctx_before > READ_MAX_CONTEXT || ctx_after > READ_MAX_CONTEXT) {
        return fail(
            "PARSE_ERROR",
            format!("edit: context window per side exceeds {READ_MAX_CONTEXT} lines"),
            false,
            None,
        );
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                "FILE_NOT_FOUND",
                format!(
                    "edit: {raw_path}: file not found (read mode; creation requires explicit hunks)"
                ),
                false,
                Some("Provide prepend_file / append_file / overwrite hunks to create the file"),
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
    let raw = match String::from_utf8(bytes) {
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
    let (content, _was_crlf) = normalize_newlines(&raw);
    let hash = content_hash(&content);
    let total_lines = content.lines().count();

    if !has_range && anchor.is_none() {
        // 整文件读（现状语义：CONTENT_CAP 截断，hash 仍为全文件）
        let truncated = content.len() > CONTENT_CAP;
        let shown = truncate_content(&content);
        let text = if truncated {
            format!("{shown}\n(read-only: {total_lines} lines, truncated)\n")
        } else {
            content
        };
        return ToolResult::ok_data(
            json!({
                "status": "ok",
                "read_only": true,
                "path": raw_path,
                "hash": hash,
                "line_count": total_lines,
                "truncated": truncated,
                // 注意：内容走 model_text（compact_data 会删 data 里的
                // content 键），data 只放元数据——与 read 同构。
            }),
            text,
        );
    }

    if has_range {
        return read_range(&content, total_lines, raw_path, &hash, start, end, fail);
    }
    // 前置分支已排除 anchor 为空且带 range 的路径；此处必为锚定读。
    let Some(anchor) = anchor else {
        return fail(
            "PARSE_ERROR",
            "edit: read requires start_line/end_line or anchor".into(),
            false,
            None,
        );
    };
    read_anchored(
        &content,
        total_lines,
        raw_path,
        &hash,
        anchor,
        ctx_before,
        ctx_after,
        fail,
    )
}
// ─────────────────────────────────────────────────────────────
// 执行入口
// ─────────────────────────────────────────────────────────────
