//! Write-ahead log for conversation persist ops (enqueue-time durability).
//!
//! Layout of `<session_dir>/messages.wal`:
//!
//! ```text
//! {"type":"qaqh-wal-v1","next_seq":1}
//! {"seq":1,"op":{"Append":{...}}}
//! {"seq":2,"op":{"UpdateMeta":{...}}}
//! ```
//!
//! Durability contract:
//!
//! - `MessageStore::flush_meta` logs every message-bearing op BEFORE it enters
//!   the in-memory drain queue, so a process death after `log_op` never loses
//!   an already-completed round (the archive itself only sees the op at the
//!   next `drain_persist_ops`).
//! - `sync` is invoked at round boundaries (round-boundary fsync policy, user
//!   decision 2026-09-02): the archive append path already fsyncs, so the WAL
//!   only has to cover enqueue → drain, which is at most one round.
//! - The host calls `checkpoint` (truncate to header) after a successful
//!   drain. Replay is idempotent (msg_id dedupe on the archive tail), so a
//!   crash between "applied" and "checkpointed" converges instead of
//!   duplicating.
//! - `SaveFull` / compact-context ops are deliberately NOT logged: they are
//!   generation rewrites (undo / compaction). Losing one on a crash degrades
//!   gracefully (the compaction is simply not applied; it can re-trigger),
//!   whereas replaying appends across an applied generation rewrite would
//!   resurrect undone turns. Keeping them out also avoids the ambiguous
//!   "archive shorter than compact checkpoint" failure mode.
//!
//! Recovery lives in `qaqh-session` (`SessionManager::replay_message_wal`),
//! which owns the apply mapping; this module only owns the file format.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::effect::PersistOp;

pub(crate) const WAL_FILE_NAME: &str = "messages.wal";
const HEADER_KIND: &str = "qaqh-wal-v1";

#[derive(Serialize, Deserialize)]
struct WalHeader {
    #[serde(rename = "type")]
    kind: String,
    next_seq: u64,
}

#[derive(Serialize)]
struct WalLineWrite<'a> {
    seq: u64,
    op: &'a PersistOp,
}

#[derive(Deserialize)]
struct WalLineRead {
    #[allow(dead_code)]
    seq: u64,
    op: PersistOp,
}

/// Append-only WAL handle owned by a `MessageStore`.
pub struct WalWriter {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

fn header_path(path: &Path) -> PathBuf {
    path.with_extension("wal.tmp")
}

fn quarantine_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_extension(format!("wal.corrupt-{nanos}"))
}

/// Read the existing WAL header. Returns `(next_seq, has_op_lines)`.
/// A missing file yields `(1, false)`; an unreadable/corrupt header yields
/// `None` so the caller can rotate the file away.
fn scan_file(path: &Path) -> io::Result<Option<(u64, bool)>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first)? == 0 {
        // Empty file — treat as fresh.
        return Ok(Some((1, false)));
    }
    let header: WalHeader = serde_json::from_str(first.trim_end())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if header.kind != HEADER_KIND {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown WAL header kind",
        ));
    }
    let mut has_ops = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        has_ops = true;
        // Stop counting at the first torn tail: everything after a partial
        // write is meaningless. `next_seq` continuity is not needed past this
        // point because the recovery path rewrites (checkpoints) the file.
        if serde_json::from_str::<WalLineRead>(line.trim_end()).is_err() {
            break;
        }
    }
    Ok(Some((header.next_seq, has_ops)))
}

