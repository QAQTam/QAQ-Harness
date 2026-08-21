//! locate — split from file_edit_v2.rs

use serde_json::Value;
use crate::edit::HINT_WINDOW;
use crate::edit::hunk::Hunk;
use crate::edit::view::FileView;
use crate::edit::matching::{Located, LocateError};
use crate::edit::matching::*;

pub(crate) fn located(view: &FileView, s: usize, win: usize, tier: u8, score: f32, note: &str) -> Located {
    let range = view.char_range(s, win);
    Located {
        start_char: range.start,
        end_char: range.end,
        start_line: s,
        win_lines: win,
        tier,
        score,
        note: note.to_string(),
        hint_line: None,
    }
}

/// 锚点定位（insert_after / insert_before 的 anchor，以及纯插入的 context）。
/// 无 context 消歧可用——多处命中即 Ambiguous。
pub(crate) fn locate_anchor(view: &FileView, anchor: &str) -> Result<Located, LocateError> {
    let pat = pattern_lines(anchor);
    if pat.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let cands = tier1(view, &pat);
    if cands.len() == 1 {
        return Ok(located(view, cands[0], pat.len(), 1, 1.0, "exact"));
    }
    if cands.len() > 1 {
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
            detail: format!("anchor matches {} locations", cands.len()),
        });
    }
    let cands = tier2(view, &pat);
    if cands.len() == 1 {
        return Ok(located(view, cands[0], pat.len(), 2, 1.0, "indent-shape"));
    }
    if cands.len() > 1 {
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 2, 1.0, &pat),
            detail: format!("anchor matches {} locations (indent-shape)", cands.len()),
        });
    }
    let probe = tier3_probe(view, &pat, &[], &[]);
    if let Some((s, w, score)) = probe.hit {
        return Ok(located(
            view,
            s,
            w,
            3,
            score,
            &format!("similarity {score:.2}"),
        ));
    }
    Err(LocateError::NoMatch {
        candidates: tier3_candidates(view, &pat, &[], &[]),
        detail: no_match_detail(&probe),
    })
}

/// 纯插入（old 为空）：context_before / context_after 各自定位，取交界处。
pub(crate) fn locate_pure_insert(view: &FileView, before: &str, after: &str) -> Result<Located, LocateError> {
    let b = pattern_lines(before);
    let a = pattern_lines(after);
    if b.is_empty() && a.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let b_loc = if b.is_empty() {
        None
    } else {
        Some(locate_anchor(view, before)?)
    };
    let a_loc = if a.is_empty() {
        None
    } else {
        Some(locate_anchor(view, after)?)
    };
    let (pos, tier, score, note) = match (&b_loc, &a_loc) {
        (Some(bl), Some(al)) => {
            if bl.end_char > al.start_char {
                return Err(LocateError::Ambiguous {
                    candidates: Vec::new(),
                    detail: "context_before and context_after regions overlap or are inverted"
                        .to_string(),
                });
            }
            (
                bl.end_char,
                bl.tier.max(al.tier),
                bl.score.min(al.score),
                "pure insert (both contexts)".to_string(),
            )
        }
        (Some(bl), None) => (
            bl.end_char,
            bl.tier,
            bl.score,
            "pure insert (context_before)".to_string(),
        ),
        (None, Some(al)) => (
            al.start_char,
            al.tier,
            al.score,
            "pure insert (context_after)".to_string(),
        ),
        (None, None) => unreachable!("both contexts empty is checked above"),
    };
    Ok(Located {
        start_char: pos,
        end_char: pos,
        start_line: 0,
        win_lines: 0,
        tier,
        score,
        note,
        hint_line: None,
    })
}

/// 解析可选的宽松行号提示（hint_line，1-based；0 视为 1）。
#[allow(dead_code)]
pub(crate) fn parse_hint_line(v: &Value) -> Option<usize> {
    v.get("hint_line")
        .and_then(|x| x.as_u64())
        .map(|x| x.max(1) as usize)
}

