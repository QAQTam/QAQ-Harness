use super::*;
use serde_json::{Value, json};

fn rp(old: &str, new: &str) -> Hunk {
    Hunk::Replace {
        old: old.to_string(),
        new: new.to_string(),
        context_before: String::new(),
        context_after: String::new(),
        replace_all: false,
        hint_line: None,
    }
}

fn rp_all(old: &str, new: &str) -> Hunk {
    Hunk::Replace {
        old: old.to_string(),
        new: new.to_string(),
        context_before: String::new(),
        context_after: String::new(),
        replace_all: true,
        hint_line: None,
    }
}

fn rp_ctx(old: &str, new: &str, before: &str, after: &str) -> Hunk {
    Hunk::Replace {
        old: old.to_string(),
        new: new.to_string(),
        context_before: before.to_string(),
        context_after: after.to_string(),
        replace_all: false,
        hint_line: None,
    }
}

fn rp_hint(old: &str, new: &str, hint: usize) -> Hunk {
    Hunk::Replace {
        old: old.to_string(),
        new: new.to_string(),
        context_before: String::new(),
        context_after: String::new(),
        replace_all: false,
        hint_line: Some(hint),
    }
}

fn edit(content: &str, hunks: &[Hunk]) -> FileOutcome {
    run_edit(content, hunks, Vec::new(), Mode::Strict)
}

fn edit_partial(content: &str, hunks: &[Hunk]) -> FileOutcome {
    run_edit(content, hunks, Vec::new(), Mode::Partial)
}

fn err_code(o: &FileOutcome) -> &str {
    o.code.as_deref().unwrap_or("")
}

// 0. replace_all：全部精确匹配位置一次替换
#[test]
fn replace_all_substitutes_every_exact_occurrence() {
    let out = edit("a\nx\nb\nx\nc\n", &[rp_all("x", "X")]);
    assert_eq!(out.edited.as_deref(), Some("a\nX\nb\nX\nc\n"));
    assert_eq!(out.reports.len(), 1);
    assert_eq!(out.reports[0].tier, Some(1));
    assert_eq!(
        out.reports[0].note.as_deref(),
        Some("2 location(s) replaced")
    );
}

// 0b. replace_all：零精确命中 → NO_MATCH，不降级模糊匹配（多位置风险不可控）
#[test]
fn replace_all_without_exact_match_rejects() {
    // old 与文件内容有缩进差异 → Tier2 可单点命中，但 replace_all 拒绝降级。
    let out = edit("a\n    x\nb\n", &[rp_all("x", "X")]);
    assert_eq!(err_code(&out), "NO_MATCH");
    assert!(out.edited.is_none());
}

// 0c. replace_all：与其它 hunk 共存（区间互不重叠）
#[test]
fn replace_all_coexists_with_other_hunks() {
    let out = edit("a\nx\nb\nx\nc\n", &[rp_all("x", "X"), rp("b", "B")]);
    assert_eq!(out.edited.as_deref(), Some("a\nX\nB\nX\nc\n"));
}

// 1. 精确匹配唯一命中
#[test]
fn exact_match_unique_hit() {
    let out = edit("a\nb\nc\n", &[rp("b", "B")]);
    assert_eq!(out.edited.as_deref(), Some("a\nB\nc\n"));
    assert_eq!(out.reports[0].tier, Some(1));
    assert_eq!(out.reports[0].line_range, Some((2, 2)));
}

// 2. 缩进基准不同 → Tier2 形状命中
#[test]
fn indent_shape_matches_on_tier2() {
    let content = "fn outer() {\n    fn inner() {\n        let x = 1;\n    }\n}\n";
    // 模型忘了 inner 体里多缩进两格：old 不带缩进，文件行带 8 空格
    let out = edit(content, &[rp("let x = 1;", "let x = 2;")]);
    assert_eq!(
        out.edited.as_deref(),
        Some("fn outer() {\n    fn inner() {\n        let x = 2;\n    }\n}\n")
    );
    assert_eq!(out.reports[0].tier, Some(2));
    assert_eq!(out.reports[0].note.as_deref(), Some("indent-shape"));
}

// 3. 细微出入 → Tier3 达标自动采纳
#[test]
fn tier3_auto_applies_when_score_and_margin_pass() {
    // 文件里是 "let foo = 1;"，模型记为 "let fo0 = 1;"（一字之差）；
    // 且忘了缩进。剥缩进后 ratio 0.92 ≥ 0.85，唯一高分候选 → Tier3 采纳。
    let content = "fn main() {\n    let foo = 1;\n    println!(\"{}\", foo);\n}\n";
    let out = edit(content, &[rp("let fo0 = 1;", "let foo = 1;")]);
    assert_eq!(
        out.edited.as_deref(),
        Some("fn main() {\n    let foo = 1;\n    println!(\"{}\", foo);\n}\n")
    );
    assert_eq!(out.reports[0].tier, Some(3));
    assert!(out.reports[0].score.unwrap() >= T3_THRESHOLD);
}

// 4. 双候选 margin 不足 → 拒绝
#[test]
fn tier3_rejects_when_margin_insufficient() {
    let content = "let alpha_value = 1;\nlet alpho_value = 1;\n";
    let out = edit(content, &[rp("let alphu_value = 1;", "let x = 1;")]);
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "NO_MATCH");
    assert!(out.reports[0].candidates.as_ref().unwrap().len() >= 2);
}

