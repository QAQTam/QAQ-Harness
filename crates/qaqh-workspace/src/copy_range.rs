//! `copy_range`：按**内容锚定范围**在文件间拷贝代码/文本。
//!
//! 解决"模型拷贝代码必须重新输出正文"的痛点：给出源范围的两行锚
//! （起始行 + 结束行）和目标插入锚，正文由引擎从文件读取原样搬运——
//! 模型只需输出几个锚字符串（几个 token），不需要重打正文。
//!
//! - **定位是内容匹配，不是行号**：锚 = 行级精确匹配（尾部空白容差）；
//!   多处命中 → 拒绝并附候选（拷贝错误会静默损坏目标，不降级模糊匹配）。
//! - 源范围 `[start_anchor, end_anchor]` 含两端行；`end_anchor` 缺省 = 单行。
//! - 目标插入：`insert_after`/`insert_before`（锚行）/ `append`（文件尾）/
//!   `prepend`（文件头）。
//! - LF 规范视图匹配（与 read 一致）；插入行使用目标文件的行尾风格
//!   （CRLF 文件插入 CRLF），未改动行保持原字节。
//! - 同文件拷贝：插入点落在被拷贝区间内 → 拒绝（区间随插入位移会乱）。

use crate::{ToolHandler, ToolResult, ToolRisk};

const MODES: &[&str] = &["insert_after", "insert_before", "append", "prepend"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    InsertAfter,
    InsertBefore,
    Append,
    Prepend,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s {
            "insert_after" => Some(Mode::InsertAfter),
            "insert_before" => Some(Mode::InsertBefore),
            "append" => Some(Mode::Append),
            "prepend" => Some(Mode::Prepend),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Mode::InsertAfter => "insert_after",
            Mode::InsertBefore => "insert_before",
            Mode::Append => "append",
            Mode::Prepend => "prepend",
        }
    }
}

struct RangeResult {
    copied: Vec<String>,
    /// 源区间（1-based 展示行号，含两端）
    range: (usize, usize),
    /// 目标插入位置（0-based 行索引）
    insert_at: usize,
    mode: Mode,
}

/// 行级精确匹配（尾部空白容差）。返回所有命中位置（0-based）。
fn locate_exact(lines: &[String], anchor: &str) -> Vec<usize> {
    let a = anchor.trim_end();
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_end() == a)
        .map(|(i, _)| i)
        .collect()
}

fn ambiguous_error(kind: &str, anchor: &str, hits: &[usize]) -> String {
    let locs: Vec<String> = hits
        .iter()
        .take(5)
        .map(|&i| format!("L{}", i + 1))
        .collect();
    serde_json::json!({
        "timeis": crate::now_utc8(),
        "status": "error",
        "code": kind,
        "message": format!("anchor {anchor:?} matches {} locations: {}", hits.len(), locs.join(", ")),
        "candidates": hits.iter().take(5).map(|&i| i + 1).collect::<Vec<usize>>(),
        "hint": "Make the anchor a longer/more unique line fragment, or add neighboring context in the anchor text (anchors match a WHOLE line)."
    })
    .to_string()
}

fn not_found_error(kind: &str, anchor: &str) -> String {
    serde_json::json!({
        "timeis": crate::now_utc8(),
        "status": "error",
        "code": kind,
        "message": format!("anchor {anchor:?} not found — check the exact line content (whitespace at line ends is tolerated; leading whitespace is significant)"),
    })
    .to_string()
}