/// replace hunk 的定位：四层流水线。
pub(crate) fn locate_replace(
    view: &FileView,
    old: &str,
    before: &str,
    after: &str,
) -> Result<Located, LocateError> {
    let pat = pattern_lines(old);
    if pat.is_empty() {
        return locate_pure_insert(view, before, after);
    }
    let b = pattern_lines(before);
    let a = pattern_lines(after);

    // Tier1 精确
    let cands = tier1(view, &pat);
    if !cands.is_empty() {
        if cands.len() == 1 {
            return Ok(located(view, cands[0], pat.len(), 1, 1.0, "exact"));
        }
        let filtered = context_filter(view, &cands, pat.len(), &b, &a);
        if filtered.len() == 1 {
            return Ok(located(
                view,
                filtered[0],
                pat.len(),
                1,
                1.0,
                "exact (context)",
            ));
        }
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
            detail: format!(
                "old matches {} locations; context_before/context_after did not disambiguate — pick one by its surrounding lines (see candidates) and add context_before/context_after",
                cands.len()
            ),
        });
    }

    // Tier2 缩进形状
    let cands = tier2(view, &pat);
    if !cands.is_empty() {
        if cands.len() == 1 {
            return Ok(located(view, cands[0], pat.len(), 2, 1.0, "indent-shape"));
        }
        let filtered = context_filter(view, &cands, pat.len(), &b, &a);
        if filtered.len() == 1 {
            return Ok(located(
                view,
                filtered[0],
                pat.len(),
                2,
                1.0,
                "indent-shape (context)",
            ));
        }
        return Err(LocateError::Ambiguous {
            candidates: make_candidates(view, &cands, pat.len(), 2, 1.0, &pat),
            detail: format!(
                "old matches {} locations (indent-shape); context did not disambiguate",
                cands.len()
            ),
        });
    }

    // Tier3 相似度评分
    let probe = tier3_probe(view, &pat, &b, &a);
    if let Some((s, w, score)) = probe.hit {
        return Ok(located(
            view,
            s,
            w,
            3,
            score,
            &format!("similarity {score:.2}"),
        ));
    }

    // Tier4 拒绝 + 候选
    Err(LocateError::NoMatch {
        candidates: tier3_candidates(view, &pat, &b, &a),
        detail: no_match_detail(&probe),
    })
}

/// replace_all：Tier1 精确匹配的**全部**位置（一个 hunk → 多个操作）。
/// 不降级 Tier2/3——模糊匹配应用于多个位置风险不可控。零命中 → NoMatch
/// （附 Tier3 候选供参考）。
pub(crate) fn locate_replace_all(view: &FileView, old: &str) -> Result<Vec<Located>, LocateError> {
    let pat = pattern_lines(old);
    if pat.is_empty() {
        return Err(LocateError::Underspecified);
    }
    let cands = tier1(view, &pat);
    if cands.is_empty() {
        return Err(LocateError::NoMatch {
            candidates: tier3_candidates(view, &pat, &[], &[]),
            detail: "replace_all: no exact match found — re-check 'old' against the file (see candidates below)"
                .into(),
        });
    }
    Ok(cands
        .into_iter()
        .map(|s| located(view, s, pat.len(), 1, 1.0, "exact (replace_all)"))
        .collect())
}

pub(crate) fn locate_hunk(view: &FileView, hunk: &Hunk) -> Result<Vec<Located>, LocateError> {
    match hunk {
        Hunk::Replace {
            old,
            replace_all: true,
            ..
        } => locate_replace_all(view, old),
        Hunk::Replace {
            old,
            context_before,
            context_after,
            ..
        } => locate_replace(view, old, context_before, context_after).map(|l| vec![l]),
        // 整文件区间；空文件（创建路径）自动退化为零长度区间。
        Hunk::Overwrite { .. } => Ok(vec![Located {
            start_char: 0,
            end_char: view.content.len(),
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "overwrite".to_string(),
            hint_line: None,
        }]),
        Hunk::InsertAfter { anchor, .. } | Hunk::InsertBefore { anchor, .. } => {
            locate_anchor(view, anchor).map(|l| vec![l])
        }
        Hunk::PrependFile { .. } => Ok(vec![Located {
            start_char: 0,
            end_char: 0,
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "prepend".to_string(),
            hint_line: None,
        }]),
        Hunk::AppendFile { .. } => Ok(vec![Located {
            start_char: view.content.len(),
            end_char: view.content.len(),
            start_line: 0,
            win_lines: 0,
            tier: 1,
            score: 1.0,
            note: "append".to_string(),
            hint_line: None,
        }]),
        Hunk::ReplaceInline {
            anchor,
            old,
            replace_all,
            regex,
            ..
        } => {
            let loc = locate_anchor(view, anchor)?;
            // 窗口内验证可命中：replace_inline 的 NO_MATCH 语义限定在 anchor 窗口内。
            let win_lines = loc.win_lines.max(1);
            let win = &view.lines[loc.start_line..loc.start_line + win_lines];
            let hit = if *regex {
                match regex::Regex::new(old) {
                    Ok(re) => win.iter().any(|l| re.is_match(l)),
                    Err(e) => return Err(LocateError::InvalidRegex(e.to_string())),
                }
            } else {
                win.iter().any(|l| l.contains(old.as_str()))
            };
            if !hit {
                let _ = replace_all; // 命中计数在 resolve 里标注
                return Err(LocateError::NoMatch {
                    candidates: Vec::new(),
                    detail: format!(
                        "replace_inline: 'old' not found within the anchor window (L{}-L{}); the anchor located the window but the substring/regex does not occur in it",
                        loc.start_line + 1,
                        loc.start_line + win_lines
                    ),
                });
            }
            Ok(vec![loc])
        }
    }
}