// 5. 同一模式 ×3 + context_before 消歧
#[test]
fn context_before_disambiguates_repeated_pattern() {
    let content = "fn a() {\n    return Ok(());\n}\nfn b() {\n    return Ok(());\n}\nfn c() {\n    return Ok(());\n}\n";
    let out = edit(
        content,
        &[rp_ctx("return Ok(());", "return Err(());", "fn b() {", "")],
    );
    assert_eq!(
        out.edited.as_deref(),
        Some(
            "fn a() {\n    return Ok(());\n}\nfn b() {\n    return Err(());\n}\nfn c() {\n    return Ok(());\n}\n"
        )
    );
    // 行带缩进 → 精确不命中，Tier2 形状命中（3 处）→ context_before 消歧
    assert_eq!(out.reports[0].tier, Some(2));
}

// 6. 两 hunk 互不重叠，各自命中
#[test]
fn two_hunks_apply_independently() {
    let out = edit("a\nb\nc\nd\n", &[rp("a", "A"), rp("c", "C")]);
    assert_eq!(out.edited.as_deref(), Some("A\nb\nC\nd\n"));
    assert_eq!(out.reports.len(), 2);
}

// 7. 两 hunk 重叠 → 整体拒绝，零改动
#[test]
fn overlapping_hunks_rejected_atomically() {
    let content = "a\nb\nc\n";
    let before = content.to_string();
    // replace 窗口 [0,4)（"a\nb\nc" 不含尾换行），insert_before "c" 的
    // 插入点 = 行 2 行首 = char 4 ∈ [0,4) 边界…… 用更直白的严格内部冲突：
    // replace "a\nb\nc" 区间 [0,5)，insert_before "b" 的插入点 = 行 1 行首 = char 2 ∈ (0,5) 内部 → 冲突
    let out = edit(
        content,
        &[
            rp("a\nb\nc", "X"),
            Hunk::InsertBefore {
                anchor: "b".into(),
                new: "BB\n".into(),
                hint_line: None,
            },
        ],
    );
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "OVERLAPPING_HUNKS");
    assert_eq!(before, "a\nb\nc\n");
}

// 8. 空文件 PrependFile / AppendFile
#[test]
fn empty_file_prepend_and_append() {
    let out = edit(
        "",
        &[Hunk::PrependFile {
            new: "hello\n".into(),
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("hello\n"));
    let out = edit(
        "",
        &[Hunk::AppendFile {
            new: "world\n".into(),
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("world\n"));
}

// 9. 无结尾换行 AppendFile 沿用原约定（自动补换行）
#[test]
fn append_file_respects_missing_trailing_newline() {
    let out = edit("a\nb", &[Hunk::AppendFile { new: "c\n".into() }]);
    assert_eq!(out.edited.as_deref(), Some("a\nb\nc\n"));
}

// 10. CRLF 混入 → 归一化 + notes
#[test]
fn crlf_in_request_normalized_with_notes() {
    // 归一化在 Hunk::parse 层：CRLF → LF 并记 note，走真实解析路径。
    let mut notes = Vec::new();
    let hunk = Hunk::parse(
        &json!({"kind": "replace", "old": "a\r\nb", "new": "A\nB"}),
        &mut notes,
    )
    .unwrap();
    let out = run_edit("a\nb\n", &[hunk], notes, Mode::Strict);
    assert_eq!(out.edited.as_deref(), Some("A\nB\n"));
    assert_eq!(out.reports[0].status, "ok");
    assert!(out.notes.iter().any(|n| n.contains("CRLF")));
}

// 14. old 空 + context 全空 → Underspecified
#[test]
fn empty_old_without_context_is_underspecified() {
    let out = edit("a\nb\n", &[rp("", "X\n")]);
    assert_eq!(err_code(&out), "UNDERSPECIFIED");
    assert!(out.edited.is_none());
}

// 纯插入：context_before 定位
#[test]
fn pure_insert_with_context_before() {
    let out = edit("a\nb\nc\n", &[rp_ctx("", "X\n", "b", "")]);
    assert_eq!(out.edited.as_deref(), Some("a\nb\nX\nc\n"));
}

// 纯插入：双 context 交界
#[test]
fn pure_insert_between_contexts() {
    let out = edit("a\nb\nc\n", &[rp_ctx("", "X\n", "b", "c")]);
    assert_eq!(out.edited.as_deref(), Some("a\nb\nX\nc\n"));
}

// 15. insert_after / insert_before 锚点
#[test]
fn insert_after_and_before_anchors() {
    let out = edit(
        "a\nb\nc\n",
        &[Hunk::InsertAfter {
            anchor: "b".into(),
            new: "b2\n".into(),
            hint_line: None,
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("a\nb\nb2\nc\n"));
    let out = edit(
        "a\nb\nc\n",
        &[Hunk::InsertBefore {
            anchor: "c".into(),
            new: "c0\n".into(),
            hint_line: None,
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("a\nb\nc0\nc\n"));
}

// 15a. 同一位置的两次插入 → 重叠拒绝（顺序无歧义才允许）
#[test]
fn two_inserts_at_same_position_rejected() {
    let out = edit(
        "a\nb\nc\n",
        &[
            Hunk::InsertAfter {
                anchor: "b".into(),
                new: "b2\n".into(),
                hint_line: None,
            },
            Hunk::InsertBefore {
                anchor: "c".into(),
                new: "c0\n".into(),
                hint_line: None,
            },
        ],
    );
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "OVERLAPPING_HUNKS");
}

// 15b. 锚点多处命中 → Ambiguous
#[test]
fn ambiguous_anchor_rejected() {
    let out = edit(
        "a\nx\nb\nx\n",
        &[Hunk::InsertAfter {
            anchor: "x".into(),
            new: "y\n".into(),
            hint_line: None,
        }],
    );
    assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
    assert!(out.edited.is_none());
}

// 16. CRLF 文件写回保持 CRLF（execute 层测试）
#[test]
fn crlf_file_roundtrip_via_execute() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crlf.txt");
    std::fs::write(&path, "a\r\nb\r\n").unwrap();
    let hash = content_hash("a\nb\n"); // LF 视图 hash
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": hash,
        "hunks": [{"kind": "replace", "old": "b", "new": "c"}],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nc\r\n");
}

// 11. expected_hash 失配 → 拒绝 + current_content
#[test]
fn hash_mismatch_rejected_with_current_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": "deadbeef",
        "hunks": [{"kind": "replace", "old": "a", "new": "A"}],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "HASH_MISMATCH");
    assert!(
        result.data["current_content"]
            .as_str()
            .unwrap()
            .contains("a\nb\n")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
}

// 12. 无 hash 编辑已有文件 → 直接编辑（内容定位命中即安全）
#[test]
fn edit_existing_file_without_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "x\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
}

// 12b. 创建新文件成功
#[test]
fn create_new_file_with_prepend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "prepend_file", "new": "hello\n"}],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
}

// 13. 非 UTF-8 → NotUtf8Text
#[test]
fn binary_file_rejected_as_not_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bin.dat");
    std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": "whatever",
        "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "NOT_UTF8_TEXT");
}