/// 核心逻辑（纯函数，方便测试）：读源 → 定位区间 → 读目标 → 定位插入点 →
/// 生成新目标内容。`src`/`tgt` 为已解析的绝对路径。
fn run_copy_range(
    src: &std::path::Path,
    tgt: &std::path::Path,
    target_display: &str,
    start_anchor: &str,
    end_anchor: Option<&str>,
    target_anchor: Option<&str>,
    mode: Mode,
) -> Result<RangeResult, String> {
    let src_content = std::fs::read_to_string(src).map_err(|e| {
        serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "SOURCE_READ_ERROR",
            "message": format!("failed to read source {}: {e}", src.to_string_lossy()),
        })
        .to_string()
    })?;
    let (src_lf, _) = crate::file_shared::normalize_newlines(&src_content);
    let mut src_lines: Vec<String> = src_lf.split('\n').map(String::from).collect();
    if src_lines.last().is_some_and(|s| s.is_empty()) {
        src_lines.pop();
    }

    // ── 源区间定位 ──────────────────────────────────────────────
    let start_hits = locate_exact(&src_lines, start_anchor);
    if start_hits.is_empty() {
        return Err(not_found_error("SOURCE_START_NOT_FOUND", start_anchor));
    }
    if start_hits.len() > 1 {
        return Err(ambiguous_error(
            "SOURCE_START_AMBIGUOUS",
            start_anchor,
            &start_hits,
        ));
    }
    let start_idx = start_hits[0];

    let end_idx = match end_anchor {
        Some(end_anchor) => {
            // 结束锚：从起始行之后（含）顺序找——首个命中即结束行。
            let end_hits: Vec<usize> = src_lines
                .iter()
                .enumerate()
                .skip(start_idx)
                .filter(|(_, l)| l.trim_end() == end_anchor.trim_end())
                .map(|(i, _)| i)
                .collect();
            match end_hits.first() {
                Some(&i) => i,
                None => {
                    return Err(not_found_error("SOURCE_END_NOT_FOUND", end_anchor));
                }
            }
        }
        None => start_idx,
    };

    let copied: Vec<String> = src_lines[start_idx..=end_idx].to_vec();
    let range = (start_idx + 1, end_idx + 1);

    // ── 目标读取与插入点定位 ────────────────────────────────────
    let tgt_content = std::fs::read_to_string(tgt).map_err(|e| {
        serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "TARGET_READ_ERROR",
            "message": format!("failed to read target {}: {e}", tgt.to_string_lossy()),
        })
        .to_string()
    })?;
    let (tgt_lf, was_crlf) = crate::file_shared::normalize_newlines(&tgt_content);
    let mut tgt_lines: Vec<String> = tgt_lf.split('\n').map(String::from).collect();
    if tgt_lines.last().is_some_and(|s| s.is_empty()) {
        tgt_lines.pop();
    }

    let insert_at = match mode {
        Mode::Append => tgt_lines.len(),
        Mode::Prepend => 0,
        Mode::InsertAfter | Mode::InsertBefore => {
            let anchor = target_anchor.ok_or_else(|| {
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "MISSING_TARGET_ANCHOR",
                    "message": format!("mode={:?} requires 'target_anchor'", mode.name()),
                })
                .to_string()
            })?;
            let hits = locate_exact(&tgt_lines, anchor);
            if hits.is_empty() {
                return Err(not_found_error("TARGET_ANCHOR_NOT_FOUND", anchor));
            }
            if hits.len() > 1 {
                return Err(ambiguous_error("TARGET_ANCHOR_AMBIGUOUS", anchor, &hits));
            }
            match mode {
                Mode::InsertAfter => hits[0] + 1,
                _ => hits[0],
            }
        }
    };

    // ── 同文件区间冲突 ──────────────────────────────────────────
    let same_file = src.canonicalize().ok() == tgt.canonicalize().ok();
    if same_file && insert_at > start_idx && insert_at <= end_idx {
        // 插入点落在 [start..=end] 区间内 → 位移后区间漂移。
        // （insert_at == end_idx + 1，即区间正后方插入，是安全的。）
        return Err(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "INSERT_INSIDE_RANGE",
            "message": format!(
                "source and target are the same file and the insertion point (after line {}) lies inside the copied range L{}-L{} — copy would shift the range",
                insert_at, range.0, range.1
            ),
            "hint": "Insert before the range start, after the range end, or use a different target file.",
        })
        .to_string());
    }

    // ── 组装并写回 ──────────────────────────────────────────────
    let eol = if was_crlf { "\r\n" } else { "\n" };
    tgt_lines.splice(insert_at..insert_at, copied.iter().cloned());
    let mut out = tgt_lines.join(eol);
    // 历史行为：目标文件以换行结尾（无尾换行时补上；空文件不加）。
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    std::fs::write(tgt, &out).map_err(|e| {
        serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "TARGET_WRITE_ERROR",
            "message": format!("failed to write target {}: {e}", tgt.to_string_lossy()),
        })
        .to_string()
    })?;

    crate::journal::record_change(
        &crate::journal::active_session(),
        "",
        "copy_range",
        target_display,
        mode.name(),
        Some(&tgt_content),
        Some(&out),
        "ok",
    );

    Ok(RangeResult {
        copied,
        range,
        insert_at,
        mode,
    })
}

