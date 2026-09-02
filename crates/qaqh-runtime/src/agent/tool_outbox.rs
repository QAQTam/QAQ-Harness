//! L3 tool execution outbox — a per-session log of the FACT that a tool
//! finished executing, independent of the conversation persistence pipeline.
//!
//! Why this exists: the `[RESTORE]` repair that `MessageStore::from_messages`
//! performs for orphan tool_use entries always says "not executed — do NOT
//! retry". That is the safe default, but it lies in the common crash window
//! where the tool actually ran (side effects happened) and only its result
//! message was lost. The outbox records execution facts at the earliest
//! possible moment (inside the tool worker thread, right after
//! `execute_authorized` returns), so recovery can distinguish:
//!
//! - no outbox record → tool never ran → keep "not executed, retry allowed";
//! - outbox record, no persisted result → tool ran, result lost → inject
//!   "executed, outcome <ok|error>, verify state before retrying".
//!
//! File format: `<session_dir>/tool_outbox.wal`, one JSON object per line,
//! `write + fsync` per record (tool frequency — negligible cost).
//!
//! Lifecycle: records are appended during execution and reconciled once per
//! worker init (`reconcile_store`). Reconciliation rewrites the file keeping
//! only records that still match a synthetic `[RESTORE]` placeholder — those
//! must survive (the amended note lives in memory until a later full rewrite
//! persists it); records whose call id has a real result in the archive are
//! dropped.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Serializes outbox appends across parallel tool worker threads. Writes are
/// rare (per tool completion) and tiny; a global lock is simpler and safer
/// than relying on O_APPEND interleaving guarantees.
static OUTBOX_LOCK: Mutex<()> = Mutex::new(());

const OUTBOX_FILE_NAME: &str = "tool_outbox.wal";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub call_id: String,
    pub name: String,
    /// Executor-observed outcome: `"ok"` or `"error"`.
    pub status: String,
    /// Unix seconds at record time (diagnostics only).
    pub ts: u64,
}

pub fn outbox_path(session_dir: &Path) -> PathBuf {
    session_dir.join(OUTBOX_FILE_NAME)
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record one completed tool execution. Called from the tool worker thread —
/// before the result message is ingested or flushed anywhere — so the fact of
/// execution is durable even if the process dies in the next instant.
pub fn record_in(session_dir: &Path, call_id: &str, name: &str, success: bool) {
    let record = OutboxRecord {
        call_id: call_id.to_string(),
        name: name.to_string(),
        status: if success { "ok" } else { "error" }.to_string(),
        ts: now_epoch(),
    };
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(error) => {
            log::error!("tool_outbox: serialize failed for {call_id}: {error}");
            return;
        }
    };
    let _guard = OUTBOX_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let result = File::options()
        .create(true)
        .append(true)
        .open(outbox_path(session_dir))
        .and_then(|mut file| {
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_all()
        });
    if let Err(error) = result {
        log::error!("tool_outbox: append failed for {call_id}: {error}");
    }
}

/// Session-seed convenience wrapper over [`record_in`].
pub fn record(seed: &str, call_id: &str, name: &str, success: bool) {
    record_in(
        &qaqh_types::platform::sessions_dir().join(seed),
        call_id,
        name,
        success,
    );
}

/// Read all records; a torn/corrupt tail stops the scan (logged, never
/// silently dropped on disk — the file is only ever rewritten by
/// [`retain_only`]).
pub fn read_records(session_dir: &Path) -> Vec<OutboxRecord> {
    let path = outbox_path(session_dir);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::error!("tool_outbox: read {} failed: {error}", path.display());
            return Vec::new();
        }
    };
    let mut records = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<OutboxRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(error) => {
                log::error!(
                    "tool_outbox: corrupt line in {} ({error}) — ignoring tail",
                    path.display()
                );
                break;
            }
        }
    }
    records
}

/// Rewrite the outbox keeping only `keep` call ids (atomic temp + rename).
fn retain_only(session_dir: &Path, keep: &[String]) {
    let path = outbox_path(session_dir);
    let records = read_records(session_dir);
    let kept: Vec<&OutboxRecord> = records
        .iter()
        .filter(|record| keep.contains(&record.call_id))
        .collect();
    if kept.len() == records.len() {
        return;
    }
    let tmp = path.with_extension("wal.tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        for record in &kept {
            let line = serde_json::to_string(record)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)
    })();
    if let Err(error) = result {
        log::error!("tool_outbox: retain rewrite failed: {error}");
    }
}