// 17. new_hash 续接
#[test]
fn new_hash_chains_into_next_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\n").unwrap();
    let r1 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": content_hash("a\nb\n"),
        "hunks": [{"kind": "replace", "old": "a", "new": "A"}],
    }));
    assert!(r1.is_success(), "model text: {}", r1.model.text);
    let new_hash = r1.data["new_hash"].as_str().unwrap().to_string();
    assert!(!new_hash.is_empty());
    let r2 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": new_hash,
        "hunks": [{"kind": "replace", "old": "b", "new": "B"}],
    }));
    assert!(r2.is_success(), "model text: {}", r2.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nB\n");
}

// 18. 多 hunk 失败时报告全部失败详情
#[test]
fn all_hunk_failures_reported_at_once() {
    let out = edit("a\nb\nc\n", &[rp("a", "A"), rp("zzz", "Z"), rp("", "P")]);
    assert!(out.edited.is_none());
    assert_eq!(out.reports.len(), 3);
    assert_eq!(out.reports[0].status, "ok");
    assert_eq!(out.reports[1].status, "error");
    assert_eq!(out.reports[2].status, "error");
    assert_eq!(err_code(&out), "NO_MATCH");
}

// 19. 未知 kind → 拒绝
#[test]
fn unknown_hunk_kind_rejected() {
    let mut notes = Vec::new();
    let err = Hunk::parse(&json!({"kind": "magic", "new": "x"}), &mut notes).unwrap_err();
    assert!(err.contains("unknown hunk kind"));
}

// 20. 三 hunk 倒序应用（文件头/中/尾）
#[test]
fn three_hunks_apply_head_mid_tail() {
    let out = edit(
        "a\nb\nc\nd\ne\n",
        // old 无尾换行 → 区间不含尾换行 → new 对称地也不带
        &[rp("a", "A"), rp("c", "C"), rp("e", "E")],
    );
    assert_eq!(out.edited.as_deref(), Some("A\nb\nC\nd\nE\n"));
}

// 插入点与替换区间边界相邻（不冲突）→ 都应用
#[test]
fn insert_at_replace_boundary_is_allowed() {
    let out = edit(
        "a\nb\nc\n",
        &[
            rp("a", "A"),
            Hunk::InsertAfter {
                anchor: "a".into(),
                new: "a2\n".into(),
                hint_line: None,
            },
        ],
    );
    // replace "a"（区间 [0,1)，不含尾换行）与 insert_after "a"（插入点 char 2，
    // 在区间边界）不相交 → 都应用；倒序应用结果确定。
    assert_eq!(out.edited.as_deref(), Some("A\na2\nb\nc\n"));
}

// 21. partial 模式：成功 hunk 应用、失败 hunk 只报告
#[test]
fn partial_mode_applies_successful_hunks() {
    let out = edit_partial("a\nb\nc\n", &[rp("a", "A"), rp("zzz", "Z"), rp("c", "C")]);
    // 成功两处已应用，失败一处报错；code 为第一个失败码
    assert_eq!(out.edited.as_deref(), Some("A\nb\nC\n"));
    assert_eq!(out.reports.len(), 3);
    assert_eq!(out.reports[0].status, "ok");
    assert_eq!(out.reports[1].status, "error");
    assert_eq!(out.reports[2].status, "ok");
    assert_eq!(out.code.as_deref(), Some("NO_MATCH"));
    assert!(out.new_hash.is_some());
    // 渲染文本含续接指引
    let text = render_text("f.txt", &out);
    assert!(text.contains("[PARTIAL]"), "text: {text}");
    assert!(
        text.contains("re-send ONLY the failed hunks"),
        "text: {text}"
    );
}

// 22. partial 模式：全部失败 → 零改动（不写空结果）
#[test]
fn partial_mode_all_failed_writes_nothing() {
    let out = edit_partial("a\nb\nc\n", &[rp("zzz", "Z")]);
    assert!(out.edited.is_none());
    assert_eq!(out.code.as_deref(), Some("NO_MATCH"));
}

// 23. partial 模式：成功 hunk 之间重叠 → 仍拒绝
#[test]
fn partial_mode_overlap_among_successful_still_rejected() {
    let out = edit_partial(
        "a\nb\nc\n",
        &[
            rp("a\nb", "X"),
            Hunk::InsertBefore {
                anchor: "b".into(),
                new: "BB\n".into(),
                hint_line: None,
            },
            rp("zzz", "Z"), // 失败的 hunk 不参与重叠检测
        ],
    );
    assert!(out.edited.is_none());
    assert_eq!(out.code.as_deref(), Some("OVERLAPPING_HUNKS"));
}

