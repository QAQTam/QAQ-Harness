//! Content guard — compliance filter for user input.
//!
//! Blocks sensitive keywords while allowing academic/crypto discussions
//! through configurable allowlist prefixes.  Uses NFKC normalization and
//! word-boundary matching for ASCII patterns.

use unicode_normalization::UnicodeNormalization;

/// NFKC-normalized blocked keyword patterns (10 pairs).
const BLOCKED_PATTERNS: &[&str] = &[
    "心理咨询",
    "情感陪伴",
    "自杀",
    "自残",
    "密钥",
    "密码",
    "api_key",
    "色情",
    "赌博",
    "毒品",
];

/// Allowlist prefixes — if the trimmed input starts with any of these,
/// the guard passes regardless of blocked keywords.
const ALLOWLIST_PREFIXES: &[&str] = &["research:", "academic:", "crypto:"];

/// Check user input against the compliance content filter.
///
/// Returns `Ok(())` when input passes.  Returns `Err(reason)` when a
/// blocked pattern is detected and the input is not allowlisted.
///
/// The check performs NFKC normalization (catches full-width / confusables)
/// and uses word-boundary matching for ASCII-only patterns.
pub fn content_guard(input: &str) -> Result<(), String> {
    let trimmed = input.trim();

    // ── Allowlist prefixes ──
    for prefix in ALLOWLIST_PREFIXES {
        if trimmed.starts_with(prefix) {
            return Ok(());
        }
    }

    // ── NFKC normalize ──
    let normalized: String = trimmed.chars().nfkc().collect();
    let lowered = normalized.to_lowercase();

    // ── Blocked-pattern check ──
    for pattern in BLOCKED_PATTERNS {
        if pattern_found(&lowered, pattern) {
            return Err(format!(
                "Content blocked by compliance filter (pattern: '{pattern}')"
            ));
        }
    }

    Ok(())
}

/// Returns `true` when `pattern` is found in `text` with word-boundary
/// semantics for ASCII patterns and substring matching for CJK patterns.
fn pattern_found(text: &str, pattern: &str) -> bool {
    let needle = pattern.to_lowercase();
    let text_bytes = text.as_bytes();

    // Purely-ASCII patterns → word-boundary matching
    let is_pure_ascii = pattern.bytes().all(|b| b.is_ascii());

    let mut search_start = 0usize;
    while let Some(pos) = text[search_start..].find(&needle) {
        let abs_start = search_start + pos;
        let abs_end = abs_start + needle.len();

        if is_pure_ascii {
            // Check word boundaries before / after
            let left_ok = abs_start == 0 || !text_bytes[abs_start - 1].is_ascii_alphanumeric();
            let right_ok =
                abs_end >= text_bytes.len() || !text_bytes[abs_end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        } else {
            // CJK / mixed patterns — substring is sufficient
            return true;
        }

        search_start = abs_start + 1;
        if search_start >= text.len() {
            break;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowlist_pass() {
        assert!(content_guard("research: suicide rates in ancient Rome").is_ok());
        assert!(content_guard("academic: token economics paper").is_ok());
        assert!(content_guard("crypto: key derivation functions").is_ok());
    }

    #[test]
    fn test_block_chinese() {
        assert!(content_guard("我想自杀").is_err());
        assert!(content_guard("赌博网站").is_err());
        assert!(content_guard("出售毒品").is_err());
    }

    #[test]
    fn test_block_api_key() {
        assert!(content_guard("my api_key is secret").is_err());
    }

    #[test]
    fn test_nfkc_normalization() {
        // Full-width characters NFKC → ASCII
        assert!(content_guard("ａｐｉ_ｋｅｙ").is_err());
    }

    #[test]
    fn test_normal_text_pass() {
        assert!(content_guard("Hello, how are you?").is_ok());
        assert!(content_guard("Can you help me write a Rust function?").is_ok());
    }
}
