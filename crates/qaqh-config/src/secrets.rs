//! Secret store: API keys never touch `config.toml`.
//!
//! Audit P0-1: credentials previously lived as plaintext in `config.toml`
//! (including the `.toml.tmp` atomic-write residue). This module moves them
//! to a dedicated `secrets.toml` next to the config file:
//!
//! - **Windows**: each value is a DPAPI-encrypted blob (`dpapi:<base64>`),
//!   protected by the current user's DPAPI key (no new dependency — the
//!   `windows` crate is already in the workspace tree).
//! - **Other platforms**: plaintext with 0600 file permissions (TODO:
//!   keyring integration; at least separated from config and permission
//!   restricted).
//!
//! `config.toml` only ever stores the opaque marker `"set"` (or nothing) for
//! configured keys. Decryption failure never falls back to reading an old
//! plaintext value.

use std::path::{Path, PathBuf};

/// Which credential slot a secret belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSlot {
    Main,
    Subagent,
    Multimodal,
}

impl SecretSlot {
    fn key(self) -> &'static str {
        match self {
            SecretSlot::Main => "main",
            SecretSlot::Subagent => "subagent",
            SecretSlot::Multimodal => "multimodal",
        }
    }
}

/// Opaque marker stored in `config.toml` for a configured key.
pub const CONFIG_MARKER: &str = "set";

/// Per-slot secret store backed by `secrets.toml` next to the config file.
#[derive(Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    /// Store at `{config_dir}/secrets.toml` (same directory as config.toml).
    pub fn default_location() -> Self {
        Self::new(qaqh_types::platform::config_path().with_file_name("secrets.toml"))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and decrypt a slot. `None` when unset, unreadable, or when the
    /// DPAPI decryption fails (never falls back to legacy plaintext).
    pub fn load(&self, slot: SecretSlot) -> Option<String> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        let doc: toml::Value = toml::from_str(&data).ok()?;
        let raw = doc
            .get(slot.key())
            .and_then(|s| s.get("api_key"))
            .and_then(|k| k.as_str())?;
        decrypt(raw).ok()
    }

    /// Whether the slot has a stored secret (does not decrypt).
    pub fn has(&self, slot: SecretSlot) -> bool {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&data) else {
            return false;
        };
        doc.get(slot.key())
            .and_then(|s| s.get("api_key"))
            .and_then(|k| k.as_str())
            .is_some_and(|raw| !raw.is_empty())
    }

    /// Encrypt and store a plaintext value for a slot (idempotent).
    pub fn set(&self, slot: SecretSlot, plaintext: &str) -> Result<(), String> {
        let encoded = encrypt(plaintext.as_bytes())?;
        let mut doc = self.read_doc();
        let mut table = match doc.get(slot.key()).cloned() {
            Some(toml::Value::Table(t)) => t,
            _ => toml::map::Map::new(),
        };
        table.insert("api_key".to_owned(), toml::Value::String(encoded));
        doc.insert(slot.key().to_owned(), toml::Value::Table(table));
        self.write_doc(&doc)
    }

    /// Remove a slot's secret.
    pub fn delete(&self, slot: SecretSlot) -> Result<(), String> {
        let mut doc = self.read_doc();
        doc.remove(slot.key());
        self.write_doc(&doc)
    }

    fn read_doc(&self) -> toml::map::Map<String, toml::Value> {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return toml::map::Map::new();
        };
        toml::from_str(&data).unwrap_or_default()
    }

    fn write_doc(&self, doc: &toml::map::Map<String, toml::Value>) -> Result<(), String> {
        let content = toml::to_string_pretty(doc)
            .map_err(|e| format!("secrets serialization failed: {e}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("secrets create_dir_all failed: {e}"))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, &content).map_err(|e| format!("secrets write failed: {e}"))?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("secrets rename failed: {e}"))?;
        restrict_permissions(&self.path);
        Ok(())
    }
}

#[cfg(windows)]
fn encrypt(plain: &[u8]) -> Result<String, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData};

    // CRYPTPROTECT_UI_FORBIDDEN: never show a prompt; CurrentUser scope.
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &in_blob,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .map_err(|e| format!("CryptProtectData failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut _));
        Ok(format!("dpapi:{}", base64_encode(&bytes)))
    }
}

