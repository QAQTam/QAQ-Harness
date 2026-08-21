//! Tool execution audit — append-only log of all tool calls.
//!
//! Each tool invocation produces an [`AuditEntry`] that is appended to
//! `<data_dir>/audit.csv`. The audit provides a tamper-evident trail
//! (via SHA-256 argument hashes) for security review.

use sha2::Digest;
use std::fs::OpenOptions;
use std::io::Write;

/// A single audit entry for a tool invocation.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub ts: String,
    pub user: String,
    pub tool: String,
    pub action: String,
    pub args_hash: String,
    pub result: String,
    pub elapsed_ms: u64,
    pub files: Vec<String>,
}

/// Path to the audit log file.
fn audit_path() -> std::path::PathBuf {
    let data_dir = qaqh_types::platform::data_dir();
    data_dir.join("audit.csv")
}

/// Append a single audit entry to the CSV log.
pub fn append_audit(entry: &AuditEntry) {
    let path = audit_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("audit: cannot open {}: {e}", path.display());
            return;
        }
    };
    // CSV row: ts,user,tool,action,args_hash,result,elapsed_ms,files
    let files_str = entry.files.join(";");
    let row = format!(
        "{},{},{},{},{},{},{},{}\n",
        csv_escape(&entry.ts),
        csv_escape(&entry.user),
        csv_escape(&entry.tool),
        csv_escape(&entry.action),
        csv_escape(&entry.args_hash),
        csv_escape(&entry.result),
        entry.elapsed_ms,
        csv_escape(&files_str),
    );
    if let Err(e) = file.write_all(row.as_bytes()) {
        log::error!("audit: write error: {e}");
    }
}

/// Compute SHA-256 hex digest of the serialized arguments.
pub fn hash_args(args: &serde_json::Value) -> String {
    let json_str = serde_json::to_string(args).unwrap_or_default();
    let hash = sha2::Sha256::digest(json_str.as_bytes());
    hex::encode(hash)
}

/// Escape a string for CSV: wrap in quotes if it contains comma, quote, or newline.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        s.to_string()
    }
}
