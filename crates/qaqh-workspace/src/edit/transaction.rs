//! transaction — split from file_edit_v2.rs

use crate::file_shared::{content_hash, unified_diff};
use crate::edit::CONTENT_CAP;
use crate::edit::hunk::Hunk;
use crate::edit::view::FileView;
use crate::edit::matching::{Candidate, Located};
use crate::edit::matching::*;
use crate::edit::resolve::{ResolvedOp};
use crate::edit::resolve::*;
use crate::edit::locate::*;

pub(crate) struct HunkReport {
    pub(crate) index: usize,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) tier: Option<u8>,
    pub(crate) score: Option<f32>,
    pub(crate) line_range: Option<(usize, usize)>,
    pub(crate) note: Option<String>,
    pub(crate) code: Option<String>,
    pub(crate) detail: Option<String>,
    pub(crate) candidates: Option<Vec<Candidate>>,
    /// hint_line 兜底命中时记录模型提示的 1-based 行号（透明回传）。
    pub(crate) used_hint: Option<usize>,
}

pub(crate) struct FileOutcome {
    pub(crate) edited: Option<String>,
    pub(crate) new_hash: Option<String>,
    pub(crate) diff: String,
    pub(crate) reports: Vec<HunkReport>,
    pub(crate) code: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) notes: Vec<String>,
    /// 账本行号修正：每 hunk 的 (编辑前 1-based 起始行, 行号增量)。
    /// 仅成功路径非空；错误/未落盘路径为空。
    pub(crate) shifts: Vec<(usize, i64)>,
}

/// 应用语义：strict = 全事务（任一 hunk 失败零改动）；partial = 成功的 hunk
/// 落盘、失败的报详情，模型用 new_hash 续接只重发失败项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Strict,
    Partial,
}

