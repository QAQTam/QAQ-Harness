//! Token counting: CJK heuristic + optional HuggingFace tokenizer.

use std::sync::OnceLock;

#[cfg(feature = "tokenizers")]
static TOKENIZER: OnceLock<tokenizers::Tokenizer> = OnceLock::new();

/// Initialize the tokenizer from a HuggingFace `tokenizer.json` file.
/// Must be called once at startup. Returns Ok(()) on success.
#[cfg(feature = "tokenizers")]
pub fn init_tokenizer(path: &str) -> Result<(), String> {
    let tok = tokenizers::Tokenizer::from_file(path).map_err(|e| format!("load tokenizer: {e}"))?;
    let _ = TOKENIZER.set(tok);
    Ok(())
}

/// Count tokens using the loaded tokenizer if available,
/// otherwise fall back to CJK-aware heuristic.
pub fn count_tokens(text: &str) -> u32 {
    #[cfg(feature = "tokenizers")]
    if let Some(tok) = TOKENIZER.get() {
        return tok
            .encode(text, false)
            .map(|e| e.get_ids().len() as u32)
            .unwrap_or_else(|_| count_tokens_heuristic(text));
    }
    count_tokens_heuristic(text)
}

// ── Heuristic-only counting (no tokenizer dependency) ──

/// Count tokens using a CJK-aware character heuristic.
///
/// Non-CJK characters are counted as `len / 3.3`; CJK characters as
/// `count / 1.67`.  These ratios are derived from empirical DeepSeek
/// tokenizer measurements and serve as a dependency-free fallback when
/// no `tokenizer.json` is available.
fn count_tokens_heuristic(text: &str) -> u32 {
    let cjk: usize = text
        .chars()
        .filter(|c| {
            matches!(
                c,
                '\u{4e00}'..='\u{9fff}'
                    | '\u{3400}'..='\u{4dbf}'
                    | '\u{3000}'..='\u{303f}'
                    | '\u{ff00}'..='\u{ffef}'
                    | '\u{3040}'..='\u{30ff}'
            )
        })
        .count();
    (text.len().saturating_sub(cjk) as f64 / 3.3 + cjk as f64 / 1.67) as u32
}

// ── Breakdown ──

/// Token usage broken down by category.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenBreakdown {
    pub system: u32,
    pub episodic: u32,
    pub total: u32,
}
