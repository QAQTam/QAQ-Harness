//! matching — split from file_edit_v2.rs

use crate::edit::view::FileView;
use crate::edit::{CANDIDATE_MAX, SNIPPET_MAX, T3_MARGIN, T3_THRESHOLD};

pub(crate) fn pattern_lines(s: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = s.split('\n').collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) line_range: (usize, usize),
    pub(crate) snippet: String,
    pub(crate) score: f32,
    pub(crate) tier: u8,
    /// pattern（模型给的）与文件窗口的 -/+ 行对照：模型一眼看出差异。
    pub(crate) diff: String,
}

#[derive(Debug)]
pub(crate) struct Located {
    pub(crate) start_char: usize,
    pub(crate) end_char: usize,
    /// 窗口首行（0-based）；win_lines = 0 表示插入点。
    pub(crate) start_line: usize,
    pub(crate) win_lines: usize,
    pub(crate) tier: u8,
    pub(crate) score: f32,
    pub(crate) note: String,
    /// hint_line 兜底命中时记录模型提示的 1-based 行号（透明回传用）。
    pub(crate) hint_line: Option<usize>,
}

#[derive(Debug)]
pub(crate) enum LocateError {
    NoMatch {
        candidates: Vec<Candidate>,
        /// 为什么没命中：差阈值 / 差 margin / 完全不像（见 no_match_detail）。
        detail: String,
    },
    Ambiguous {
        candidates: Vec<Candidate>,
        detail: String,
    },
    Underspecified,
    /// replace_inline 的 `regex=true` 时正则编译失败。
    InvalidRegex(String),
}

/// Tier1：逐行全等匹配。返回全部命中位置。
pub(crate) fn tier1(view: &FileView, pat: &[&str]) -> Vec<usize> {
    let fl = &view.lines;
    if pat.is_empty() || pat.len() > fl.len() {
        return Vec::new();
    }
    (0..=fl.len() - pat.len())
        .filter(|&s| (0..pat.len()).all(|k| fl[s + k] == pat[k]))
        .collect()
}

/// 剥离公共最小缩进（spec Tier2 的"形状"）。空行/纯空白行 → 空串。
pub(crate) fn strip_indent(lines: &[&str]) -> Vec<String> {
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                // 前导空白（空格/tab）是单字节 UTF-8，min 必落在 char boundary。
                l.get(min.min(l.len())..).unwrap_or_default().to_string()
            }
        })
        .collect()
}

/// Tier2：缩进形状匹配。返回全部命中位置。
pub(crate) fn tier2(view: &FileView, pat: &[&str]) -> Vec<usize> {
    let fl = &view.lines;
    let shape = strip_indent(pat);
    if shape.is_empty() || shape.len() > fl.len() {
        return Vec::new();
    }
    (0..=fl.len() - shape.len())
        .filter(|&s| strip_indent(&fl[s..s + shape.len()]) == shape)
        .collect()
}

/// 字符级 SequenceMatcher ratio（difflib 语义：1.0=相同，0.0=完全不同）。
///
/// ⚠ 不用 `TextDiff::from_lines().ratio()`：它是**行粒度**比较（每行是原子
/// 单元），单行 typo 场景（"let fo0" vs "let foo"）会直接判 0——spec 原文
/// 假设其等价 difflib，实测不符，以 crate 实际行为为准改 from_chars。
pub(crate) fn ratio(a: &str, b: &str) -> f32 {
    similar::TextDiff::from_chars(a, b).ratio()
}

/// context 全等消歧：候选窗口中，紧贴窗口前/后的行与 context 逐行全等。
pub(crate) fn context_filter(
    view: &FileView,
    cands: &[usize],
    win: usize,
    before: &[&str],
    after: &[&str],
) -> Vec<usize> {
    let fl = &view.lines;
    cands
        .iter()
        .copied()
        .filter(|&s| {
            let b_ok = before.is_empty()
                || (s >= before.len()
                    && (0..before.len()).all(|k| fl[s - before.len() + k] == before[k]));
            let a_ok = after.is_empty()
                || (s + win + after.len() <= fl.len()
                    && (0..after.len()).all(|k| fl[s + win + k] == after[k]));
            b_ok && a_ok
        })
        .collect()
}