// 24. NO_MATCH 诊断：候选带 -/+ 对照
#[test]
fn no_match_candidates_carry_pattern_diff() {
    let out = edit(
        "let alpha_value = 1;\nlet alpho_value = 1;\n",
        &[rp("let alphu_value = 1;", "let x = 1;")],
    );
    let cands = out.reports[0].candidates.as_ref().unwrap();
    assert!(!cands.is_empty());
    let diff = &cands[0].diff;
    assert!(diff.contains("- let alphu_value = 1;"), "diff: {diff}");
    assert!(
        diff.contains("+ let alpha_value = 1;") || diff.contains("+ let alpho_value = 1;"),
        "diff: {diff}"
    );
}

// 25. NO_MATCH 诊断：margin 不足时说明原因
#[test]
fn no_match_detail_explains_margin_shortfall() {
    let out = edit(
        "let alpha_value = 1;\nlet alpho_value = 1;\n",
        &[rp("let alphu_value = 1;", "let x = 1;")],
    );
    let detail = out.reports[0].detail.as_deref().unwrap_or("");
    assert!(detail.contains("margin"), "detail: {detail}");
    assert!(detail.contains("context_before"), "detail: {detail}");
}

// 26. NO_MATCH 诊断：完全不像时说明原因
#[test]
fn no_match_detail_explains_total_mismatch() {
    let out = edit("a\nb\nc\n", &[rp("zzzz", "x")]);
    let detail = out.reports[0].detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("no window had any similarity"),
        "detail: {detail}"
    );
}

// 27. partial 执行链：partial 落盘后 new_hash 续接只重发失败 hunk
#[test]
fn partial_then_resend_failed_hunk_via_execute() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let r1 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": content_hash("a\nb\nc\n"),
        "mode": "partial",
        "hunks": [
            {"kind": "replace", "old": "a", "new": "A"},
            {"kind": "replace", "old": "zzz", "new": "Z"},
            {"kind": "replace", "old": "c", "new": "C"}
        ],
    }));
    assert!(r1.is_success(), "model text: {}", r1.model.text);
    assert_eq!(r1.data["status"], "partial");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nC\n");
    let new_hash = r1.data["new_hash"].as_str().unwrap().to_string();
    // 只重发失败的 hunk
    let r2 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": new_hash,
        "hunks": [{"kind": "replace", "old": "zzz", "new": "Z"}],
    }));
    assert!(
        !r2.is_success(),
        "expected NO_MATCH, model text: {}",
        r2.model.text
    );
    // 文件未被第二次调用改动
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nC\n");
    // 修正后成功
    let r3 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": new_hash,
        "hunks": [{"kind": "insert_after", "anchor": "b", "new": "b2\n"}],
    }));
    assert!(r3.is_success(), "model text: {}", r3.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nb\nb2\nC\n");
}

// 28. 未知 mode → PARSE_ERROR
#[test]
fn unknown_mode_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "mode": "magic",
        "hunks": [{"kind": "replace", "old": "a", "new": "b"}],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "PARSE_ERROR");
}

// ── 多字节（中文）回归：FileView 字节/字符索引混淆曾导致 ropey 越界
//    panic 与区间错位（char_indices 返回字节偏移被误当 char 索引）。
// ────────────────────────────────────────────────────────────────

// 中文行 Tier1 精确替换：多字节行命中 + 区间正确（修复前会 panic/错位）。
#[test]
fn multibyte_exact_replace() {
    let content = "fn main() {\n    // 处理用户输入\n    let x = 1;\n}\n";
    let out = edit(content, &[rp("    // 处理用户输入", "    // 处理用户请求")]);
    assert_eq!(
        out.edited.as_deref(),
        Some("fn main() {\n    // 处理用户请求\n    let x = 1;\n}\n")
    );
    assert_eq!(out.reports[0].tier, Some(1));
}

// 中文文件多 hunk 倒序应用：两处中文替换 + 一处 ASCII 替换，倒序区间都正确。
#[test]
fn multibyte_multi_hunk_reverse_apply() {
    let content = "// 配置说明\nlet a = 1;\n// 环境变量\nlet b = 2;\n";
    let out = edit(
        content,
        &[
            rp("// 配置说明", "// 配置文档"),
            rp("let b = 2;", "let b = 3;"),
            rp("// 环境变量", "// 环境参数"),
        ],
    );
    assert_eq!(
        out.edited.as_deref(),
        Some("// 配置文档\nlet a = 1;\n// 环境参数\nlet b = 3;\n")
    );
    assert_eq!(err_code(&out), "");
}

// 中文锚点 InsertAfter：插入位置按 char 索引定位，中文行后插入正确。
// （语义：new 自带换行，与 ASCII 用例 15 一致。）
#[test]
fn multibyte_insert_after_anchor() {
    let content = "// 头部注释\nbody();\n";
    let out = edit(
        content,
        &[Hunk::InsertAfter {
            anchor: "// 头部注释".to_string(),
            new: "// 追加注释\n".to_string(),
            hint_line: None,
        }],
    );
    assert_eq!(
        out.edited.as_deref(),
        Some("// 头部注释\n// 追加注释\nbody();\n")
    );
}