/// 核心纯逻辑：同一份未修改快照上定位全部 hunk → 重叠检测 → 倒序应用。
/// strict：任一 hunk 失败 → 整体拒绝（edited = None）；
/// partial：成功 hunk 应用（edited = Some + code = 首个失败码），报告含全部详情。
pub(crate) fn run_edit(content: &str, hunks: &[Hunk], notes: Vec<String>, mode: Mode) -> FileOutcome {
    // overwrite 是整文件独占语义：单个调用里只能有它自己。
    let overwrite_count = hunks
        .iter()
        .filter(|h| matches!(h, Hunk::Overwrite { .. }))
        .count();
    if overwrite_count > 1 || (overwrite_count == 1 && hunks.len() > 1) {
        return FileOutcome {
            edited: None,
            new_hash: None,
            diff: String::new(),
            reports: Vec::new(),
            code: Some("OVERWRITE_EXCLUSIVE".to_string()),
            message: Some(
                "overwrite replaces the WHOLE file and must be the only hunk in the call; use one overwrite alone, or prepend_file/append_file/replace hunks for incremental edits"
                    .to_string(),
            ),
            notes,
            shifts: Vec::new(),
        };
    }
    let view = FileView::new(content);
    let mut reports: Vec<HunkReport> = Vec::with_capacity(hunks.len());
    let mut located: Vec<Vec<Located>> = Vec::with_capacity(hunks.len());
    let mut first_code: Option<String> = None;

    for (i, hunk) in hunks.iter().enumerate() {
        // hint_line 消歧：多命中（AMBIGUOUS_MATCH）时按提示行号 ±10 开窗口
        // 过滤，窗口内唯一命中才应用（默认路径零变化：无 hint 原样报错）。
        let located_result = match locate_hunk(&view, hunk) {
            Ok(locs) => Ok(locs),
            Err(LocateError::Ambiguous { candidates, detail }) => {
                match locate_with_hint(&view, hunk) {
                    Some(Ok(loc)) => Ok(vec![loc]),
                    Some(Err(e)) => Err(e),
                    None => Err(LocateError::Ambiguous { candidates, detail }),
                }
            }
            Err(e) => Err(e),
        };
        match located_result {
            Ok(locs) => {
                // replace_all 一个 hunk 可产出多个位置；报告仍按 hunk 一条，
                // 位置/分数取首个，note 标注命中数量。
                let loc = &locs[0];
                let line_range = (loc.win_lines > 0)
                    .then(|| (loc.start_line + 1, loc.start_line + loc.win_lines));
                let note = if locs.len() > 1 {
                    format!("{} location(s) replaced", locs.len())
                } else {
                    loc.note.clone()
                };
                reports.push(HunkReport {
                    index: i,
                    kind: hunk.kind_name().to_string(),
                    status: "ok".to_string(),
                    tier: Some(loc.tier),
                    score: Some(loc.score),
                    line_range,
                    note: Some(note),
                    code: None,
                    detail: None,
                    candidates: None,
                    used_hint: loc.hint_line,
                });
                located.push(locs);
            }
            Err(e) => {
                let (code, detail, candidates) = match e {
                    LocateError::NoMatch { candidates, detail } => {
                        ("NO_MATCH".to_string(), detail, Some(candidates))
                    }
                    LocateError::Ambiguous { candidates, detail } => {
                        ("AMBIGUOUS_MATCH".to_string(), detail, Some(candidates))
                    }
                    LocateError::Underspecified => (
                        "UNDERSPECIFIED".to_string(),
                        "old/anchor and contexts are all empty; use insert_after / insert_before / prepend_file / append_file instead"
                            .to_string(),
                        None,
                    ),
                    LocateError::InvalidRegex(e) => (
                        "INVALID_REGEX".to_string(),
                        format!("replace_inline: invalid regex: {e}"),
                        None,
                    ),
                };
                if first_code.is_none() {
                    first_code = Some(code.clone());
                }
                reports.push(HunkReport {
                    index: i,
                    kind: hunk.kind_name().to_string(),
                    status: "error".to_string(),
                    tier: None,
                    score: None,
                    line_range: None,
                    note: None,
                    code: Some(code),
                    detail: Some(detail),
                    candidates,
                    used_hint: None,
                });
                located.push(Vec::new());
            }
        }
    }

    if let Some(code) = &first_code {
        let failed = reports.iter().filter(|r| r.status == "error").count();
        match mode {
            Mode::Strict => {
                let message = format!(
                    "{failed} of {} hunk(s) failed to locate; all-or-nothing: no changes written",
                    hunks.len()
                );
                return FileOutcome {
                    edited: None,
                    new_hash: None,
                    diff: String::new(),
                    reports,
                    code: Some(code.clone()),
                    message: Some(message),
                    notes,
                    shifts: Vec::new(),
                };
            }
            Mode::Partial => {
                // 只应用定位成功的 hunk（仍做重叠检测与单快照倒序应用）。
                let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
                for (i, (hunk, locs)) in hunks.iter().zip(located.iter()).enumerate() {
                    for loc in locs {
                        ops.push((i, resolve(&view, hunk, loc)));
                    }
                }
                if ops.is_empty() {
                    return FileOutcome {
                        edited: None,
                        new_hash: None,
                        diff: String::new(),
                        reports,
                        code: Some(code.clone()),
                        message: Some(format!("{failed}/{failed} hunk(s) failed; nothing applied")),
                        notes,
                        shifts: Vec::new(),
                    };
                }
                if let Err((a, b)) = check_overlap(&ops) {
                    return FileOutcome {
                        edited: None,
                        new_hash: None,
                        diff: String::new(),
                        reports,
                        code: Some("OVERLAPPING_HUNKS".to_string()),
                        message: Some(format!(
                            "successful hunk {a} and hunk {b} resolve to overlapping ranges; no changes written"
                        )),
                        notes,
                        shifts: Vec::new(),
                    };
                }
                let (edited, shifts) = apply_ops(content, &ops);
                let applied = ops.len();
                return FileOutcome {
                    diff: unified_diff(content, &edited, "file"),
                    new_hash: Some(content_hash(&edited)),
                    edited: Some(edited),
                    reports,
                    code: Some(code.clone()),
                    message: Some(format!(
                        "{applied} hunk(s) applied, {failed} failed — re-send ONLY the failed hunks with the returned new_hash as expected_hash"
                    )),
                    notes,
                    shifts,
                };
            }
        }
    }

    let mut ops: Vec<(usize, ResolvedOp)> = Vec::with_capacity(hunks.len());
    for (i, (hunk, locs)) in hunks.iter().zip(located.iter()).enumerate() {
        for loc in locs {
            ops.push((i, resolve(&view, hunk, loc)));
        }
    }
    if let Err((a, b)) = check_overlap(&ops) {
        return FileOutcome {
            edited: None,
            new_hash: None,
            diff: String::new(),
            reports,
            code: Some("OVERLAPPING_HUNKS".to_string()),
            message: Some(format!(
                "hunk {a} and hunk {b} resolve to overlapping ranges; no changes written"
            )),
            notes,
            shifts: Vec::new(),
        };
    }

    let (edited, shifts) = apply_ops(content, &ops);
    FileOutcome {
        diff: unified_diff(content, &edited, "file"),
        new_hash: Some(content_hash(&edited)),
        edited: Some(edited),
        reports,
        code: None,
        message: None,
        notes,
        shifts,
    }
}

// ─────────────────────────────────────────────────────────────
// 渲染
// ─────────────────────────────────────────────────────────────

