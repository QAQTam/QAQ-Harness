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
    run_edit(content, "test.rs", hunks, Vec::new())
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
        &[rp("a\nb\nc", "X"), rp("a\nb", "Y")], // 第二个区间严格内部 → 冲突
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
    let out = run_edit("a\nb\n", "test.rs", &[hunk], notes);
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

// 纯插入：双 context 交界

// 15. insert_after / insert_before 锚点

// 15a. 同一位置的两次插入 → 重叠拒绝（顺序无歧义才允许）

// 15b. 锚点多处命中 → Ambiguous

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
    assert!(result.is_success(), "model text: {}", result.model_text());
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
    assert!(result.is_success(), "model text: {}", result.model_text());
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
    assert!(result.is_success(), "model text: {}", result.model_text());
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
    assert_eq!(result.error.as_ref().unwrap().code, "NOT_UTF8_TEXT");
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
    assert!(r1.is_success(), "model text: {}", r1.model_text());
    let new_hash = r1.data["new_hash"].as_str().unwrap().to_string();
    assert!(!new_hash.is_empty());
    let r2 = exec_edit(&json!({
        "path": path.to_string_lossy(),
        "expected_hash": new_hash,
        "hunks": [{"kind": "replace", "old": "b", "new": "B"}],
    }));
    assert!(r2.is_success(), "model text: {}", r2.model_text());
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

// 21. partial 模式：成功 hunk 应用、失败 hunk 只报告

// 22. partial 模式：全部失败 → 零改动（不写空结果）

// 23. partial 模式：成功 hunk 之间重叠 → 仍拒绝

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

// 28. 未知 mode → PARSE_ERROR

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

// O1. 整文件覆盖：new 全文替换，Tier1 恒命中

// O2. 空内容（创建路径）上 overwrite = 创建

// O3. overwrite 与其他 hunk 混用 → OVERWRITE_EXCLUSIVE（独占语义）

// O4. 两个 overwrite → OVERWRITE_EXCLUSIVE

// O5. parse：overwrite 只要 new；误传的 old 被忽略（整文件语义不看旧内容）

// O6. exec 层：overwrite 创建新文件

// O7. exec 层：overwrite 覆盖已有文件

// O8. overwrite + expected_hash：错值拒绝（防覆盖竞争写），对值通过

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

// ── 读路径（空 hunks = 读，二元组场景）──

// R1. 空 hunks 读已有文件：read_only + content + hash + line_count，不写盘

// R2. 省略 hunks 字段同样走读路径

// R3. 空 hunks 读不存在的文件 → FILE_NOT_FOUND，且不创建

// R4. 读→编辑闭环：读的 hash 直接作 expected_hash 编辑成功

// ── 读路径扩展：行号范围读（grep 直连）──

// R5. 范围读：L 前缀行 + 元数据，hash 为全文件 hash

// R6. 只给 start_line：读到文件尾

// R7. 越界 → LINE_OUT_OF_RANGE（带 total_lines + hash）

// R8. end < start → PARSE_ERROR

// R9. 行号从 1 开始

// R10. 行数超上限 → RANGE_TOO_LARGE

// R11. 字符预算超限 → RANGE_TOO_LARGE

// ── 读路径扩展：锚定读（复用定位引擎）──

// R12. 锚定读基本：唯一命中 → 窗口 + anchor_line + tier + 全文件 hash

// R13. 锚定读上下文窗口：context_before/context_after 控制展示范围

// R14. 模糊锚 → ANCHOR_AMBIGUOUS + 候选（读阶段消歧，改阶段不再失败）

// R15. 未命中 → NO_MATCH + 候选

// R16. 参数互斥与依赖校验

// R17. 范围读的 hash 链进编辑（grep → 读 → 改 闭环）

// R18. 锚定读的 hash 链进编辑（锚定读 → 同锚编辑 闭环）

// ── replace_inline（sed `s///` 语义）──

// R19. 基本：anchor 窗口内子串替换第一处，窗口外不受影响

// R20. replace_all：窗口内全部替换（仍不跨窗口）

// R21. 超长行场景（backend_prompt.md 案例）：整行锚 + 行内子串替换

// R22. 窗口内无 old → NO_MATCH

// R23. regex 替换 + 捕获组引用

// R24. 非法正则 → INVALID_REGEX

// R25. 模糊锚 → AMBIGUOUS_MATCH（与 replace/insert 同款消歧）

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
    // "x" 两处，hint=14 窗口只含第二个 → replace 落到第二处。
    let content = "a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nx\nm\n";
    let out = edit(content, &[rp_hint("x", "INS", 14)]);
    assert!(out.edited.is_some(), "hint should disambiguate");
    assert_eq!(
        out.edited.as_deref(),
        Some("a\nb\nx\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nINS\nm\n")
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
    assert!(r.is_success(), "model text: {}", r.model_text());
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
    assert!(r.is_success(), "model text: {}", r.model_text());
    // 只改 L2 的 x（L16 保持小写）。
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\nX\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\nx\n"
    );
    assert_eq!(r.data["hunks"][0]["used_hint"], 1);
    assert_eq!(r.data["hunks"][0]["actual_line"], 2);
}
