//! Low-level JSONL I/O for session persistence.
//!
//! Each session directory contains:
//!   meta.json      — session metadata (small, atomic replace-write)
//!   messages.jsonl — one JSON line per Message, append-only
//!
//! A central `index.json` in the sessions root enables fast listing
//! without scanning every session directory.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use qaqh_types::{Message, SessionMeta};

// ── Meta ──

/// Write session metadata to `meta.json` atomically (write to temp, rename).
pub fn write_meta(session_dir: &Path, meta: &SessionMeta) -> Result<(), String> {
    let tmp = session_dir.join(".meta.tmp");
    let dst = session_dir.join("meta.json");
    let json = serde_json::to_string_pretty(meta).map_err(|e| format!("serialize meta: {e}"))?;
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create meta tmp: {e}"))?;
        f.write_all(json.as_bytes())
            .map_err(|e| format!("write meta tmp: {e}"))?;
        f.flush().map_err(|e| format!("flush meta tmp: {e}"))?;
        f.sync_all().map_err(|e| format!("sync meta tmp: {e}"))?;
    }
    fs::rename(&tmp, &dst).map_err(|e| format!("rename meta: {e}"))?;
    Ok(())
}

/// Read session metadata from `meta.json`.
pub fn read_meta(session_dir: &Path) -> Option<SessionMeta> {
    let path = session_dir.join("meta.json");
    let data = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

// ── Messages (JSONL) ──

/// Append a single message as a JSON line to `messages.jsonl`.
/// Used for immediate per-message persistence.
pub fn append_one(session_dir: &Path, msg: &Message) -> Result<(), String> {
    let path = session_dir.join("messages.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open messages.jsonl: {e}"))?;
    let line = serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("write message: {e}"))?;
    file.flush().map_err(|e| format!("flush: {e}"))?;
    file.sync_all().map_err(|e| format!("sync: {e}"))?;
    Ok(())
}

/// Append messages as JSON lines to `messages.jsonl`.
/// Creates the file if it doesn't exist.
pub fn append_messages(session_dir: &Path, messages: &[Message]) -> Result<(), String> {
    let path = session_dir.join("messages.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open messages.jsonl: {e}"))?;
    for msg in messages {
        let line = serde_json::to_string(msg).map_err(|e| format!("serialize message: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write message: {e}"))?;
    }
    file.flush().map_err(|e| format!("flush messages: {e}"))?;
    file.sync_all().map_err(|e| format!("sync messages: {e}"))?;
    Ok(())
}

/// Rewrite the entire messages.jsonl with the given messages.
/// Used after undo or compact.
pub fn rewrite_messages(session_dir: &Path, messages: &[Message]) -> Result<(), String> {
    let tmp = session_dir.join(".messages.tmp");
    let dst = session_dir.join("messages.jsonl");
    {
        let mut file = fs::File::create(&tmp).map_err(|e| format!("create tmp: {e}"))?;
        for msg in messages {
            let line = serde_json::to_string(msg).map_err(|e| format!("serialize: {e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;
        }
        file.flush().map_err(|e| format!("flush: {e}"))?;
        file.sync_all().map_err(|e| format!("sync: {e}"))?;
    }
    fs::rename(&tmp, &dst).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Count lines in messages.jsonl (fast, reads line-by-line without parsing JSON).
pub fn count_message_lines(session_dir: &Path) -> Result<usize, String> {
    let path = session_dir.join("messages.jsonl");
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(&path).map_err(|e| format!("open: {e}"))?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

// ── Index ──

/// Read the central session index.
pub fn read_index(sessions_dir: &Path) -> Vec<SessionMeta> {
    let path = sessions_dir.join("index.json");
    let Ok(data) = fs::read_to_string(&path) else {
        return vec![];
    };
    serde_json::from_str(&data).unwrap_or_default()
}

/// Write the central session index atomically.
pub fn write_index(sessions_dir: &Path, metas: &[SessionMeta]) {
    let Ok(json) = serde_json::to_string_pretty(metas) else {
        return;
    };
    let tmp = sessions_dir.join(".index.tmp");
    let dst = sessions_dir.join("index.json");
    let write_and_sync = || -> Result<(), String> {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create index tmp: {e}"))?;
        f.write_all(json.as_bytes())
            .map_err(|e| format!("write index tmp: {e}"))?;
        f.flush().map_err(|e| format!("flush index tmp: {e}"))?;
        f.sync_all().map_err(|e| format!("sync index tmp: {e}"))?;
        Ok(())
    };
    if write_and_sync().is_err() {
        return;
    }
    let _ = fs::rename(&tmp, &dst);
}

/// Acquire an advisory file lock on the index for cross-process safety.
/// Uses a lock file with exponential backoff. Returns a guard that
/// removes the lock on drop.
struct IndexLock {
    lock_path: std::path::PathBuf,
}
impl IndexLock {
    fn acquire(sessions_dir: &Path) -> Self {
        let lock_path = sessions_dir.join(".index.lock");
        let mut backoff = std::time::Duration::from_millis(1);
        let max_backoff = std::time::Duration::from_millis(200);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return IndexLock { lock_path },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(max_backoff);
                }
                Err(_) => {
                    // Can't acquire lock — proceed without it (better than hanging)
                    log::warn!("[IndexLock] cannot create lock file, proceeding unlocked");
                    return IndexLock {
                        lock_path: std::path::PathBuf::new(),
                    };
                }
            }
        }
    }
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        if !self.lock_path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
}

/// Upsert a single session meta into the index (avoids full rewrite).
pub fn upsert_index(sessions_dir: &Path, meta: &SessionMeta) {
    let _lock = IndexLock::acquire(sessions_dir);
    let mut index = read_index(sessions_dir);
    if let Some(existing) = index.iter_mut().find(|m| m.seed == meta.seed) {
        *existing = meta.clone();
    } else {
        index.push(meta.clone());
    }
    write_index(sessions_dir, &index);
}

/// Remove a session from the index.
pub fn remove_from_index(sessions_dir: &Path, seed: &str) {
    let _lock = IndexLock::acquire(sessions_dir);
    let mut index = read_index(sessions_dir);
    index.retain(|m| m.seed != seed);
    write_index(sessions_dir, &index);
}