impl WalWriter {
    /// Open (or create) the WAL inside `session_dir`.
    ///
    /// Recovery (`SessionManager::replay_message_wal`) runs before a store is
    /// created, so finding op lines here means an unrecovered stale log: it is
    /// rotated to `messages.wal.stale-<ts>` instead of being appended after.
    pub fn open(session_dir: &Path) -> io::Result<Self> {
        let path = session_dir.join(WAL_FILE_NAME);
        let scanned = scan_file(&path)?;
        let (next_seq, has_ops) = scanned.unwrap_or((1, false));
        if has_ops {
            let stale = path.with_extension(format!(
                "wal.stale-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            fs::rename(&path, &stale)?;
            log::error!(
                "WAL: unrecovered op lines at open — rotated to {} (recovery should have run first)",
                stale.display()
            );
            write_header(&path, 1)?;
            return Ok(Self {
                file: open_append(&path)?,
                path,
                next_seq: 1,
            });
        }
        if scanned.is_none() {
            write_header(&path, next_seq)?;
        }
        Ok(Self {
            file: open_append(&path)?,
            path,
            next_seq,
        })
    }

    /// Append one op. Returns the assigned sequence number.
    /// `write` only — pair with [`Self::sync`] at the round boundary.
    pub fn log_op(&mut self, op: &PersistOp) -> io::Result<u64> {
        let seq = self.next_seq;
        let line = serde_json::to_string(&WalLineWrite { seq, op })
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Round-boundary fsync (user decision: `sync_data` — content, not metadata).
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Truncate the log back to a bare header (all logged ops have been
    /// applied to the archive). Atomic via temp + rename; a crash that loses
    /// the rename resurfaces the old ops, which replay dedupes idempotently.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        write_header(&self.path, self.next_seq)?;
        self.file = open_append(&self.path)?;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn write_header(path: &Path, next_seq: u64) -> io::Result<()> {
    let tmp = header_path(path);
    let header = WalHeader {
        kind: HEADER_KIND.to_string(),
        next_seq,
    };
    {
        let mut file = File::create(&tmp)?;
        let line = serde_json::to_string(&header)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Read all logged ops for recovery. A torn/corrupt line stops the scan and
/// the whole file is quarantined to `messages.wal.corrupt-<ts>` (fail-closed:
/// evidence is never silently deleted); ops before the bad line are returned.
pub fn read_ops(session_dir: &Path) -> Vec<PersistOp> {
    let path = session_dir.join(WAL_FILE_NAME);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::error!("WAL: read {} failed: {error}", path.display());
            return Vec::new();
        }
    };
    let mut ops = Vec::new();
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first).is_ok_and(|n| n > 0) {
        // Header skipped; corruption of the header itself is handled below.
    }
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<WalLineRead>(trimmed) {
            Ok(entry) => ops.push(entry.op),
            Err(error) => {
                let quarantine = quarantine_path(&path);
                log::error!(
                    "WAL: corrupt line in {} ({error}) — quarantining to {}",
                    path.display(),
                    quarantine.display()
                );
                if let Err(copy_error) = fs::copy(&path, &quarantine) {
                    log::error!("WAL: quarantine copy failed: {copy_error}");
                }
                break;
            }
        }
    }
    ops
}

/// Reset the WAL file to a bare header after a successful replay.
pub fn checkpoint_file(session_dir: &Path) {
    let path = session_dir.join(WAL_FILE_NAME);
    if !path.exists() {
        return;
    }
    if let Err(error) = write_header(&path, 1) {
        log::error!("WAL: checkpoint {} failed: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::PersistOp;
    use qaqh_types::Message;

    fn append_op(seed: &str, ids: u64) -> PersistOp {
        PersistOp::Append {
            seed: seed.to_string(),
            messages: vec![Message {
                msg_id: Some(ids),
                role: "user".into(),
                name: None,
                content: vec![qaqh_types::ContentBlock::text("hello")],
            }],
            model: "m".into(),
            effort: None,
            compact_skip: 0,
            turn_count: 1,
        }
    }

    #[test]
    fn log_read_and_checkpoint_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = WalWriter::open(dir.path()).expect("open");
        writer.log_op(&append_op("s", 1)).expect("log 1");
        writer.log_op(&append_op("s", 2)).expect("log 2");
        writer.sync().expect("sync");

        let ops = read_ops(dir.path());
        assert_eq!(ops.len(), 2);

        writer.checkpoint().expect("checkpoint");
        assert!(read_ops(dir.path()).is_empty());
    }

    #[test]
    fn torn_tail_keeps_prefix_and_survives_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = WalWriter::open(dir.path()).expect("open");
        writer.log_op(&append_op("s", 1)).expect("log 1");
        writer.sync().expect("sync");
        let path = dir.path().join(WAL_FILE_NAME);
        // Simulate a crash mid-write: append a truncated JSON line.
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("append");
            f.write_all(b"{\"seq\":2,\"op\":{\"App").expect("torn");
        }
        let ops = read_ops(dir.path());
        assert_eq!(ops.len(), 1, "prefix before the torn line must survive");
        // Quarantine copy exists next to the log.
        let has_quarantine = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("wal.corrupt-"));
        assert!(has_quarantine, "torn WAL must be quarantined, not deleted");
    }

    #[test]
    fn reopened_writer_continues_after_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut writer = WalWriter::open(dir.path()).expect("open");
            writer.log_op(&append_op("s", 1)).expect("log");
            writer.checkpoint().expect("checkpoint");
        }
        let mut writer = WalWriter::open(dir.path()).expect("reopen");
        writer.log_op(&append_op("s", 2)).expect("log after reopen");
        assert_eq!(read_ops(dir.path()).len(), 1);
    }
}