/// Tier1 精确匹配限定在 [lo, hi)（0-based 行窗口）内。
pub(crate) fn tier1_in_window(view: &FileView, pat: &[&str], lo: usize, hi: usize) -> Vec<usize> {
    tier1(view, pat)
        .into_iter()
        .filter(|&s| s >= lo && s < hi)
        .collect()
}

/// hint_line 兜底定位（宽松行号）：多命中（Ambiguous）时按行号窗口消歧。
///
/// 语义：行号 = 提示/窗口（±10 行），内容 = 唯一通行证——窗口内 Tier1 精确
/// 匹配，**唯一命中才应用**（0 个 → None，调用方保留原 Ambiguous；多个 →
/// 新 Ambiguous 附窗口信息）。不触碰默认路径：hint_line 为 None 返回 None。
pub(crate) fn locate_with_hint(view: &FileView, hunk: &Hunk) -> Option<Result<Located, LocateError>> {
    let hint = match hunk {
        Hunk::Replace {
            hint_line: Some(h),
            replace_all: false,
            ..
        }
        | Hunk::InsertAfter {
            hint_line: Some(h), ..
        }
        | Hunk::InsertBefore {
            hint_line: Some(h), ..
        }
        | Hunk::ReplaceInline {
            hint_line: Some(h), ..
        } => (*h).max(1), // 0 视为 1（与 parse 层一致）
        _ => return None,
    };
    let total = view.lines.len();
    // 1-based hint → 0-based 窗口 [hint-11, hint+10)，钳制到文件范围。
    let lo = hint.saturating_sub(HINT_WINDOW + 1).min(total);
    let hi = (hint + HINT_WINDOW).min(total);
    if lo >= hi {
        return None; // 窗口为空 → hint 无意义，保留原错误
    }

    match hunk {
        Hunk::Replace { old, .. } => {
            let pat = pattern_lines(old);
            if pat.is_empty() {
                return None; // 纯插入走 locate_pure_insert，不做 hint 兜底
            }
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: 'old' still matches {} locations within window L{}-L{}; add context_before/context_after or a more specific hint",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        Hunk::InsertAfter { anchor, .. } | Hunk::InsertBefore { anchor, .. } => {
            let pat = pattern_lines(anchor);
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: anchor still matches {} locations within window L{}-L{}; refine the anchor",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        Hunk::ReplaceInline {
            anchor, old, regex, ..
        } => {
            let pat = pattern_lines(anchor);
            let cands = tier1_in_window(view, &pat, lo, hi);
            match cands.len() {
                0 => None,
                1 => {
                    let mut loc = located(
                        view,
                        cands[0],
                        pat.len(),
                        4,
                        1.0,
                        &format!("hint_line {hint} (window, exact)"),
                    );
                    // 窗口内验证子串/regex 可命中（与 locate_hunk 的 ReplaceInline 一致）。
                    let win_lines = loc.win_lines.max(1);
                    let win = &view.lines[loc.start_line..loc.start_line + win_lines];
                    let hit = if *regex {
                        match regex::Regex::new(old) {
                            Ok(re) => win.iter().any(|l| re.is_match(l)),
                            Err(e) => {
                                return Some(Err(LocateError::InvalidRegex(e.to_string())));
                            }
                        }
                    } else {
                        win.iter().any(|l| l.contains(old.as_str()))
                    };
                    if !hit {
                        return None; // 窗口内无子串 → hint 无效，保留原错误
                    }
                    loc.hint_line = Some(hint);
                    Some(Ok(loc))
                }
                _ => Some(Err(LocateError::Ambiguous {
                    candidates: make_candidates(view, &cands, pat.len(), 1, 1.0, &pat),
                    detail: format!(
                        "hint_line {hint}: anchor still matches {} locations within window L{}-L{}",
                        cands.len(),
                        lo + 1,
                        hi
                    ),
                })),
            }
        }
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────
// 应用：resolve → 重叠检测 → 倒序 rope 应用