#[cfg(windows)]
fn decrypt(raw: &str) -> Result<String, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};

    let b64 = raw
        .strip_prefix("dpapi:")
        .ok_or_else(|| "secret is not a dpapi blob".to_string())?;
    let blob = base64_decode(b64)?;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
            .map_err(|e| format!("CryptUnprotectData failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut _));
        String::from_utf8(bytes).map_err(|_| "decrypted secret is not utf-8".to_string())
    }
}

#[cfg(not(windows))]
fn encrypt(plain: &[u8]) -> Result<String, String> {
    // No DPAPI outside Windows: keep the value separated from config.toml and
    // rely on 0600 file permissions. TODO: keyring integration.
    String::from_utf8(plain.to_vec()).map_err(|_| "secret is not utf-8".to_string())
}

#[cfg(not(windows))]
fn decrypt(raw: &str) -> Result<String, String> {
    Ok(raw.to_string())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

// ── Minimal base64 (RFC 4648) — avoids pulling a new dependency ──
// 生产用途仅限 Windows DPAPI blob 编解码；非 Windows 平台仅测试使用。

#[cfg_attr(not(windows), allow(dead_code))]
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg_attr(not(windows), allow(dead_code))]
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(B64_ALPHABET[((b0 & 0x03) << 4 | b1 >> 4) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((b1 & 0x0F) << 2 | b2 >> 6) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg_attr(not(windows), allow(dead_code))]
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' {
            break;
        }
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("invalid base64 char: {ch}")),
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for sample in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"sk-1234567890-abcdefghijklmnopqrstuvwxyz",
        ] {
            let encoded = base64_encode(sample);
            assert_eq!(
                base64_decode(&encoded).unwrap(),
                sample,
                "sample: {sample:?}"
            );
        }
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let secret = "sk-test-secret-value";
        let encoded = encrypt(secret.as_bytes()).expect("encrypt");
        #[cfg(windows)]
        assert!(
            encoded.starts_with("dpapi:"),
            "windows secrets are dpapi blobs"
        );
        assert_eq!(decrypt(&encoded).expect("decrypt"), secret);
    }

    #[test]
    fn store_roundtrip_delete() {
        let dir = std::env::temp_dir().join(format!("qaqh-secrets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("secrets.toml");
        let store = SecretStore::new(path.clone());

        assert!(!store.has(SecretSlot::Main));
        assert!(store.load(SecretSlot::Main).is_none());

        store.set(SecretSlot::Main, "sk-main").expect("set main");
        store.set(SecretSlot::Subagent, "sk-sub").expect("set sub");
        assert!(store.has(SecretSlot::Main));
        assert_eq!(store.load(SecretSlot::Main).as_deref(), Some("sk-main"));
        assert_eq!(store.load(SecretSlot::Subagent).as_deref(), Some("sk-sub"));
        // 多槽位共存：重写 main 不丢 sub
        store
            .set(SecretSlot::Main, "sk-main-2")
            .expect("re-set main");
        assert_eq!(store.load(SecretSlot::Subagent).as_deref(), Some("sk-sub"));
        assert_eq!(store.load(SecretSlot::Main).as_deref(), Some("sk-main-2"));

        store.delete(SecretSlot::Main).expect("delete main");
        assert!(!store.has(SecretSlot::Main));
        assert!(store.load(SecretSlot::Main).is_none());
        assert_eq!(store.load(SecretSlot::Subagent).as_deref(), Some("sk-sub"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_value_is_opaque() {
        assert_eq!(CONFIG_MARKER, "set");
    }
}
