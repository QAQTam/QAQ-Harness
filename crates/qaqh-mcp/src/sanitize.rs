//! Secret 值脱敏工具（E-6 红线的机制面；接线在 M1-5 stderr/审计路径）。
//!
//! 原则（设计 §5.5）：secret/env 值出现在任何将进入 ToolResult、审计、日志
//! 或模型上下文的文本之前，必须先经 [`redact_secrets`]；替换为定长标记
//! `[redacted]`（不泄露长度信息）。

/// 把 `text` 中出现的每个 secret 值替换为 `[redacted]`。
///
/// 按长度降序替换——防"一个值是另一个值的前缀"时先替换短值留下半截泄漏
/// （如 secret `abc` 与 `abcdef`：先替换 `abc` 会把 `abcdef` 变成
/// `[redacted]def`，后者永远匹配不上）。空值跳过。
pub fn redact_secrets(text: &str, secrets: &[String]) -> String {
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut out = text.to_owned();
    for secret in ordered {
        out = out.replace(secret, "[redacted]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_all_values_with_fixed_marker() {
        let secrets = vec!["sk-alpha".to_owned(), "sk-beta-longer".to_owned()];
        let out = redact_secrets("key=sk-alpha other=sk-beta-longer none=sk-gamma", &secrets);
        assert_eq!(out, "key=[redacted] other=[redacted] none=sk-gamma");
    }

    #[test]
    fn longer_value_replaced_first_no_partial_leak() {
        // abc 是 abcdef 的前缀：必须先替换长者，否则留下 "[redacted]def"。
        let secrets = vec!["abc".to_owned(), "abcdef".to_owned()];
        let out = redact_secrets("x=abcdef y=abc", &secrets);
        assert_eq!(out, "x=[redacted] y=[redacted]");
    }

    #[test]
    fn empty_secrets_are_skipped() {
        assert_eq!(redact_secrets("a= b=", &["".to_owned()]), "a= b=");
        assert_eq!(redact_secrets("plain", &[]), "plain");
    }
}