/// Reconcile outbox records against a freshly restored `MessageStore`:
/// every record matching a synthetic `[RESTORE]` placeholder (orphan tool_use
/// with no persisted result) refines the placeholder to "executed but result
/// lost"; all other records are accounted for by real archive results and
/// pruned. Idempotent — safe to call on every worker init.
pub fn reconcile_store(store: &mut qaqh_message::MessageStore, seed: &str) {
    reconcile_store_in(&qaqh_types::platform::sessions_dir().join(seed), store);
}

/// Directory-injected variant (tests / non-standard data roots).
pub fn reconcile_store_in(session_dir: &Path, store: &mut qaqh_message::MessageStore) {
    let records = read_records(session_dir);
    if records.is_empty() {
        return;
    }
    let synthetic = store.synthetic_repair_call_ids();
    let mut unresolved: Vec<String> = Vec::new();
    let mut amended = 0usize;
    for record in &records {
        if !synthetic.iter().any(|id| id == &record.call_id) {
            continue;
        }
        let note = format!(
            "[RESTORE] Tool \"{}\" was executed before the session was saved, but its \
             result was not persisted (outcome: {}). Side effects may have occurred — \
             verify the workspace state before retrying; do NOT blindly re-run.",
            record.name, record.status
        );
        if store.amend_synthetic_repair(&record.call_id, &note) {
            amended += 1;
            unresolved.push(record.call_id.clone());
        }
    }
    if amended > 0 {
        log::info!("tool_outbox: refined {amended} orphan tool_use repair(s)");
    }
    retain_only(session_dir, &unresolved);
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_message::MessageStore;

    fn orphan_assistant(call_id: &str, name: &str) -> qaqh_types::Message {
        let mut message = qaqh_types::Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: Vec::new(),
        };
        message.content.push(qaqh_types::ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"command": "echo hi"}),
        });
        message
    }

    #[test]
    fn record_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "call-1", "bash", true);
        record_in(dir.path(), "call-2", "edit", false);
        let records = read_records(dir.path());
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].call_id, "call-1");
        assert_eq!(records[0].status, "ok");
        assert_eq!(records[1].status, "error");
    }

    #[test]
    fn reconcile_refines_placeholder_and_prunes_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_in(dir.path(), "call-ran", "bash", true);
        record_in(dir.path(), "call-unknown", "grep", true);

        // Store with an orphan tool_use that from_messages repairs.
        let assistant = orphan_assistant("call-ran", "bash");
        // A turn starts with a user message; the orphan tool_use lives in the
        // assistant step that follows it.
        let vec = vec![qaqh_types::Message::user("do it"), assistant];
        let (mut store, repairs) = MessageStore::from_messages("outbox-seed", &vec, 0);
        assert_eq!(repairs.len(), 1, "orphan tool_use must be repaired");
        assert!(
            store
                .synthetic_repair_call_ids()
                .contains(&"call-ran".to_string())
        );

        // The default note claims "not executed" — the outbox says otherwise.
        reconcile_store_in(dir.path(), &mut store);

        let note_text = store
            .turns()
            .iter()
            .flat_map(|turn| turn.steps.iter())
            .flat_map(|step| step.tool_results.iter())
            .find_map(|result| {
                result.content.iter().find_map(|block| match block {
                    qaqh_types::ContentBlock::ToolResult { result, .. } => {
                        Some(result.model.text.clone())
                    }
                    _ => None,
                })
            })
            .expect("repair result exists");
        assert!(
            note_text.contains("was executed before the session was saved"),
            "note must reflect execution fact, got: {note_text}"
        );

        // Records matching the refined placeholder are kept; unmatched ones
        // (no orphan in the archive) are pruned.
        let kept = read_records(dir.path());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].call_id, "call-ran");
    }

    #[test]
    fn reconcile_without_outbox_is_noop() {
        let vec: Vec<qaqh_types::Message> = Vec::new();
        let (mut store, _) = MessageStore::from_messages("seed-x", &vec, 0);
        // Point reconcile at a directory with no outbox file at all.
        reconcile_store_in(
            &std::env::temp_dir().join("qaqh-outbox-missing-dir-test"),
            &mut store,
        );
        assert!(store.synthetic_repair_call_ids().is_empty());
    }
}