pub(crate) fn render_text(path: &str, outcome: &FileOutcome) -> String {
    let mut out = String::new();
    match (&outcome.edited, &outcome.code) {
        (Some(_), None) => {
            // [OK] 全部成功
            let n = outcome.reports.len();
            let hash8 = outcome
                .new_hash
                .as_deref()
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("");
            out.push_str(&format!(
                "[OK] edit {path}\n  {n}/{n} hunks applied (new_hash {hash8})\n"
            ));
            for r in &outcome.reports {
                out.push_str(&format!("  {}\n", hunk_ok_line(r)));
            }
        }
        (Some(_), Some(code)) => {
            // [PARTIAL] 成功落盘 + 失败报告 + 续接指引
            let applied = outcome.reports.iter().filter(|r| r.status == "ok").count();
            let total = outcome.reports.len();
            let hash8 = outcome
                .new_hash
                .as_deref()
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("");
            out.push_str(&format!(
                "[PARTIAL] edit {path}\n  applied {applied}/{total} hunks (new_hash {hash8})\n"
            ));
            for r in &outcome.reports {
                if r.status == "ok" {
                    out.push_str(&format!("  {}\n", hunk_ok_line(r)));
                }
            }
            for r in &outcome.reports {
                if r.status != "ok" {
                    render_hunk_error(&mut out, r);
                }
            }
            out.push_str(&format!(
                "  next: re-send ONLY the failed hunks with \"expected_hash\": \"{}\" — already-applied hunks will conflict if repeated\n",
                outcome.new_hash.as_deref().unwrap_or_default()
            ));
            let _ = code;
        }
        (None, Some(code)) => {
            // [ERROR] 零改动 + 全部失败详情
            out.push_str(&format!(
                "[ERROR] edit {path}\n  no changes written ({code})\n"
            ));
            if let Some(m) = &outcome.message {
                out.push_str(&format!("  {m}\n"));
            }
            for r in &outcome.reports {
                if r.status == "ok" {
                    out.push_str(&format!(
                        "  hunk{} {}: ok (not applied — all-or-nothing)\n",
                        r.index, r.kind
                    ));
                } else {
                    render_hunk_error(&mut out, r);
                }
            }
        }
        (None, None) => {
            out.push_str(&format!("[ERROR] edit {path}\n  internal: no result\n"));
        }
    }
    for n in &outcome.notes {
        out.push_str(&format!("  note: {n}\n"));
    }
    out
}

pub(crate) fn hunk_ok_line(r: &HunkReport) -> String {
    let loc = match r.line_range {
        Some((a, b)) if a == b => format!("L{a}"),
        Some((a, b)) => format!("L{a}-L{b}"),
        None => "file".to_string(),
    };
    let tier = r.tier.map(|t| format!("tier{t}")).unwrap_or_default();
    let score = r
        .score
        .filter(|_| r.tier == Some(3))
        .map(|s| format!(" score {s:.2}"))
        .unwrap_or_default();
    let note = r.note.as_deref().unwrap_or("");
    format!("hunk{} {}: {loc} {tier}{score} {note}", r.index, r.kind)
}

/// 单个失败 hunk 的详细渲染：错误码 + 为什么失败 + 候选与 -/+ 对照 + 下一步。
pub(crate) fn render_hunk_error(out: &mut String, r: &HunkReport) {
    out.push_str(&format!(
        "  hunk{} {}: {} — {}\n",
        r.index,
        r.kind,
        r.code.as_deref().unwrap_or("error"),
        r.detail.as_deref().unwrap_or("")
    ));
    if let Some(cands) = &r.candidates {
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
    }
}

pub(crate) fn truncate_content(content: &str) -> String {
    if content.len() <= CONTENT_CAP {
        return content.to_string();
    }
    let cut = content.floor_char_boundary(CONTENT_CAP);
    format!(
        "{}…[truncated: {} bytes total]",
        &content[..cut],
        content.len()
    )
}

pub(crate) fn hint_for(code: &str) -> Option<&'static str> {
    match code {
        "NO_MATCH" => Some("Refine 'old'/context from the candidates, or re-locate with rg."),
        "AMBIGUOUS_MATCH" => {
            Some("Add context_before/context_after to disambiguate the repeated occurrence.")
        }
        "OVERLAPPING_HUNKS" => {
            Some("Split or merge hunks so their resolved ranges do not overlap.")
        }
        "UNDERSPECIFIED" => Some(
            "Provide context, or use insert_after / insert_before / prepend_file / append_file.",
        ),
        "OVERWRITE_EXCLUSIVE" => {
            Some("overwrite replaces the whole file — send it as the only hunk in the call.")
        }
        "HASH_MISMATCH" => Some(
            "Re-read the file with read and retry with the fresh hash (or omit expected_hash to edit without verification).",
        ),
        "INVALID_REGEX" => Some("replace_inline: fix the regex (regex crate syntax) and retry."),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────
// 读路径（空 hunks）：整文件 / 行号范围 / 锚定读