/// 窗口 snippet（展示用，截断）。
pub(crate) fn snippet_of(view: &FileView, s: usize, win: usize) -> String {
    let text = view.lines[s..s + win].join("\n");
    let cut = text.floor_char_boundary(text.len().min(SNIPPET_MAX));
    let mut out = text.get(..cut).unwrap_or_default().to_string();
    if cut < text.len() {
        out.push('…');
    }
    out
}

/// pattern（模型给的）与文件窗口的 -/+ 行对照：模型一眼看出差异在哪。
pub(crate) fn pattern_vs_candidate(pat: &[&str], cand: &[&str]) -> String {
    let mut out = String::new();
    let n = pat.len().min(cand.len());
    for i in 0..n {
        if pat[i] == cand[i] {
            out.push_str(&format!("    {}\n", cand[i]));
        } else {
            out.push_str(&format!("  - {}\n  + {}\n", pat[i], cand[i]));
        }
    }
    for l in &cand[n..] {
        out.push_str(&format!("  + {}\n", l));
    }
    for l in &pat[n..] {
        out.push_str(&format!("  - {}\n", l));
    }
    out.trim_end().to_string()
}

pub(crate) fn make_candidates(
    view: &FileView,
    positions: &[usize],
    win: usize,
    tier: u8,
    score: f32,
    pat: &[&str],
) -> Vec<Candidate> {
    positions
        .iter()
        .take(CANDIDATE_MAX)
        .map(|&s| Candidate {
            line_range: (s + 1, s + win),
            snippet: snippet_of(view, s, win),
            score,
            tier,
            diff: pattern_vs_candidate(pat, &view.lines[s..s + win]),
        })
        .collect()
}

/// Tier3 探测结果：hit 达标（满足阈值 + margin），best/second 供失败诊断。
pub(crate) struct Tier3Probe {
    pub(crate) hit: Option<(usize, usize, f32)>,
    pub(crate) best: f32,
    pub(crate) second: f32,
}

/// 失败诊断：告诉模型**为什么**没采纳（差阈值 / 差 margin / 完全不像），
/// 而不是只给一句 NO_MATCH。
pub(crate) fn no_match_detail(probe: &Tier3Probe) -> String {
    if probe.best >= T3_THRESHOLD {
        format!(
            "best score {:.2} but margin to second-best ({:.2}) is below {:.2} — two locations look equally plausible; add context_before/context_after or extend 'old'",
            probe.best, probe.second, T3_MARGIN
        )
    } else if probe.best > 0.01 {
        format!(
            "best score {:.2} is below threshold {:.2} — closest location is probably wrong; re-check 'old' against the file (see candidates below)",
            probe.best, T3_THRESHOLD
        )
    } else {
        "no window had any similarity — 'old' does not resemble any part of the file".to_string()
    }
}

