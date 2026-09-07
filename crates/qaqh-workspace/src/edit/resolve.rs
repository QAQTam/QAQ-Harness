//! resolve — split from the v2 edit core

use crate::edit::hunk::Hunk;
use crate::edit::matching::Located;
use crate::edit::matching::*;
use crate::edit::view::FileView;
use ropey::Rope;
use std::ops::Range;

pub(crate) enum ResolvedOp {
    Replace { range: Range<usize>, new: String },
    Insert { pos: usize, text: String },
}

impl ResolvedOp {
    pub(crate) fn start(&self) -> usize {
        match self {
            ResolvedOp::Replace { range, .. } => range.start,
            ResolvedOp::Insert { pos, .. } => *pos,
        }
    }
}

/// Replace 的替换区间：行窗口 char range，末行尾换行按 `want_trailing_nl`
/// 决定是否纳入（文件尾无换行时自动退化）。纯插入（win_lines=0）→ 零长度区间。
pub(crate) fn replace_range(
    view: &FileView,
    loc: &Located,
    want_trailing_nl: bool,
) -> Range<usize> {
    if loc.win_lines == 0 {
        return loc.start_char..loc.end_char;
    }
    let start = view.char_starts[loc.start_line];
    let end = view.char_starts[loc.start_line + loc.win_lines];
    if want_trailing_nl || end <= start {
        return start..end;
    }
    // 窗口最后一行是否以 '\n' 结尾：用字节索引检查（end 的字节位置前一个字节）。
    // `end - 1` 是 char 索引（'\n' 为单字符，安全）。
    let end_byte = view.byte_starts[loc.start_line + loc.win_lines];
    let trimmed = if view.content.as_bytes().get(end_byte.wrapping_sub(1)) == Some(&b'\n') {
        end - 1
    } else {
        end
    };
    start..trimmed
}

/// 命中窗口的公共缩进（首个非空行的前导空白，取到公共最小缩进长度）。
pub(crate) fn window_indent(view: &FileView, loc: &Located) -> String {
    if loc.win_lines == 0 {
        return String::new();
    }
    let lines = &view.lines[loc.start_line..loc.start_line + loc.win_lines];
    // min 是各行前导空白字节数的最小值；命中行自身缩进 >= min，
    // 且空白均为单字节 ASCII → [..min] 必为边界（get 仅通过 lint）。
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.get(..min).unwrap_or_default().to_string())
        .unwrap_or_default()
}

/// 缩进补偿：new 先剥自身公共缩进，再逐非空行补回 `base`。
/// new 带不带缩进都给对，相对缩进结构保留，空行保持空。
pub(crate) fn reindent(new: &str, base: &str) -> String {
    if new.is_empty() || base.is_empty() {
        return new.to_string();
    }
    let lines: Vec<&str> = new.split('\n').collect();
    let stripped = strip_indent(&lines);
    stripped
        .iter()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{base}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn resolve(view: &FileView, hunk: &Hunk, loc: &Located) -> ResolvedOp {
    match hunk {
        Hunk::Replace { old, new, .. } => {
            // 换行语义：old 以 '\n' 结尾 → 区间包含末行尾换行（模型显式声明）；
            // 否则不含（替换内容对称：old 不带换行，new 也不用带，不会粘行）。
            let range = replace_range(view, loc, !old.is_empty() && old.ends_with('\n'));
            // 缩进补偿：Tier2/3 按剥离公共缩进后的形状/相似度命中，
            // new 需补回文件窗口的基准缩进（new 自身可带可不带缩进，
            // 统一剥壳后补基准，相对缩进结构保留）。Tier1 精确命中不补偿。
            let new = if loc.tier >= 2 {
                reindent(new, &window_indent(view, loc))
            } else {
                new.clone()
            };
            ResolvedOp::Replace { range, new }
        }
        Hunk::PrependFile { new } => ResolvedOp::Insert {
            pos: 0,
            text: new.clone(),
        },
        Hunk::AppendFile { new } => {
            let text = if !view.content.is_empty() && !view.content.ends_with('\n') {
                format!("\n{new}")
            } else {
                new.clone()
            };
            ResolvedOp::Insert {
                pos: view.char_len(),
                text,
            }
        }
    }
}

/// 重叠判定：
/// - 两个零长度插入点：同位置冲突；
/// - 插入点落入替换区间**严格内部**：冲突（落在边界上安全，结果与顺序无关）；
/// - 两个非零区间：按半开区间相交判定。
pub(crate) fn ranges_overlap(x: &ResolvedOp, y: &ResolvedOp) -> bool {
    let (x1, x2, xz) = match x {
        ResolvedOp::Replace { range, .. } => (range.start, range.end, false),
        ResolvedOp::Insert { pos, .. } => (*pos, *pos, true),
    };
    let (y1, y2, yz) = match y {
        ResolvedOp::Replace { range, .. } => (range.start, range.end, false),
        ResolvedOp::Insert { pos, .. } => (*pos, *pos, true),
    };
    if xz && yz {
        return x1 == y1;
    }
    if xz {
        return x1 > y1 && x1 < y2;
    }
    if yz {
        return y1 > x1 && y1 < x2;
    }
    x1 < y2 && y1 < x2
}

pub(crate) fn check_overlap(ops: &[(usize, ResolvedOp)]) -> Result<(), (usize, usize)> {
    for i in 0..ops.len() {
        for j in (i + 1)..ops.len() {
            if ranges_overlap(&ops[i].1, &ops[j].1) {
                return Err((ops[i].0, ops[j].0));
            }
        }
    }
    Ok(())
}

/// 倒序应用：位置大的先改，前面区间的 char index 不受后面编辑影响。
/// 返回 (编辑后文本, 行号偏移表)——偏移基于同一快照（单快照两阶段），
/// 供账本行号修正（read 用旧行号时自动补偿）。
pub(crate) fn apply_ops(content: &str, ops: &[(usize, ResolvedOp)]) -> (String, Vec<(usize, i64)>) {
    let mut rope = Rope::from_str(content);
    let mut shifts: Vec<(usize, i64)> = Vec::with_capacity(ops.len());
    let mut ordered: Vec<&ResolvedOp> = ops.iter().map(|(_, op)| op).collect();
    ordered.sort_by_key(|op| std::cmp::Reverse(op.start()));
    for op in ordered {
        match op {
            ResolvedOp::Replace { range, new } => {
                // 行号增量 = 新增换行数 - 移除换行数（行级替换/行内子串统一）。
                let before_line = rope.char_to_line(range.start) + 1;
                let removed_lf = rope
                    .slice(range.clone())
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as i64;
                let added_lf = new.chars().filter(|&c| c == '\n').count() as i64;
                shifts.push((before_line, added_lf - removed_lf));
                // ropey 的 remove/insert 为 in-place 修改（返回 ()）——与 spec 假定
                // 的“返回新 rope”不同，以 crate 实际 API 为准。倒序应用保证前面
                // 区间的 char index 不受后面编辑影响，in-place 同样安全。
                let start = range.start;
                rope.remove(range.clone());
                rope.insert(start, new.as_str());
            }
            ResolvedOp::Insert { pos, text } => {
                let before_line = rope.char_to_line(*pos) + 1;
                let added_lf = text.chars().filter(|&c| c == '\n').count() as i64;
                shifts.push((before_line, added_lf));
                rope.insert(*pos, text.as_str());
            }
        }
    }
    (rope.to_string(), shifts)
}

// ─────────────────────────────────────────────────────────────
// 执行结果模型
