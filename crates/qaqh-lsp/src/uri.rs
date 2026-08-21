//! Filesystem path ↔ `file://` URI conversion.
//!
//! Windows specifics: drive letters, `\\?\` verbatim prefixes, backslash
//! separators, percent-encoding of reserved / non-ASCII characters.
//!
//! Round-trip guarantee: `uri_to_path(&path_to_uri(p)) == p` for absolute
//! paths (drive-letter case preserved as-is).

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors raised during URI conversion.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UriError {
    #[error("not a file:// URI: `{0}`")]
    NotFileScheme(String),
    #[error("invalid percent-encoding in URI `{0}`")]
    InvalidPercent(String),
    #[error("URI path is not valid UTF-8: `{0}`")]
    NonUtf8(String),
}

/// RFC 3986 pchar plus `/`: kept literal in URI paths (drive-colons and `@`
/// stay readable; everything else — spaces, `#`, `?`, `%`, non-ASCII — is
/// percent-encoded).
fn is_path_char(b: u8) -> bool {
    matches!(
        b,
        b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'@'
            | b'/'
    )
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'A' + v - 10) as char,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if is_path_char(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0F));
        }
    }
    out
}

fn percent_decode(s: &str) -> Result<Vec<u8>, UriError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(UriError::InvalidPercent(s.to_owned()));
                }
                let hi =
                    hex_val(bytes[i + 1]).ok_or_else(|| UriError::InvalidPercent(s.to_owned()))?;
                let lo =
                    hex_val(bytes[i + 2]).ok_or_else(|| UriError::InvalidPercent(s.to_owned()))?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn normalize_sep(s: &str) -> String {
    // `file:///F:/x` keeps a leading slash from the URI path syntax; the
    // drive letter then follows. Strip it and use Windows separators.
    s.strip_prefix('/').unwrap_or(s).replace('/', "\\")
}

#[cfg(not(windows))]
fn normalize_sep(s: &str) -> String {
    s.to_owned()
}

/// Convert an absolute filesystem path to a `file://` URI string.
///
/// Strips a Windows `\\?\` verbatim prefix and percent-encodes reserved and
/// non-ASCII characters. Example: `F:\QAQ-Harness\a b.rs` → `file:///F:/QAQ-Harness/a%20b.rs`.
pub fn path_to_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let unified = stripped.replace('\\', "/");
    let encoded = percent_encode(unified.as_bytes());
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// Convert a `file://` URI string back to a filesystem path.
///
/// Accepts `file:///F:/...` (standard), `file://F:/...` (drive letter as
/// authority), and `file://localhost/...`. The result uses platform
/// separators and has no verbatim prefix.
pub fn uri_to_path(uri: &str) -> Result<PathBuf, UriError> {
    let rest = uri
        .strip_prefix("file://")
        .ok_or_else(|| UriError::NotFileScheme(uri.to_owned()))?;
    let (authority, path_part) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    let decoded = percent_decode(&path_part)?;
    let s = String::from_utf8(decoded).map_err(|_| UriError::NonUtf8(uri.to_owned()))?;
    // `file://F:/...`: the drive letter sits in the authority — glue it back.
    let full = if !authority.is_empty() && authority != "localhost" {
        format!("{authority}{s}")
    } else {
        s
    };
    Ok(PathBuf::from(normalize_sep(&full)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn windows_path_to_uri() {
        assert_eq!(
            path_to_uri(Path::new(r"F:\QAQ-Harness\crates\a b.rs")),
            "file:///F:/QAQ-Harness/crates/a%20b.rs"
        );
        assert_eq!(
            path_to_uri(Path::new(r"F:\QAQ-Harness\a#b?c%.rs")),
            "file:///F:/QAQ-Harness/a%23b%3Fc%25.rs"
        );
        assert_eq!(
            path_to_uri(Path::new(r"F:\测试\文件.rs")),
            "file:///F:/%E6%B5%8B%E8%AF%95/%E6%96%87%E4%BB%B6.rs"
        );
    }

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn verbatim_prefix_is_stripped() {
        assert_eq!(
            path_to_uri(Path::new(r"\\?\F:\QAQ-Harness\x.rs")),
            "file:///F:/QAQ-Harness/x.rs"
        );
    }

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn uri_to_windows_path() {
        assert_eq!(
            uri_to_path("file:///F:/QAQ-Harness/crates/a%20b.rs").unwrap(),
            PathBuf::from(r"F:\QAQ-Harness\crates\a b.rs")
        );
        assert_eq!(
            uri_to_path("file:///F:/%E6%B5%8B%E8%AF%95/%E6%96%87%E4%BB%B6.rs").unwrap(),
            PathBuf::from(r"F:\测试\文件.rs")
        );
    }

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn drive_letter_as_authority() {
        assert_eq!(
            uri_to_path("file://F:/QAQ-Harness/x.rs").unwrap(),
            PathBuf::from(r"F:\QAQ-Harness\x.rs")
        );
    }

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn localhost_authority_ignored() {
        assert_eq!(
            uri_to_path("file://localhost/F:/QAQ-Harness/x.rs").unwrap(),
            PathBuf::from(r"F:\QAQ-Harness\x.rs")
        );
    }

    #[test]
    #[cfg(windows)] // Windows 盘符路径语义；Linux 下无盘符概念
    fn roundtrip_preserves_path() {
        for p in [
            r"F:\QAQ-Harness\crates\a b.rs",
            r"F:\QAQ-Harness\src\a#b?c%.rs",
            r"F:\测试\文件.rs",
            r"F:\QAQ-Harness",
        ] {
            assert_eq!(
                uri_to_path(&path_to_uri(Path::new(p))).unwrap(),
                PathBuf::from(p)
            );
        }
    }

    #[test]
    fn rejects_non_file_scheme() {
        assert_eq!(
            uri_to_path("http://example.com/x").unwrap_err(),
            UriError::NotFileScheme("http://example.com/x".to_owned())
        );
    }

    #[test]
    fn rejects_bad_percent_encoding() {
        assert!(matches!(
            uri_to_path("file:///F:/bad%2"),
            Err(UriError::InvalidPercent(_))
        ));
        assert!(matches!(
            uri_to_path("file:///F:/bad%zz"),
            Err(UriError::InvalidPercent(_))
        ));
    }
}
