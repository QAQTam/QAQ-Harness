//! Query tools: file read, diff.

use super::file_shared::{content_hash, is_binary_read_error};
use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};

// ------ exec_read (from file_read.rs) ------

pub(super) fn exec_read(args: &serde_json::Value) -> ToolResult {
    let requests = args
        .get("requests")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_else(|| vec![args.clone()]);
    if requests.is_empty() || requests.len() > 8 {
        return ToolResult::error_data(
            "INVALID_REQUEST_COUNT",
            "read accepts between 1 and 8 file requests",
            false,
            Some("Split the read into multiple calls.".into()),
            serde_json::json!({"max_requests": 8}),
        );
    }

    let mut outputs = Vec::with_capacity(requests.len());
    let mut metadata = Vec::with_capacity(requests.len());
    let mut total_chars = 0usize;
    for request in requests {
        let (result, meta) = read_one(&request);
        if !result.is_success() {
            return result;
        }
        let text = result.model_text().to_string();
        total_chars += text.chars().count();
        if total_chars > 48_000 {
            return ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "combined read result exceeds the 12k-token lap budget",
                false,
                Some("Read fewer files or split the requests.".into()),
                serde_json::json!({"max_tokens": 12_000}),
            );
        }
        outputs.push(text);
        metadata.push(meta);
    }
    let text = outputs.join("\n\n---\n\n");
    ToolResult::ok_data(serde_json::json!({"files": metadata}), text)
}