fn workspace_root() -> String {
    let ws = crate::current_workspace();
    if ws.is_empty() { ".".to_string() } else { ws }
}

fn exec_copy_range(args: &serde_json::Value) -> ToolResult {
    let get = |k: &str| args.get(k).and_then(|x| x.as_str()).map(str::to_string);

    let (Some(source_path), Some(source_start), Some(target_path)) =
        (get("source_path"), get("source_start"), get("target_path"))
    else {
        return crate::ToolResult::error(
            serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "MISSING_ARGUMENT",
                "message": "copy_range requires 'source_path', 'source_start' and 'target_path'",
            })
            .to_string(),
        );
    };
    let source_end = get("source_end");
    let target_anchor = get("target_anchor");
    let mode = match Mode::parse(
        args.get("mode")
            .and_then(|x| x.as_str())
            .unwrap_or("append"),
    ) {
        Some(m) => m,
        None => {
            return crate::ToolResult::error(
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "INVALID_MODE",
                    "message": format!("invalid mode — use one of: {}", MODES.join(" | ")),
                })
                .to_string(),
            );
        }
    };

    let ws = std::path::PathBuf::from(workspace_root());
    let src =
        crate::apply_patch_engine::resolve_workspace_path(&ws, std::path::Path::new(&source_path));
    let tgt =
        crate::apply_patch_engine::resolve_workspace_path(&ws, std::path::Path::new(&target_path));
    let (src, tgt) = match (src, tgt) {
        (Ok(s), Ok(t)) => (s, t),
        (Err(e), _) | (_, Err(e)) => {
            return crate::ToolResult::error(
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "PATH_OUTSIDE_WORKSPACE",
                    "message": e.to_string(),
                })
                .to_string(),
            );
        }
    };

    match run_copy_range(
        &src,
        &tgt,
        &target_path,
        &source_start,
        source_end.as_deref(),
        target_anchor.as_deref(),
        mode,
    ) {
        Ok(r) => {
            // 账本同步：目标已写盘，登记最新内容供 edit 防漂移。
            if let Ok(content) = std::fs::read_to_string(&tgt) {
                crate::file_state::record_write(&target_path, &content);
            }
            let n = r.copied.len();
            let mut text = format!(
                "[OK] copy_range — {n} line(s) L{}-L{} copied: {source_path} → {target_path} ({})\n",
                r.range.0,
                r.range.1,
                match r.mode {
                    Mode::InsertAfter => format!("after line {}", r.insert_at),
                    Mode::InsertBefore => format!("before line {}", r.insert_at + 1),
                    Mode::Append => "append".to_string(),
                    Mode::Prepend => "prepend".to_string(),
                }
            );
            if let Some(anchor) = &target_anchor {
                text = format!(
                    "[OK] copy_range — {n} line(s) L{}-L{} copied: {source_path} → {target_path} ({} {anchor:?})\n",
                    r.range.0,
                    r.range.1,
                    r.mode.name(),
                );
            }
            let data = serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "copied_lines": n,
                "source_range": [r.range.0, r.range.1],
                "source_path": source_path,
                "target_path": target_path,
                "mode": r.mode.name(),
            });
            crate::ToolResult::ok_data(data, text)
        }
        Err(msg) => crate::ToolResult::error(msg),
    }
}