// 中文文件无尾换行 AppendFile：自动补换行（new 给纯内容，不带前导 \n）。
#[test]
fn multibyte_append_no_trailing_newline() {
    let content = "// 中文内容";
    let out = edit(
        content,
        &[Hunk::AppendFile {
            new: "// 追加行".to_string(),
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("// 中文内容\n// 追加行"));
}

// 中文 + 尾换行语义：old 带 \n 的整段替换（删除尾行时区间含 \n）。
#[test]
fn multibyte_replace_with_trailing_newline() {
    let content = "第一行\n第二行\n第三行\n";
    let out = edit(content, &[rp("第二行\n", "第二行（改）\n")]);
    assert_eq!(
        out.edited.as_deref(),
        Some("第一行\n第二行（改）\n第三行\n")
    );
}

// ── overwrite（D10）──

// O1. 整文件覆盖：new 全文替换，Tier1 恒命中
#[test]
fn overwrite_replaces_whole_file() {
    let out = edit(
        "a\nb\nc\n",
        &[Hunk::Overwrite {
            new: "x\ny\n".into(),
        }],
    );
    assert_eq!(out.edited.as_deref(), Some("x\ny\n"));
    assert_eq!(out.reports[0].tier, Some(1));
    assert_eq!(out.reports[0].kind, "overwrite");
    assert_eq!(out.reports[0].status, "ok");
}

// O2. 空内容（创建路径）上 overwrite = 创建
#[test]
fn overwrite_on_empty_content_creates() {
    let out = edit("", &[Hunk::Overwrite { new: "x\n".into() }]);
    assert_eq!(out.edited.as_deref(), Some("x\n"));
}

// O3. overwrite 与其他 hunk 混用 → OVERWRITE_EXCLUSIVE（独占语义）
#[test]
fn overwrite_mixed_with_other_hunks_rejected() {
    let out = edit(
        "a\n",
        &[rp("a", "b"), Hunk::Overwrite { new: "x\n".into() }],
    );
    assert_eq!(err_code(&out), "OVERWRITE_EXCLUSIVE");
    assert!(out.edited.is_none());
    assert_eq!(out.reports.len(), 0);
}

// O4. 两个 overwrite → OVERWRITE_EXCLUSIVE
#[test]
fn two_overwrites_rejected() {
    let out = edit(
        "a\n",
        &[
            Hunk::Overwrite { new: "x\n".into() },
            Hunk::Overwrite { new: "y\n".into() },
        ],
    );
    assert_eq!(err_code(&out), "OVERWRITE_EXCLUSIVE");
    assert!(out.edited.is_none());
}

// O5. parse：overwrite 只要 new；误传的 old 被忽略（整文件语义不看旧内容）
#[test]
fn overwrite_parse_ignores_old() {
    let mut notes = Vec::new();
    let h = Hunk::parse(
        &json!({"kind": "overwrite", "new": "x\ny\n", "old": "irrelevant"}),
        &mut notes,
    );
    assert_eq!(
        h,
        Ok(Hunk::Overwrite {
            new: "x\ny\n".into()
        })
    );
    // 缺 new → 报错
    let h2 = Hunk::parse(&json!({"kind": "overwrite"}), &mut notes);
    assert!(h2.is_err());
    // unknown kind 消息包含 overwrite
    let h3 = Hunk::parse(&json!({"kind": "nope", "new": "x"}), &mut notes);
    assert!(
        h3.unwrap_err()
            .contains("expected replace / overwrite / insert_after")
    );
}

// O6. exec 层：overwrite 创建新文件
#[test]
fn overwrite_creates_new_file_via_exec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "overwrite", "new": "hello\nworld\n"}],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    // 台账已记录（创建）
    assert!(crate::file_state::last_hash(&path.to_string_lossy()).is_some());
}

// O7. exec 层：overwrite 覆盖已有文件
#[test]
fn overwrite_replaces_existing_file_via_exec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "old content\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "overwrite", "new": "brand new\n"}],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "brand new\n");
}

// O8. overwrite + expected_hash：错值拒绝（防覆盖竞争写），对值通过
#[test]
fn overwrite_with_expected_hash_guards_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\n").unwrap();
    let bad = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": "deadbeef",
        "hunks": [{"kind": "overwrite", "new": "x\n"}],
    }));
    assert!(!bad.is_success());
    assert_eq!(bad.data["code"], "HASH_MISMATCH");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");

    let good = content_hash("a\nb\n");
    let ok = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": good,
        "hunks": [{"kind": "overwrite", "new": "x\n"}],
    }));
    assert!(ok.is_success(), "model text: {}", ok.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
}

// O9. 不存在文件上误用 replace → NO_MATCH + 创建提示（hint 含 does not exist）
#[test]
fn replace_on_missing_file_hints_creation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghost.txt");
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "NO_MATCH");
    let hint = result
        .error
        .as_ref()
        .and_then(|e| e.hint.as_deref())
        .unwrap_or("");
    assert!(
        hint.contains("does not exist"),
        "hint should guide creation, got: {hint}"
    );
    assert!(!path.exists());
}

// O10. exec 层混用 → OVERWRITE_EXCLUSIVE 透传
#[test]
fn overwrite_mixed_rejected_via_exec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [
            {"kind": "replace", "old": "a", "new": "b"},
            {"kind": "overwrite", "new": "x\n"}
        ],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "OVERWRITE_EXCLUSIVE");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
}

// ── 读路径（空 hunks = 读，二元组场景）──

// R1. 空 hunks 读已有文件：read_only + content + hash + line_count，不写盘
#[test]
fn empty_hunks_reads_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(result.data["read_only"], true);
    // 内容在 model_text（对齐 read：data 只放元数据）
    assert!(result.model_text().contains("a\nb\nc\n"));
    assert_eq!(result.data["line_count"], 3);
    assert_eq!(result.data["hash"], content_hash("a\nb\nc\n"));
    assert_eq!(result.data["truncated"], false);
    // 不写盘（mtime 不变）
    let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after);
}