fn read_one(args: &serde_json::Value) -> (ToolResult, serde_json::Value) {
    const MAX_LINES: usize = crate::file_shared::READ_MAX_LINES;
    const MAX_MODEL_CHARS: usize = crate::file_shared::READ_MAX_CHARS;
    let path = crate::resolve_workspace_path(
        args.get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    );
    if path.is_empty() {
        return (
            ToolResult::error("read: path is required"),
            serde_json::json!({}),
        );
    }
    let workspace = crate::current_workspace();
    if let Some(skill) = qaqh_skills::managed_skill_for_path(
        std::path::Path::new(&workspace),
        std::path::Path::new(&path),
    ) {
        return (
            ToolResult::error_data(
                "USE_SKILLS_TOOL",
                format!("'{path}' is managed by skill '{skill}'"),
                false,
                Some("Use skills(action=activate|resource, name=...) instead.".into()),
                serde_json::json!({"path": path}),
            ),
            serde_json::json!({}),
        );
    }
    if std::path::Path::new(&path).is_dir() {
        return (
            ToolResult::error_data(
                "IS_DIRECTORY",
                format!("'{path}' is a directory"),
                false,
                Some("Use exec with argv [\"rg\", \"--files\"] (or [\"ls\", \"-la\"] / [\"cmd\", \"/c\", \"dir\", \"/b\"]) to list directory contents.".into()),
                serde_json::json!({"path": path}),
            ),
            serde_json::json!({}),
        );
    }

    let mut start = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .map(|v| if v == 0 { 1 } else { v });
    let mut end = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .map(|v| if v == 0 { 1 } else { v });
    if let (Some(start), Some(end)) = (start, end) {
        if end < start {
            return (
                ToolResult::error("end_line must be greater than or equal to start_line"),
                serde_json::json!({}),
            );
        }
        if end - start + 1 > MAX_LINES {
            return (
                ToolResult::error_data(
                    "RANGE_TOO_LARGE",
                    format!("requested range exceeds {MAX_LINES} lines"),
                    false,
                    Some("Use smaller contiguous ranges.".into()),
                    serde_json::json!({"max_lines": MAX_LINES}),
                ),
                serde_json::json!({}),
            );
        }
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if is_binary_read_error(&error.to_string()) => {
            return (
                ToolResult::error_data(
                    "BINARY_FILE",
                    format!("'{path}' is binary and cannot be read as text"),
                    false,
                    Some("Use exec for a binary-aware inspection.".into()),
                    serde_json::json!({"path": path}),
                ),
                serde_json::json!({}),
            );
        }
        Err(error) => {
            return (
                ToolResult::error_data(
                    "NOT_FOUND",
                    format!("cannot read '{path}': {error}"),
                    false,
                    Some("Verify the path, then retry read.".into()),
                    serde_json::json!({"path": path}),
                ),
                serde_json::json!({}),
            );
        }
    };
    let content = raw.replace("\r\n", "\n").replace('\r', "\n");
    let hash = content_hash(&content);
    if args.get("if_hash").and_then(|v| v.as_str()) == Some(hash.as_str()) {
        let meta = serde_json::json!({"path": path, "not_modified": true, "hash": hash});
        return (ToolResult::ok_data(meta.clone(), "not modified"), meta);
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();

    // ── 账本行号修正 ──────────────────────────────────────────────
    // 偏移链非空 = 最近一次 read 基线之后发生过账本 edit → 模型仍用旧行号
    // 盲定位时自动补偿（grep 输出清链、write 全覆盖清链、read 新基线清链，
    // 因此实时行号/全覆盖场景不会被误修正）。仅当首尾偏移一致（范围宽度
    // 不变）才修正；偏移不一致保持原样，由 expected_hash 兜底。修正信息经
    // meta 透明回传（corrected/original_lines/line_offset）。
    let mut corrected: Option<(usize, usize)> = None;
    let mut offset: Option<i64> = None;
    if let (Some(s), Some(e)) = (start, end)
        && let (Some((s2, ds)), Some((e2, de))) = (
            crate::file_state::correct_line(&path, s),
            crate::file_state::correct_line(&path, e),
        )
            && (s2 != s || e2 != e) && ds == de {
                corrected = Some((s, e));
                offset = Some(ds);
                start = Some(s2);
                end = Some(e2);
            }

    let explicit = start.is_some() || end.is_some();
    let first = start.unwrap_or(1).saturating_sub(1);
    // 宽容语义：仅 start 越界（行号概念失效）硬拒绝；end 越界截断到
    // total_lines（读少不读错，与全文件读的截断一致，truncated 会标注）。
    if first > total_lines {
        return (
            ToolResult::error_data(
                "LINE_OUT_OF_RANGE",
                format!("requested lines are outside '{path}' ({total_lines} total lines)"),
                false,
                Some("Use the total_lines value and retry.".into()),
                serde_json::json!({"path": path, "total_lines": total_lines, "hash": hash}),
            ),
            serde_json::json!({}),
        );
    }
    let requested_end = end.unwrap_or(total_lines).min(total_lines);
    let mut end_index = requested_end;
    if explicit && end_index.saturating_sub(first) > MAX_MODEL_CHARS / 40 {
        return (
            ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "requested range exceeds the model output budget",
                false,
                Some("Split the range into smaller contiguous reads.".into()),
                serde_json::json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
            ),
            serde_json::json!({}),
        );
    }
    if !explicit {
        let full_chars = lines
            .iter()
            .map(|line| line.chars().count() + 8)
            .sum::<usize>();
        if total_lines > MAX_LINES || full_chars > MAX_MODEL_CHARS {
            end_index = first;
            while end_index < total_lines {
                let next = end_index + 1;
                let chars = lines[first..next]
                    .iter()
                    .map(|line| line.chars().count() + 8)
                    .sum::<usize>();
                if chars > MAX_MODEL_CHARS {
                    break;
                }
                end_index = next;
            }
        }
    }
    let body = lines[first..end_index]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("L{}: {line}", first + offset + 1))
        .collect::<Vec<_>>()
        .join("\n");
    if explicit && body.chars().count() > MAX_MODEL_CHARS {
        return (
            ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "requested range exceeds the model output budget",
                false,
                Some("Split the range into smaller contiguous reads.".into()),
                serde_json::json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
            ),
            serde_json::json!({}),
        );
    }
    let truncated = end_index < total_lines;
    let mut meta = serde_json::json!({
        "path": path,
        "start_line": first + 1,
        "end_line": end_index,
        "total_lines": total_lines,
        "hash": hash,
        "truncated": truncated,
    });
    if truncated {
        let mut continuation = args.clone();
        continuation["start_line"] = serde_json::json!(end_index + 1);
        continuation["end_line"] = serde_json::Value::Null;
        meta["continuation"] = continuation;
    }
    if let (Some((os, oe)), Some(delta)) = (corrected, offset) {
        meta["corrected"] = serde_json::json!(true);
        meta["original_lines"] = serde_json::json!([os, oe]);
        meta["line_offset"] = serde_json::json!(delta);
    }
    // 任何 read（全文件或范围）都建立账本基线：模型后续用 start_line 盲定位时，
    // 工具凭账本自动防漂移，无需模型手动回传 hash。
    crate::file_state::record_read(&path, &content, total_lines);
    (ToolResult::ok_data(meta.clone(), body), meta)
}

