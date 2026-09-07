//! exec::truncate — token 截断（CJK 估计 + token_truncate/find_token_boundary）。

/// CJK character ranges used for token-count estimation.
/// CJK characters consume ~1.67 tokens each, vs ~3.3 for ASCII.
pub(crate) const fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}'
        | '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}'
        | '\u{3040}'..='\u{30ff}'
    )
}

/// Find byte index for `target` tokens walking forward.
pub(crate) fn find_token_boundary(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    for (i, c) in text.char_indices() {
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return i;
        }
    }
    text.len()
}

/// Find byte index for `target` tokens walking backward from end.
pub(crate) fn find_token_boundary_reverse(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, c) in chars.iter().rev() {
        if is_cjk(*c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return *i;
        }
    }
    0
}

/// Token-aware smart truncation: keeps head (70%) + tail (30%).
pub(crate) fn token_truncate(text: &str, max_tokens: u32) -> String {
    let total = qaqh_types::token::count_tokens(text);
    if total <= max_tokens {
        return text.to_string();
    }
    let head_tokens = (max_tokens as f64 * 0.7).max(1.0) as u32;
    let tail_tokens = (max_tokens as f64 * 0.3).max(1.0) as u32;
    let head_end = find_token_boundary(text, head_tokens);
    let tail_start = find_token_boundary_reverse(text, tail_tokens);
    if head_end >= tail_start {
        let end = find_token_boundary(text, max_tokens);
        format!(
            "{}\n...[TRUNCATED: {}/{} tokens. Call exec again with narrower argv or a filtering command.]",
            text.get(..end).expect("token boundary is a char boundary"),
            max_tokens,
            total
        )
    } else {
        let tail = text
            .get(tail_start..)
            .expect("token boundary is a char boundary");
        format!(
            "{}\n\n...[TRUNCATED: {}/{} tokens, {} lines dropped. Call exec again with narrower argv or a filtering command.]\n\n{}",
            text.get(..head_end)
                .expect("token boundary is a char boundary"),
            max_tokens,
            total,
            text.get(head_end..tail_start)
                .expect("token boundaries are char boundaries")
                .lines()
                .count(),
            tail.trim_start(),
        )
    }
}

// ── Output helpers ──

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | '_' | '^') => {
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