// R2. 省略 hunks 字段同样走读路径
#[test]
fn missing_hunks_reads_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "hello\n").unwrap();
    let result = exec_edit(&json!({ "path": path.to_string_lossy() }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(result.data["read_only"], true);
    assert_eq!(result.data["hash"], content_hash("hello\n"));
}

// R3. 空 hunks 读不存在的文件 → FILE_NOT_FOUND，且不创建
#[test]
fn empty_hunks_on_missing_file_reports_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghost.txt");
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "FILE_NOT_FOUND");
    assert!(!path.exists());
}

// R4. 读→编辑闭环：读的 hash 直接作 expected_hash 编辑成功
#[test]
fn read_hash_chains_into_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "x\n").unwrap();
    let read = exec_edit(&json!({ "path": path.to_string_lossy(), "hunks": [] }));
    assert!(read.is_success());
    let h = read.data["hash"].as_str().unwrap().to_string();
    let edit = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": h,
        "hunks": [{"kind": "replace", "old": "x", "new": "y"}],
    }));
    assert!(edit.is_success(), "model text: {}", edit.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
}

// ── 读路径扩展：行号范围读（grep 直连）──

// R5. 范围读：L 前缀行 + 元数据，hash 为全文件 hash
#[test]
fn range_read_returns_prefixed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 2,
        "end_line": 4,
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert!(result.model_text().contains("L2: b\nL3: c\nL4: d"));
    assert_eq!(result.data["start_line"], 2);
    assert_eq!(result.data["end_line"], 4);
    assert_eq!(result.data["total_lines"], 5);
    assert_eq!(result.data["hash"], content_hash("a\nb\nc\nd\ne\n"));
    assert_eq!(result.data["truncated"], false);
}

// R6. 只给 start_line：读到文件尾
#[test]
fn range_read_start_only_reads_to_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 4,
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert!(result.model_text().contains("L4: d\nL5: e"));
    assert_eq!(result.data["end_line"], 5);
}

// R7. 越界 → LINE_OUT_OF_RANGE（带 total_lines + hash）
#[test]
fn range_read_out_of_range_reports_total_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 6,
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "LINE_OUT_OF_RANGE");
    assert_eq!(result.data["total_lines"], 3);
    assert_eq!(result.data["hash"], content_hash("a\nb\nc\n"));
}

// R8. end < start → PARSE_ERROR
#[test]
fn range_read_inverted_bounds_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 4,
        "end_line": 2,
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "PARSE_ERROR");
}

// R9. 行号从 1 开始
#[test]
fn range_read_zero_line_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 0,
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "PARSE_ERROR");
}

// R10. 行数超上限 → RANGE_TOO_LARGE
#[test]
fn range_read_too_many_lines_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "x\n".repeat(450)).unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 1,
        "end_line": 450,
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "RANGE_TOO_LARGE");
}

// R11. 字符预算超限 → RANGE_TOO_LARGE
#[test]
fn range_read_exceeding_char_budget_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let line = "y".repeat(100);
    let content = (0..300)
        .map(|_| line.clone())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 1,
        "end_line": 300,
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "RANGE_TOO_LARGE");
}

// ── 读路径扩展：锚定读（复用定位引擎）──

// R12. 锚定读基本：唯一命中 → 窗口 + anchor_line + tier + 全文件 hash
#[test]
fn anchored_read_locates_unique_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(
        &path,
        "line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n",
    )
    .unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "fn foo(a: i32) -> i32 {",
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert!(result.model_text().contains("L2: fn foo(a: i32) -> i32 {"));
    assert_eq!(result.data["anchor_line"], 2);
    assert_eq!(result.data["anchor_lines"], 1);
    assert_eq!(result.data["tier"], 1);
    assert_eq!(result.data["start_line"], 1);
    assert_eq!(result.data["end_line"], 5);
    assert_eq!(result.data["total_lines"], 5);
    assert_eq!(
        result.data["hash"],
        content_hash("line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n")
    );
}

// R13. 锚定读上下文窗口：context_before/context_after 控制展示范围
#[test]
fn anchored_read_context_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(
        &path,
        "line one\nfn foo(a: i32) -> i32 {\n    a + 1\n}\nline five\n",
    )
    .unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "fn foo(a: i32) -> i32 {",
        "context_before": 1,
        "context_after": 1,
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(
        result.model_text(),
        "L1: line one\nL2: fn foo(a: i32) -> i32 {\nL3:     a + 1"
    );
    assert_eq!(result.data["start_line"], 1);
    assert_eq!(result.data["end_line"], 3);
}

// R14. 模糊锚 → ANCHOR_AMBIGUOUS + 候选（读阶段消歧，改阶段不再失败）
#[test]
fn anchored_read_ambiguous_returns_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "dup\ndup\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "dup",
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "ANCHOR_AMBIGUOUS");
    let cands = result.data["candidates"].as_array().unwrap();
    assert!(!cands.is_empty());
    assert!(
        result
            .error
            .as_ref()
            .and_then(|e| e.hint.as_deref())
            .is_some_and(|h| h.contains("context_before"))
    );
}

// R15. 未命中 → NO_MATCH + 候选
#[test]
fn anchored_read_no_match_returns_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "zzz_nope",
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "NO_MATCH");
}

// R16. 参数互斥与依赖校验
#[test]
fn read_mode_parameter_conflicts_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\n").unwrap();
    // start_line 与 anchor 互斥
    let r1 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 1,
        "anchor": "a",
    }));
    assert!(!r1.is_success());
    assert_eq!(r1.data["code"], "PARSE_ERROR");
    // context 必须配 anchor
    let r2 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "context_before": 3,
    }));
    assert!(!r2.is_success());
    assert_eq!(r2.data["code"], "PARSE_ERROR");
    // context 窗口超上限
    let r3 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "a",
        "context_after": 200,
    }));
    assert!(!r3.is_success());
    assert_eq!(r3.data["code"], "PARSE_ERROR");
}