handler!(handle_read, exec_read);

// ------ Registration ------

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "read".to_string(),
        description: "Read up to eight files as precise contiguous ranges. Every returned line has a stable L<number> prefix, and each file includes its hash, total line count, and a directly executable continuation when the model budget is insufficient. Directories are rejected with IS_DIRECTORY; list directory contents with exec (e.g. argv [\"rg\", \"--files\"]).",
        input_schema: serde_json::json!({
            "type":"object",
            "properties": {
                "requests": {
                    "type":"array", "maxItems":8,
                    "description":"Batch of up to 8 files/requests; each item mirrors the single-file fields below.",
                    "items": {"type":"object", "properties": {
                        "path":{"type":"string","description":"File path (relative to workspace root, or absolute)"},
                        "start_line":{"type":"integer","minimum":1,"description":"First line to read, 1-based, inclusive"},
                        "end_line":{"type":"integer","minimum":1,"description":"Last line to read, 1-based, inclusive; omit to read to EOF"},
                        "if_hash":{"type":"string","description":"Expected content hash from a prior read; returns NOT_MODIFIED when unchanged, guarding against silent drift"}
                    }, "required":["path"], "additionalProperties":false}
                },
                "path":{"type":"string","description":"File path (relative to workspace root, or absolute)"},
                "start_line":{"type":"integer","minimum":1,"description":"First line to read, 1-based, inclusive"},
                "end_line":{"type":"integer","minimum":1,"description":"Last line to read, 1-based, inclusive; omit to read to EOF"},
                "if_hash":{"type":"string","description":"Expected content hash from a prior read; returns NOT_MODIFIED when unchanged, guarding against silent drift"}
            },
            "oneOf":[{"required":["requests"]},{"required":["path"]}],
            "additionalProperties":false
        }),
        handler: handle_read,
        risk: ToolRisk::ReadOnly,
        category: crate::permission::ToolCategory::Read,
        default_timeout: std::time::Duration::from_secs(15),
    },
    crate::ToolPlacement::Workspace,
);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_directory_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "# hi\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let result = exec_read(&serde_json::json!({
            "path": dir.path().to_string_lossy(),
        }));

        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "IS_DIRECTORY");
    }

    #[test]
    fn read_range_is_contiguous_and_numbered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "one\ntwo\nthree\n").unwrap();

        let result = exec_read(&serde_json::json!({
            "path": dir.path().join("x.txt").to_string_lossy(),
            "start_line": 2,
            "end_line": 3,
        }));

        assert!(
            result.is_success(),
            "range read should succeed: {}",
            result.model_text()
        );
        assert_eq!(result.model_text(), "L2: two\nL3: three");
        assert_eq!(result.data["files"][0]["start_line"], 2);
        assert_eq!(result.data["files"][0]["end_line"], 3);
        // 防呆闭环：响应必须带 hash（LF 视图 content_hash），供 edit 的 expected_hash 校验
        let hash = result.data["files"][0]["hash"]
            .as_str()
            .expect("read must return hash");
        assert_eq!(hash, crate::file_shared::content_hash("one\ntwo\nthree\n"));
    }
}

#[test]
fn zero_line_is_tolerated_as_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\n").unwrap();

    // 0-based 混用（LSP 习惯）：0 视为 1，不再拒绝。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 0,
        "end_line": 1,
    }));
    assert!(
        result.is_success(),
        "0-based tolerance: {}",
        result.model_text()
    );
    assert_eq!(result.model_text(), "L1: one");
    assert_eq!(result.data["files"][0]["start_line"], 1);
}

#[test]
fn out_of_range_end_truncates_instead_of_rejecting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    // end 越界（行号漂移常见场景）：截断到文件尾 + truncated 标记，不拒绝。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 2,
        "end_line": 99,
    }));
    assert!(
        result.is_success(),
        "end truncation should succeed: {}",
        result.model_text()
    );
    assert_eq!(result.model_text(), "L2: two\nL3: three\nL4: four");
    let meta = &result.data["files"][0];
    assert_eq!(meta["end_line"], 4);
    assert_eq!(meta["total_lines"], 4);
    assert!(!meta["truncated"].as_bool().unwrap());
}