/// Tier3：相似度评分探测（不采纳）。达标返回 hit。
///
/// 权重：old 0.6，context_before 0.2，context_after 0.2；缺失的 context
/// 不占权重（总权重归一化，score 恒在 0~1）。
/// 窗口行数容差：pattern 行数 −2..=+2（≥1）。
pub(crate) fn tier3_probe(
    view: &FileView,
    pat: &[&str],
    before: &[&str],
    after: &[&str],
) -> Tier3Probe {
    let fl = &view.lines;
    let p = pat.len();
    if p == 0 || p > fl.len() + 2 {
        return Tier3Probe {
            hit: None,
            best: 0.0,
            second: 0.0,
        };
    }
    let old_s = strip_indent(pat).join("\n");
    let b_s = strip_indent(before).join("\n");
    let a_s = strip_indent(after).join("\n");
    let w_old = 0.6f32;
    let w_b = if before.is_empty() { 0.0 } else { 0.2 };
    let w_a = if after.is_empty() { 0.0 } else { 0.2 };
    let w_total = w_old + w_b + w_a;

    let lo = p.saturating_sub(2).max(1);
    let hi = (p + 2).min(fl.len());
    let mut best: Option<(usize, usize, f32)> = None;
    let mut second_score: f32 = 0.0;
    for win in lo..=hi {
        for s in 0..=fl.len() - win {
            let win_s = strip_indent(&fl[s..s + win]).join("\n");
            let mut score = w_old * ratio(&old_s, &win_s);
            if w_b > 0.0 {
                let avail = strip_indent(&fl[s.saturating_sub(before.len())..s]).join("\n");
                score += w_b * ratio(&b_s, &avail);
            }
            if w_a > 0.0 {
                let a_end = (s + win + after.len()).min(fl.len());
                let avail = strip_indent(&fl[s + win..a_end]).join("\n");
                score += w_a * ratio(&a_s, &avail);
            }
            let score = score / w_total;
            match best {
                None => best = Some((s, win, score)),
                Some((_, _, bs)) if score > bs => {
                    second_score = bs;
                    best = Some((s, win, score));
                }
                Some((_, _, _)) => {
                    // 含并列：score == bs 时 second 被抬到 bs，margin 归零 → 拒绝。
                    if score > second_score {
                        second_score = score;
                    }
                }
            }
        }
    }
    match best {
        None => Tier3Probe {
            hit: None,
            best: 0.0,
            second: 0.0,
        },
        Some((s, w, sc)) => Tier3Probe {
            hit: (sc >= T3_THRESHOLD && (sc - second_score) >= T3_MARGIN).then_some((s, w, sc)),
            best: sc,
            second: second_score,
        },
    }
}

/// Tier3 全窗口候选（不判 margin），Top3。
pub(crate) fn tier3_candidates(
    view: &FileView,
    pat: &[&str],
    before: &[&str],
    after: &[&str],
) -> Vec<Candidate> {
    let fl = &view.lines;
    let p = pat.len();
    if p == 0 || p > fl.len() + 2 {
        return Vec::new();
    }
    let old_s = strip_indent(pat).join("\n");
    let b_s = strip_indent(before).join("\n");
    let a_s = strip_indent(after).join("\n");
    let w_old = 0.6f32;
    let w_b = if before.is_empty() { 0.0 } else { 0.2 };
    let w_a = if after.is_empty() { 0.0 } else { 0.2 };
    let w_total = w_old + w_b + w_a;

    let lo = p.saturating_sub(2).max(1);
    let hi = (p + 2).min(fl.len());
    let mut all: Vec<(f32, usize, usize)> = Vec::new();
    for win in lo..=hi {
        for s in 0..=fl.len() - win {
            let win_s = strip_indent(&fl[s..s + win]).join("\n");
            let mut score = w_old * ratio(&old_s, &win_s);
            if w_b > 0.0 {
                let avail = strip_indent(&fl[s.saturating_sub(before.len())..s]).join("\n");
                score += w_b * ratio(&b_s, &avail);
            }
            if w_a > 0.0 {
                let a_end = (s + win + after.len()).min(fl.len());
                let avail = strip_indent(&fl[s + win..a_end]).join("\n");
                score += w_a * ratio(&a_s, &avail);
            }
            all.push((score / w_total, s, win));
        }
    }
    all.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(CANDIDATE_MAX);
    all.into_iter()
        .map(|(score, s, win)| Candidate {
            line_range: (s + 1, s + win),
            snippet: snippet_of(view, s, win),
            score,
            tier: 3,
            diff: pattern_vs_candidate(pat, &view.lines[s..s + win]),
        })
        .collect()
}
