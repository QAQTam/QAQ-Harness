//! view — split from file_edit_v2.rs

use std::ops::Range;

pub(crate) struct FileView<'a> {
    pub(crate) content: &'a str,
    /// 每行内容（不含 '\n'）。文件以 '\n' 结尾时含一个尾空行。
    pub(crate) lines: Vec<&'a str>,
    /// 每行行首**字节**偏移；len = lines.len() + 1，末项 = content.len()（str 切片用）。
    pub(crate) byte_starts: Vec<usize>,
    /// 每行行首 **char** index；len = lines.len() + 1，末项 = 总字符数（ropey 操作用）。
    ///
    /// ⚠ 二者不可混用：`char_indices()` 返回的是字节偏移，此前误当 char 索引存入，
    /// 中文文件（字节数 > 字符数）下 ropey remove/insert 越界 panic、区间错位。
    pub(crate) char_starts: Vec<usize>,
}

impl<'a> FileView<'a> {
    pub(crate) fn new(content: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut byte_starts = Vec::new();
        let mut char_starts = Vec::new();
        let mut byte_start = 0usize;
        let mut char_start = 0usize;
        let mut char_count = 0usize;
        for (i, ch) in content.char_indices() {
            if ch == '\n' {
                lines.push(&content[byte_start..i]);
                byte_starts.push(byte_start);
                char_starts.push(char_start);
                byte_start = i + 1;
                char_start = char_count + 1;
            }
            char_count += 1;
        }
        if byte_start <= content.len() {
            lines.push(&content[byte_start..]);
            byte_starts.push(byte_start);
            char_starts.push(char_start);
        }
        byte_starts.push(content.len());
        char_starts.push(char_count);
        FileView {
            content,
            lines,
            byte_starts,
            char_starts,
        }
    }

    /// 行窗口 [s, s+win) 的 **char** 区间（ropey remove/insert 用）。
    pub(crate) fn char_range(&self, s: usize, win: usize) -> Range<usize> {
        self.char_starts[s]..self.char_starts[s + win]
    }

    /// 总字符数。
    pub(crate) fn char_len(&self) -> usize {
        self.char_starts[self.char_starts.len() - 1]
    }
}

// ─────────────────────────────────────────────────────────────
// 匹配流水线（Tier1 → Tier2 → Tier3 → 拒绝）