#[test]
fn out_of_range_start_still_rejects() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\n").unwrap();

    // start 越界 = 行号概念失效：仍硬拒绝（data 带 total_lines 可立即重试）。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 10,
    }));
    assert!(!result.is_success());
    assert_eq!(result.error.as_ref().unwrap().code, "LINE_OUT_OF_RANGE");
}

#[test]
fn ledger_corrects_stale_line_numbers_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("y.txt");
    std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
    let path = p.to_string_lossy().to_string();

    // 基线 read（清空偏移链）
    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    // 账本 edit：在 L2 处插入两行（模拟 edit 的 record_edit_with_shifts）
    let new_content = "one\nINS1\nINS2\ntwo\nthree\n";
    crate::file_state::record_edit_with_shifts(&path, new_content, &[(2, 2)]);
    std::fs::write(&p, new_content).unwrap();

    // 模型仍用旧行号 L3（= 现在的 L5 three）盲定位 → 自动修正 + 透明回传
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(
        r2.is_success(),
        "stale line correction: {}",
        r2.model_text()
    );
    assert_eq!(r2.model_text(), "L5: three");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["original_lines"], serde_json::json!([3, 3]));
    assert_eq!(meta["line_offset"], serde_json::json!(2));
}

#[test]
fn external_modification_is_not_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("z.txt");
    std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    // 外部直接改（不经账本）：偏移链为空 → 无法解释 → 不修正（hash 兜底）
    std::fs::write(&p, "one\nEXT\nthree\n").unwrap();
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(r2.is_success());
    let meta = &r2.data["files"][0];
    assert!(
        meta.get("corrected").is_none(),
        "external change must not be corrected"
    );
}

#[test]
fn write_clears_shift_chain() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.txt");
    std::fs::write(&p, "one\ntwo\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());
    crate::file_state::record_edit_with_shifts(&path, "one\ntwo\n", &[(1, 3)]);
    // write 全覆盖：行号全失效 → 清链
    crate::file_state::record_write(&path, "a\nb\nc\nd\n");
    std::fs::write(&p, "a\nb\nc\nd\n").unwrap();

    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(r2.is_success());
    assert_eq!(r2.model_text(), "L3: c");
    let meta = &r2.data["files"][0];
    assert!(
        meta.get("corrected").is_none(),
        "write semantics must clear the shift chain"
    );
}

#[test]
fn end_to_end_edit_then_stale_read_is_corrected() {
    // 真实工具链路：read（基线）→ edit（落账本偏移）→ 旧行号 read（自动修正）。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("e2e.txt");
    std::fs::write(&p, "a\nb\nc\nd\ne\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());
    let h1 = r1.data["files"][0]["hash"].as_str().unwrap().to_string();

    // 真实 edit 工具：L3 的 c 后插入两行（+2 偏移）
    let e = crate::file_edit_v2::exec_edit(&serde_json::json!({
        "path": path,
        "expected_hash": h1,
        "hunks": [{"kind": "insert_after", "anchor": "c", "new": "C1\nC2\n"}],
    }));
    assert!(e.is_success(), "edit: {}", e.model_text());

    // 模型仍用旧行号 L5（现在 = L7 e）→ 自动修正 + 透明回传
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 5,
        "end_line": 5,
    }));
    assert!(r2.is_success(), "stale read: {}", r2.model_text());
    assert_eq!(r2.model_text(), "L7: e");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["original_lines"], serde_json::json!([5, 5]));
    assert_eq!(meta["line_offset"], serde_json::json!(2));
}

#[test]
fn partial_edit_records_shifts_for_applied_hunks_only() {
    // partial 模式：成功的 hunk 偏移入账本（失败的 no-op），随后旧行号 read 修正。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("p.txt");
    std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
    let path = p.to_string_lossy().to_string();
    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    let e = crate::file_edit_v2::exec_edit(&serde_json::json!({
        "path": path,
        "mode": "partial",
        "hunks": [
            {"kind": "replace", "old": "b", "new": "B1\nB2"},
            {"kind": "replace", "old": "zzz", "new": "yyy"},
        ],
    }));
    assert!(e.is_success(), "partial: {}", e.model_text());
    assert_eq!(e.data["status"], "partial");

    // 旧 L4 d → 新 L5 d（仅成功 hunk 的偏移生效）
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 4,
        "end_line": 4,
    }));
    assert!(r2.is_success(), "{}", r2.model_text());
    assert_eq!(r2.model_text(), "L5: d");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["line_offset"], serde_json::json!(1));
}