// R17. 范围读的 hash 链进编辑（grep → 读 → 改 闭环）
#[test]
fn range_read_hash_chains_into_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let read = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "start_line": 2,
        "end_line": 2,
    }));
    assert!(read.is_success());
    let h = read.data["hash"].as_str().unwrap().to_string();
    let edit = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": h,
        "hunks": [{"kind": "replace", "old": "b", "new": "B"}],
    }));
    assert!(edit.is_success(), "model text: {}", edit.model.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB\nc\n");
}

// R18. 锚定读的 hash 链进编辑（锚定读 → 同锚编辑 闭环）
#[test]
fn anchored_read_hash_chains_into_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "pre\ntarget\npost\n").unwrap();
    let read = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "anchor": "target",
        "context_before": 1,
        "context_after": 1,
    }));
    assert!(read.is_success());
    let h = read.data["hash"].as_str().unwrap().to_string();
    let edit = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": h,
        "hunks": [{"kind": "insert_after", "anchor": "target", "new": "inserted\n"}],
    }));
    assert!(edit.is_success(), "model text: {}", edit.model.text);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "pre\ntarget\ninserted\npost\n"
    );
}

// ── replace_inline（sed `s///` 语义）──

// R19. 基本：anchor 窗口内子串替换第一处，窗口外不受影响
#[test]
fn replace_inline_replaces_first_occurrence_in_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(
        &path,
        "fn main() {\n    let x = input.clone();\n    input\n}\n",
    )
    .unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "let x = input.clone();",
            "old": "input",
            "new": "payload",
        }],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn main() {\n    let x = payload.clone();\n    input\n}\n"
    );
}

// R20. replace_all：窗口内全部替换（仍不跨窗口）
#[test]
fn replace_inline_replace_all_in_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(
        &path,
        "fn f() {\n    let a = alpha + alpha;\n    alpha\n}\n",
    )
    .unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "let a = alpha + alpha;",
            "old": "alpha",
            "new": "beta",
            "replace_all": true,
        }],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn f() {\n    let a = beta + beta;\n    alpha\n}\n"
    );
}

// R21. 超长行场景（backend_prompt.md 案例）：整行锚 + 行内子串替换
#[test]
fn replace_inline_on_long_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let long_line = format!("start {} end", "y".repeat(200));
    std::fs::write(&path, format!("a\n{long_line}\nb\n")).unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": long_line,
            "old": "yyy",
            "new": "ZZZ",
        }],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        content,
        format!("a\nstart {} end\nb\n", "ZZZ".to_string() + &"y".repeat(197))
    );
}

// R22. 窗口内无 old → NO_MATCH
#[test]
fn replace_inline_missing_in_window_reports_no_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar()\n}\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "fn foo() {",
            "old": "baz",
            "new": "qux",
        }],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "NO_MATCH");
    // 事务：文件未被修改
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn foo() {\n    bar()\n}\n"
    );
}

// R23. regex 替换 + 捕获组引用
#[test]
fn replace_inline_regex_with_captures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn f() {\n    let _a = 1;\n}\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "let _a = 1;",
            "old": r"let (\w+) = (\d+)",
            "new": "let $2 = $1",
            "regex": true,
        }],
    }));
    assert!(result.is_success(), "model text: {}", result.model.text);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn f() {\n    let 1 = _a;\n}\n"
    );
}

// R24. 非法正则 → INVALID_REGEX
#[test]
fn replace_inline_invalid_regex_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn f() {\n    x\n}\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "x",
            "old": "(",
            "new": "y",
            "regex": true,
        }],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "INVALID_REGEX");
}

// R25. 模糊锚 → AMBIGUOUS_MATCH（与 replace/insert 同款消歧）
#[test]
fn replace_inline_ambiguous_anchor_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "dup\ndup\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "dup",
            "old": "dup",
            "new": "DUP",
        }],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "AMBIGUOUS_MATCH");
}

// R26. 多 hunk 混用：replace_inline + replace 同批事务，失败整体拒绝
#[test]
fn replace_inline_mixed_with_replace_is_transactional() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn f() {\n    let a = alpha;\n    beta\n}\n").unwrap();
    let result = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [
            {
                "kind": "replace_inline",
                "anchor": "let a = alpha;",
                "old": "alpha",
                "new": "gamma",
            },
            {"kind": "replace", "old": "zzz_nope", "new": "x"},
        ],
    }));
    assert!(!result.is_success());
    assert_eq!(result.data["code"], "NO_MATCH");
    // 整体拒绝：零改动
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn f() {\n    let a = alpha;\n    beta\n}\n"
    );
}

// R27. replace_inline 走 hash 链：读 → 改，返回 new_hash
#[test]
fn replace_inline_hash_chains() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn f() {\n    let x = input;\n}\n").unwrap();
    let read = exec_edit(&json!({ "path": path.to_string_lossy() }));
    assert!(read.is_success());
    let h = read.data["hash"].as_str().unwrap().to_string();
    let edit = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": h,
        "hunks": [{
            "kind": "replace_inline",
            "anchor": "let x = input;",
            "old": "input",
            "new": "payload",
        }],
    }));
    assert!(edit.is_success(), "model text: {}", edit.model.text);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn f() {\n    let x = payload;\n}\n"
    );
    assert!(edit.data.get("new_hash").is_some());
}