fn handle_copy_range(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_copy_range(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "copy_range".to_string(),
        description: concat!(
            "Copy a CONTENT RANGE from one file to another (or within one file) WITHOUT re-typing the body. ",
            "The range is anchored by content, not line numbers: 'source_start' and optional 'source_end' are each a SHORT unique line fragment (the first line and last line of the range; omit source_end to copy a single line). ",
            "Insertion: mode insert_after/insert_before with 'target_anchor' (a unique line fragment in the target), or append (end of file, default) / prepend (start of file). ",
            "Matching is exact per whole line (trailing whitespace tolerated); ambiguous anchors are rejected with candidate line numbers — make anchors longer/more unique. ",
            "This is NOT clipboard copy and NOT whole-file copy (that is exec cp); it copies a selected region of file contents between workspace files. ",
            "The model only writes the anchors (a few tokens); the engine reads the region from the source file verbatim."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "source_path": {"type": "string", "description": "File to copy FROM (relative to workspace root)"},
                "source_start": {"type": "string", "description": "Anchor line: first line of the range to copy (exact whole-line match, trailing whitespace tolerated)"},
                "source_end": {"type": "string", "description": "Optional anchor line: last line of the range (inclusive); omit to copy a single line", "default": null},
                "target_path": {"type": "string", "description": "File to copy INTO (relative to workspace root)"},
                "target_anchor": {"type": "string", "description": "Required when mode is insert_after/insert_before: the line to insert after/before"},
                "mode": {"type": "string", "enum": ["insert_after", "insert_before", "append", "prepend"], "description": "Where to insert in the target; default append", "default": "append"}
            },
            "required": ["source_path", "source_start", "target_path"],
            "additionalProperties": false
        }),
        handler: handle_copy_range,
        risk: ToolRisk::Write,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(60),
    },
    crate::ToolPlacement::Workspace,
    );
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 写 CURRENT_WORKSPACE 的测试必须串行（全局静态，并行测试会互相踩踏）。
    static WS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn run(
        dir: &std::path::Path,
        src: &str,
        start: &str,
        end: Option<&str>,
        tgt: &str,
        anchor: Option<&str>,
        mode: &str,
    ) -> Result<String, String> {
        let _guard = WS_LOCK.lock().unwrap();
        let mut args = serde_json::json!({
            "source_path": src,
            "source_start": start,
            "target_path": tgt,
            "mode": mode,
        });
        if let Some(e) = end {
            args["source_end"] = serde_json::json!(e);
        }
        if let Some(a) = anchor {
            args["target_anchor"] = serde_json::json!(a);
        }
        // 注入 workspace（避免依赖全局 CURRENT_WORKSPACE）
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&dir.to_string_lossy().to_string());
        let result = exec_copy_range(&args);
        if result.is_success() {
            Ok(result.model_text().to_string())
        } else {
            Err(result.model_text().to_string())
        }
    }

    #[test]
    fn copies_range_to_append() {
        let (dir, ws) = setup(&[
            (
                "src.rs",
                "fn a() {}\nfn target() {\n    body\n}\nfn c() {}\n",
            ),
            ("dst.rs", "// header\n"),
        ]);
        let out = run(
            &ws,
            "src.rs",
            "fn target() {",
            Some("}"),
            "dst.rs",
            None,
            "append",
        )
        .unwrap();
        assert!(out.starts_with("[OK] copy_range"));
        assert!(out.contains("3 line(s)"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "// header\nfn target() {\n    body\n}\n"
        );
    }

    #[test]
    fn copies_single_line_without_source_end() {
        let (dir, ws) = setup(&[
            ("src.rs", "keep\nfn marker() {}\nkeep2\n"),
            ("dst.rs", "a\nb\n"),
        ]);
        run(
            &ws,
            "src.rs",
            "fn marker() {}",
            None,
            "dst.rs",
            None,
            "append",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "a\nb\nfn marker() {}\n"
        );
    }

    #[test]
    fn insert_after_anchor() {
        let (dir, ws) = setup(&[
            ("src.rs", "line1\nline2\nline3\n"),
            ("dst.rs", "head\nmid\ntail\n"),
        ]);
        run(
            &ws,
            "src.rs",
            "line2",
            None,
            "dst.rs",
            Some("mid"),
            "insert_after",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "head\nmid\nline2\ntail\n"
        );
    }

    #[test]
    fn insert_before_anchor() {
        let (dir, ws) = setup(&[("src.rs", "X1\nX2\n"), ("dst.rs", "a\nb\nc\n")]);
        run(
            &ws,
            "src.rs",
            "X1",
            Some("X2"),
            "dst.rs",
            Some("b"),
            "insert_before",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "a\nX1\nX2\nb\nc\n"
        );
    }

    #[test]
    fn prepend_to_file() {
        let (dir, ws) = setup(&[("src.rs", "HEADER\n"), ("dst.rs", "body\n")]);
        run(&ws, "src.rs", "HEADER", None, "dst.rs", None, "prepend").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "HEADER\nbody\n"
        );
    }

    #[test]
    fn ambiguous_source_start_rejected_with_candidates() {
        let (dir, ws) = setup(&[("src.rs", "dup\nx\ndup\ny\n"), ("dst.rs", "out\n")]);
        let err = run(&ws, "src.rs", "dup", None, "dst.rs", None, "append").unwrap_err();
        assert!(err.contains("SOURCE_START_AMBIGUOUS"), "got: {err}");
        assert!(err.contains("L1"), "candidates expected, got: {err}");
        assert!(err.contains("L3"), "candidates expected, got: {err}");
    }

    #[test]
    fn end_anchor_missing_rejected() {
        let (dir, ws) = setup(&[("src.rs", "start\nbody\n"), ("dst.rs", "out\n")]);
        let err = run(
            &ws,
            "src.rs",
            "start",
            Some("nope"),
            "dst.rs",
            None,
            "append",
        )
        .unwrap_err();
        assert!(err.contains("SOURCE_END_NOT_FOUND"), "got: {err}");
    }

    #[test]
    fn same_file_insert_inside_range_rejected() {
        let (dir, ws) = setup(&[("f.rs", "a\nb\nc\nd\n")]);
        let err = run(
            &ws,
            "f.rs",
            "b",
            Some("c"),
            "f.rs",
            Some("b"),
            "insert_after",
        )
        .unwrap_err();
        assert!(err.contains("INSERT_INSIDE_RANGE"), "got: {err}");
    }

    #[test]
    fn same_file_insert_after_range_allowed() {
        let (dir, ws) = setup(&[("f.rs", "a\nb\nc\nd\n")]);
        run(
            &ws,
            "f.rs",
            "b",
            Some("c"),
            "f.rs",
            Some("d"),
            "insert_before",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "a\nb\nc\nb\nc\nd\n"
        );
    }

    #[test]
    fn crlf_target_uses_crlf_for_inserted_lines() {
        let (dir, ws) = setup(&[("src.rs", "s1\ns2\n"), ("dst.rs", "h1\r\nh2\r\n")]);
        run(&ws, "src.rs", "s1", Some("s2"), "dst.rs", None, "append").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "h1\r\nh2\r\ns1\r\ns2\r\n"
        );
    }

    #[test]
    fn missing_anchor_when_mode_requires_it() {
        let (dir, ws) = setup(&[("src.rs", "s\n"), ("dst.rs", "t\n")]);
        let err = run(&ws, "src.rs", "s", None, "dst.rs", None, "insert_after").unwrap_err();
        assert!(err.contains("MISSING_TARGET_ANCHOR"), "got: {err}");
    }
}
