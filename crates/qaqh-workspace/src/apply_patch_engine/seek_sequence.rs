//! 4-tier content matching: exact → rstrip → trim → Unicode-normalised.
//! Ported from OpenAI `codex-rs/apply-patch` (Apache-2.0).

use crate::apply_patch_engine::UpdateMode;

/// Find `pattern` inside `lines` starting at `start`.
///
/// - `eof`: anchor the search at the end of the file (the hunk was marked
///   `*** End of File`).
/// - Matching passes, in order: exact, trailing-whitespace-insensitive,
///   fully-trimmed, and Unicode-normalised (typographic punctuation → ASCII).
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
    update_file_mode: UpdateMode,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }

    // When the pattern is longer than the available input there is no possible
    // match. Early-return to avoid the out-of-bounds slice that would occur in
    // the search loops below.
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        let eof_start = lines.len() - pattern.len();
        match update_file_mode {
            UpdateMode::NormalizeToLf => eof_start,
            UpdateMode::PreserveLineEndings => eof_start.max(start),
        }
    } else {
        start
    };
    // Exact match first.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Then rstrip match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    // Finally, trim both sides to allow more lenience.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // Most permissive pass: match after normalising common Unicode punctuation
    // to their ASCII equivalents, so patches authored with plain ASCII can be
    // applied to source files that contain typographic dashes / quotes. This
    // mirrors the fuzzy behaviour of `git apply` for context lines.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if normalise(&lines[i + p_idx]) != normalise(pat) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    None
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            // Various dash / hyphen code-points → ASCII '-'
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Fancy single quotes → '\''
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Fancy double quotes → '"'
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Non-breaking space and other odd spaces → normal space
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::seek_sequence;
    use crate::apply_patch_engine::UpdateMode;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false, UpdateMode::NormalizeToLf),
            Some(1)
        );
    }

    #[test]
    fn rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false, UpdateMode::NormalizeToLf),
            Some(0)
        );
    }

    #[test]
    fn trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false, UpdateMode::NormalizeToLf),
            Some(0)
        );
    }

    #[test]
    fn pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic – must return None when pattern cannot possibly fit.
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false, UpdateMode::NormalizeToLf),
            None
        );
    }

    #[test]
    fn unicode_normalised_match_accepts_typographic_dash() {
        // File contains U+2014 (em dash); patch was authored with ASCII '-'.
        let lines = to_vec(&["let x = a — b;"]);
        let pattern = to_vec(&["let x = a - b;"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 0, false, UpdateMode::NormalizeToLf),
            Some(0)
        );
    }
}