#[test]
fn shifts_are_computed_for_line_level_edits() {
    let content = "a\nb\nc\nd\ne\n";
    // L2 的 b 换成两行 → (2, +1)；L4-5 的 d+e 行块换 e → (4, -1)
    let outcome = edit(content, &[rp("b", "b1\nb2"), rp("d\ne", "e")]);
    assert!(outcome.edited.is_some(), "strict edit should apply");
    let mut shifts = outcome.shifts.clone();
    shifts.sort(); // 应用序 = 倒序（位置大者先）；断言与顺序无关
    assert_eq!(shifts, vec![(2, 1), (4, -1)]);
}

#[test]
fn inline_replace_has_zero_shift() {
    let content = "aaa\nbbb\nccc\n";
    let outcome = edit(content, &[rp("bbb", "BBB")]);
    assert!(outcome.edited.is_some());
    assert_eq!(outcome.shifts, vec![(2, 0)]);
}

#[test]
fn failed_edit_has_empty_shifts() {
    let content = "aaa\nbbb\n";
    let outcome = edit(content, &[rp("zzz", "yyy")]);
    assert!(outcome.edited.is_none());
    assert!(outcome.shifts.is_empty());
}

// ── R30-R35：hint_line 宽松行号 ──────────────────────────────

#[test]
fn hint_line_disambiguates_repeated_content() {
    // x 出现两次（L3 与 L14），hint=14 的窗口 [4,24) 只含第二个 → 唯一命中。
    let content = "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n";
    let out = edit(content, &[rp_hint("x", "X", 14)]);
    assert!(out.edited.is_some(), "hint window should disambiguate");
    // 只替换 L14 的 x（L3 保持小写）。
    assert_eq!(
        out.edited.as_deref(),
        Some("a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nX\nm\n")
    );
    let r = &out.reports[0];
    assert_eq!(r.status, "ok");
    assert_eq!(r.tier, Some(4));
    assert_eq!(r.used_hint, Some(14));
    assert_eq!(r.line_range, Some((14, 14)));
}

#[test]
fn hint_line_window_still_ambiguous_keeps_error() {
    // 两个 x 都在窗口 [1,12) 内 → hint 无法消歧 → 保持 AMBIGUOUS_MATCH。
    let content = "x\nx\ny\n";
    let out = edit(content, &[rp_hint("x", "X", 2)]);
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
    assert_eq!(out.reports[0].used_hint, None);
}

#[test]
fn hint_line_miss_keeps_original_error() {
    // hint 偏太远（窗口内无 x）→ 保留原 Ambiguous（不误改）。
    // x 位于 L2 与 L25；hint=13 的窗口 [3,23) 不含任何 x。
    let content = "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\n";
    let out = edit(content, &[rp_hint("x", "X", 13)]);
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
}

#[test]
fn hint_line_disambiguates_anchor_insert() {
    // anchor "x" 两处，hint=14 窗口只含第二个 → insert_before 落到 L14。
    let content = "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n";
    let out = edit(
        content,
        &[Hunk::InsertBefore {
            anchor: "x".into(),
            new: "INS\n".into(),
            hint_line: Some(14),
        }],
    );
    assert!(out.edited.is_some(), "hint should disambiguate the anchor");
    assert_eq!(
        out.edited.as_deref(),
        Some("a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nINS\nx\nm\n")
    );
    assert_eq!(out.reports[0].used_hint, Some(14));
}

#[test]
fn no_hint_line_keeps_default_behavior() {
    // 无 hint_line：多命中 → 原样 AMBIGUOUS_MATCH（默认路径零变化）。
    let content = "a\nx\nb\nx\n";
    let out = edit(content, &[rp("x", "X")]);
    assert!(out.edited.is_none());
    assert_eq!(err_code(&out), "AMBIGUOUS_MATCH");
}

#[test]
fn hint_line_zero_is_treated_as_one() {
    // hint_line=0（LSP 习惯）→ 视为 1；窗口 [1,11) 含第一个 x（L2 的 0-based 1）。
    let content = "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n";
    let out = edit(content, &[rp_hint("x", "X", 0)]);
    assert!(out.edited.is_some());
    // 替换的是 0-based 1（L2）的 x。
    assert_eq!(
        out.edited.as_deref(),
        Some(content.replacen("x", "X", 1).as_str())
    );
    assert_eq!(out.reports[0].line_range, Some((2, 2)));
    assert_eq!(out.reports[0].used_hint, Some(1));
}
#[test]
fn hint_line_via_json_parses_and_reports() {
    // JSON 解析层："hint_line" 字段 → parse_hint_line → 窗口消歧 → JSON 回传。
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.txt");
    std::fs::write(&path, "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n").unwrap();
    let r = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "replace", "old": "x", "new": "X", "hint_line": 14}],
    }));
    assert!(r.is_success(), "model text: {}", r.model.text);
    // 只改 L14 的 x（L3 保持小写）。
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nX\nm\n"
    );
    let hr = &r.data["hunks"][0];
    assert_eq!(hr["used_hint"], 14);
    assert_eq!(hr["actual_line"], 14);
    assert_eq!(hr["line_offset"], 0);
}

#[test]
fn hint_line_zero_via_json_is_normalized_to_one() {
    // 两个 x 相距 >20 行（L2 与 L16），hint_line=0 → 归一化 1 → 窗口只含
    // 第一个 → 消歧成功，used_hint 回传归一化后的 1。
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h0.txt");
    std::fs::write(&path, "a\nx\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n").unwrap();
    let r = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "hunks": [{"kind": "replace", "old": "x", "new": "X", "hint_line": 0}],
    }));
    assert!(r.is_success(), "model text: {}", r.model.text);
    // 只改 L2 的 x（L16 保持小写）。
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\nX\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n"
    );
    assert_eq!(r.data["hunks"][0]["used_hint"], 1);
    assert_eq!(r.data["hunks"][0]["actual_line"], 2);
}
