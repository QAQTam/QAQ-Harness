//! Session Modification Journal (SMJ).
//!
//! Append-only event log for workspace file mutations. The journal lives in
//! the user data directory (not inside a repository), so it can survive a
//! deleted working tree. It records every successful `write`, `edit`,
//! `apply_patch`, and `delete` operation with enough content-addressed data to
//! replay a file back to a requested sequence point.
//!
//! Design: `docs/nextdev/SESSION-JOURNAL-PLAN.md`

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Full-file snapshots are stored as content-addressed blobs up to this size.
/// Larger files only keep their hashes; replay reports that the full content
/// is unavailable (the design allows a patch-only mode to be added later).
pub const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// A single append-only modification step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    /// Global monotonically increasing sequence number.
    pub seq: u64,
    /// Session seed this mutation belongs to.
    pub session: String,
    /// Tool call id from the conversation, when available.
    pub tool_use_id: String,
    /// Epoch seconds.
    pub ts: u64,
    /// Workspace tool name: write | edit | apply_patch | delete.
    pub tool: String,
    /// Path as seen by the tool (workspace-relative or absolute).
    pub file: String,
    /// Fine-grained operation: replace | append | overwrite | delete | ...
    pub op: String,
    /// SHA-256 of the file before the change (None for create).
    pub before_sha: Option<String>,
    /// SHA-256 of the file after the change (None for delete).
    pub after_sha: Option<String>,
    /// Optional blob name for a patch/diff representation.
    pub patch_sha: Option<String>,
    /// ok | failed | reverted
    pub result: String,
    /// Last known git commit, when available.
    pub git_before: Option<String>,
}

/// Process-wide lock for append + seq allocation. This keeps concurrent tool
/// calls in the same daemon/process from interleaving JSON lines.
static JOURNAL_IO: Mutex<()> = Mutex::new(());

/// Root directory for the SMJ.
pub fn journal_root() -> PathBuf {
    if let Ok(dir) = std::env::var("QAQH_JOURNAL_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    qaqh_types::platform::data_dir().join("journal")
}

/// `journal/steps.jsonl`
pub fn steps_path() -> PathBuf {
    journal_root().join("steps.jsonl")
}

/// `journal/blobs/`
pub fn blobs_dir() -> PathBuf {
    journal_root().join("blobs")
}

fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(blobs_dir())
}

fn blob_path(hash: &str) -> PathBuf {
    blobs_dir().join(hash)
}

/// Return the content hash and, for small files, store a full-file snapshot as
/// a content-addressed blob. Large content intentionally keeps only the hash;
/// the design allows a patch-only mode to be added later.
fn store_blob(content: &str) -> std::io::Result<String> {
    let hash = crate::file_shared::content_hash(content);
    if content.len() > MAX_BLOB_BYTES {
        return Ok(hash);
    }
    let path = blob_path(&hash);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content.as_bytes())?;
    }
    Ok(hash)
}

fn read_blob(hash: &str) -> std::io::Result<String> {
    let path = blob_path(hash);
    std::fs::read_to_string(path)
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current active session seed, if any.
pub fn active_session() -> String {
    crate::current_session().unwrap_or_default()
}

/// Read all steps from disk. Corrupt/partial trailing lines are skipped.
pub fn load_steps() -> Vec<Step> {
    let path = steps_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Step>(line) {
            Ok(step) => steps.push(step),
            Err(error) => log::warn!("[smj] skipping corrupt journal line: {error}"),
        }
    }
    steps
}

fn next_seq(steps: &[Step]) -> u64 {
    steps.iter().map(|s| s.seq).max().unwrap_or(0) + 1
}

/// Append a step after assigning its sequence number and writing blobs.
fn append_step(mut step: Step) -> std::io::Result<Step> {
    let _guard = JOURNAL_IO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_dirs()?;

    // Persist full snapshots before the index line so a crash never leaves an
    // index entry pointing at a missing blob. Callers use `record_change`,
    // which stores blobs before appending this index line.

    let existing = load_steps();
    step.seq = next_seq(&existing);

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(steps_path())?;
    let line = serde_json::to_string(&step).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    file.sync_all()?;
    Ok(step)
}

/// Record a successful file mutation in the journal.
///
/// This is intentionally non-fatal: if the journal cannot be written the tool
/// result is not blocked. The return value is `None` when journaling failed.
pub fn record_change(
    session: &str,
    tool_use_id: &str,
    tool: &str,
    file: &str,
    op: &str,
    before: Option<&str>,
    after: Option<&str>,
    result: &str,
) -> Option<u64> {
    let before_sha = before.and_then(|content| {
        store_blob(content)
            .map_err(|error| log::warn!("[smj] failed to store before blob: {error}"))
            .ok()
    });
    let after_sha = after.and_then(|content| {
        store_blob(content)
            .map_err(|error| log::warn!("[smj] failed to store after blob: {error}"))
            .ok()
    });
    let patch_sha = match (before, after) {
        (Some(before), Some(after)) => {
            let diff = crate::file_shared::unified_diff(before, after, file);
            if diff.trim().is_empty() {
                None
            } else {
                store_blob(&diff)
                    .map_err(|error| log::warn!("[smj] failed to store patch blob: {error}"))
                    .ok()
            }
        }
        _ => None,
    };

    let step = Step {
        seq: 0,
        session: session.to_string(),
        tool_use_id: tool_use_id.to_string(),
        ts: now_epoch_secs(),
        tool: tool.to_string(),
        file: file.to_string(),
        op: op.to_string(),
        before_sha,
        after_sha,
        patch_sha,
        result: result.to_string(),
        git_before: None,
    };

    match append_step(step) {
        Ok(step) => Some(step.seq),
        Err(error) => {
            log::warn!("[smj] failed to append step: {error}");
            None
        }
    }
}

/// Query journal steps with optional filters.
pub fn query(session: Option<&str>, file: Option<&str>, since: Option<u64>) -> Vec<Step> {
    load_steps()
        .into_iter()
        .filter(|step| session.is_none_or(|s| step.session == s))
        .filter(|step| file.is_none_or(|f| step.file == f))
        .filter(|step| since.is_none_or(|ts| step.ts >= ts))
        .collect()
}

/// Replay a single file up to an optional sequence point.
///
/// Returns `Ok(None)` when the file did not exist at the requested point.
pub fn replay_file(file: &str, at_seq: Option<u64>) -> Result<Option<String>, String> {
    let mut steps = query(None, Some(file), None);
    steps.sort_by_key(|step| step.seq);

    let mut current: Option<String> = None;
    for step in steps {
        if let Some(limit) = at_seq
            && step.seq > limit
        {
            break;
        }
        match &step.after_sha {
            Some(hash) => {
                let content = read_blob(hash).map_err(|error| {
                    format!("cannot read blob {hash} for {}: {error}", step.file)
                })?;
                current = Some(content);
            }
            None => current = None,
        }
    }
    Ok(current)
}

/// Alias for [`replay_file`], matching the `journal replay` naming in the design.
pub fn replay(file: &str, at_seq: Option<u64>) -> Result<Option<String>, String> {
    replay_file(file, at_seq)
}

/// Write the replayed content to `out` (or stdout when `out` is `None`).
pub fn replay_to_path(
    file: &str,
    at_seq: Option<u64>,
    out: Option<&Path>,
) -> Result<Option<String>, String> {
    let content = replay_file(file, at_seq)?;
    if let Some(out) = out {
        let path = if out.is_dir() {
            let name = Path::new(file)
                .file_name()
                .map(Path::new)
                .unwrap_or_else(|| Path::new("restored"));
            out.join(name)
        } else {
            out.to_path_buf()
        };
        if let Some(content) = &content {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create output dir: {error}"))?;
            }
            std::fs::write(&path, content.as_bytes())
                .map_err(|error| format!("cannot write output file: {error}"))?;
        } else if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cannot remove output file: {error}"))?;
        }
    }
    Ok(content)
}

/// Render journal steps as a re-appliable patch collection.
fn export_patches(steps: &[Step]) -> String {
    let mut out = String::new();
    for step in steps {
        let before = step
            .before_sha
            .as_deref()
            .and_then(|hash| read_blob(hash).ok());
        let after = step
            .after_sha
            .as_deref()
            .and_then(|hash| read_blob(hash).ok());
        match (before, after) {
            (Some(before), Some(after)) => {
                if before != after {
                    out.push_str(&crate::file_shared::unified_diff(
                        &before, &after, &step.file,
                    ));
                    out.push('\n');
                }
            }
            (None, Some(after)) => {
                out.push_str(&format!("=== Add File: {} ===\n{}\n", step.file, after));
            }
            (Some(before), None) => {
                out.push_str(&format!("=== Delete File: {} ===\n{}\n", step.file, before));
            }
            (None, None) => {}
        }
    }
    out
}

/// Execute the `journal` workspace tool.
fn exec_journal(args: &serde_json::Value) -> crate::ToolResult {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("query");
    match action {
        "query" | "export" => {
            let session = args
                .get("session")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let file = args.get("file").and_then(|v| v.as_str());
            let since = args.get("since").and_then(|v| v.as_u64());
            let format = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("json");
            let steps = query(
                (!session.is_empty()).then_some(session),
                file,
                since,
            );
            if action == "export" && format == "patches" {
                let patches = export_patches(&steps);
                let text = if patches.is_empty() {
                    "[OK] journal export: no patches\n".to_string()
                } else {
                    patches.clone()
                };
                let data = serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "ok",
                    "action": action,
                    "format": "patches",
                    "patches": patches,
                });
                return crate::ToolResult::ok_data(data, text);
            }
            let text = if steps.is_empty() {
                format!("[OK] journal {action}: no matching steps\n")
            } else {
                format!("[OK] journal {action}: {} step(s)\n", steps.len())
            };
            let data = serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "action": action,
                "format": format,
                "steps": steps,
            });
            crate::ToolResult::ok_data(data, text)
        }
        "replay" => {
            let file = match args.get("file").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                Some(file) => file,
                None => {
                    return crate::ToolResult::error(serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "error",
                        "code": "MISSING_FILE",
                        "message": "journal replay requires 'file'",
                    }).to_string());
                }
            };
            let at_seq = args.get("at").and_then(|v| v.as_u64());
            let out = args.get("out").and_then(|v| v.as_str());
            match replay_to_path(file, at_seq, out.map(std::path::Path::new)) {
                Ok(Some(content)) => {
                    let data = serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "action": "replay",
                        "file": file,
                        "at": at_seq,
                        "exists": true,
                        "content": content,
                    });
                    crate::ToolResult::ok_data(data, content)
                }
                Ok(None) => {
                    let data = serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "action": "replay",
                        "file": file,
                        "at": at_seq,
                        "exists": false,
                    });
                    crate::ToolResult::ok_data(data, format!("[OK] journal replay: {file} did not exist at requested sequence\n"))
                }
                Err(error) => crate::ToolResult::error(serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "REPLAY_FAILED",
                    "message": error,
                }).to_string()),
            }
        }
        other => crate::ToolResult::error(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "INVALID_ACTION",
            "message": format!("invalid journal action {other:?} — use query, replay, or export"),
        }).to_string()),
    }
}

fn handle_journal(ctx: crate::ToolCallCtx) -> crate::ToolResult {
    exec_journal(&ctx.args)
}

/// Register the `journal` workspace tool.
pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        crate::ToolHandler {
            key: "journal".to_string(),
            description: "Query or replay the session modification journal (SMJ). Use action='query' to list recorded file modifications, action='export' to dump all steps for a session, or action='replay' to recover a file's content at a sequence point.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["query", "export", "replay"], "default": "query", "description": "query=filter steps; export=all steps for a session; replay=restore file content"},
                    "session": {"type": "string", "description": "Session seed filter"},
                    "file": {"type": "string", "description": "File path filter (for query) or file to replay"},
                    "since": {"type": "integer", "description": "Only steps with ts >= since (epoch seconds)"},
                    "at": {"type": "integer", "description": "Replay up to this sequence number (inclusive)"},
                    "out": {"type": "string", "description": "Optional output path for replay"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            handler: handle_journal,
            risk: crate::ToolRisk::Write,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(30),
        },
        crate::ToolPlacement::Workspace,
    );
}

/// CLI entry: `qaqh-workspace journal query|replay|export ...`
pub fn cli_main(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("Usage: qaqh-workspace journal <query|replay|export> ...");
        return 2;
    };

    match command {
        "query" => {
            let mut session = None;
            let mut file = None;
            let mut since = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" => {
                        i += 1;
                        if i < args.len() {
                            session = Some(args[i].clone());
                        }
                    }
                    "--file" => {
                        i += 1;
                        if i < args.len() {
                            file = Some(args[i].clone());
                        }
                    }
                    "--since" => {
                        i += 1;
                        if i < args.len() {
                            since = args[i].parse::<u64>().ok();
                        }
                    }
                    other => {
                        eprintln!("unknown journal query argument: {other}");
                        return 2;
                    }
                }
                i += 1;
            }
            let steps = query(session.as_deref(), file.as_deref(), since);
            println!(
                "{}",
                serde_json::to_string_pretty(&steps).unwrap_or_else(|_| "[]".into())
            );
            0
        }
        "replay" => {
            let file = args.get(1).map(String::as_str).unwrap_or("");
            if file.is_empty() {
                eprintln!(
                    "Usage: qaqh-workspace journal replay <file> [--at <seq>] [--out <file-or-dir>]"
                );
                return 2;
            }
            let mut at_seq = None;
            let mut out = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--at" => {
                        i += 1;
                        if i < args.len() {
                            at_seq = args[i].parse::<u64>().ok();
                        }
                    }
                    "--out" => {
                        i += 1;
                        if i < args.len() {
                            out = Some(PathBuf::from(&args[i]));
                        }
                    }
                    other => {
                        eprintln!("unknown journal replay argument: {other}");
                        return 2;
                    }
                }
                i += 1;
            }
            match replay_to_path(file, at_seq, out.as_deref()) {
                Ok(Some(content)) => {
                    println!("{content}");
                    0
                }
                Ok(None) => {
                    println!("(file did not exist at requested sequence)");
                    0
                }
                Err(error) => {
                    eprintln!("journal replay failed: {error}");
                    1
                }
            }
        }
        "export" => {
            let mut session = None;
            let mut format = "json";
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" => {
                        i += 1;
                        if i < args.len() {
                            session = Some(args[i].clone());
                        }
                    }
                    "--format" => {
                        i += 1;
                        if i < args.len() {
                            format = args[i].as_str();
                        }
                    }
                    other if session.is_none() => session = Some(other.to_string()),
                    other => {
                        eprintln!("unknown journal export argument: {other}");
                        return 2;
                    }
                }
                i += 1;
            }
            let steps = query(session.as_deref(), None, None);
            if format == "patches" {
                print!("{}", export_patches(&steps));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&steps).unwrap_or_else(|_| "[]".into())
                );
            }
            0
        }
        other => {
            eprintln!("unknown journal command: {other}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn with_temp_journal<T>(f: impl FnOnce() -> T) -> T {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("QAQH_JOURNAL_DIR", dir.path()) };
        let result = f();
        unsafe { std::env::remove_var("QAQH_JOURNAL_DIR") };
        result
    }

    #[test]
    fn record_and_query_roundtrip() {
        with_temp_journal(|| {
            let seq = record_change(
                "s1",
                "call_1",
                "write",
                "a.txt",
                "overwrite",
                Some("old\n"),
                Some("new\n"),
                "ok",
            )
            .expect("record");
            assert!(seq >= 1);

            let steps = query(Some("s1"), Some("a.txt"), None);
            assert_eq!(steps.len(), 1);
            assert_eq!(steps[0].seq, seq);
            assert_eq!(
                steps[0].before_sha.as_deref(),
                Some(crate::file_shared::content_hash("old\n").as_str())
            );
            assert_eq!(
                steps[0].after_sha.as_deref(),
                Some(crate::file_shared::content_hash("new\n").as_str())
            );
        });
    }

    #[test]
    fn replay_restores_byte_identical_content() {
        with_temp_journal(|| {
            record_change(
                "s1",
                "c1",
                "write",
                "a.txt",
                "overwrite",
                None,
                Some("hello\n"),
                "ok",
            );
            record_change(
                "s1",
                "c2",
                "edit",
                "a.txt",
                "replace",
                Some("hello\n"),
                Some("hello world\n"),
                "ok",
            );
            record_change(
                "s1",
                "c3",
                "delete",
                "a.txt",
                "delete",
                Some("hello world\n"),
                None,
                "ok",
            );

            assert_eq!(replay_file("a.txt", None).expect("replay"), None);
            assert_eq!(
                replay_file("a.txt", Some(1)).expect("replay"),
                Some("hello\n".to_string())
            );
            assert_eq!(
                replay_file("a.txt", Some(2)).expect("replay"),
                Some("hello world\n".to_string())
            );
        });
    }

    #[test]
    fn export_patches_contains_unified_diff() {
        with_temp_journal(|| {
            record_change(
                "s1",
                "c1",
                "write",
                "a.txt",
                "overwrite",
                None,
                Some("hello\n"),
                "ok",
            );
            record_change(
                "s1",
                "c2",
                "edit",
                "a.txt",
                "replace",
                Some("hello\n"),
                Some("hello world\n"),
                "ok",
            );
            let patches = export_patches(&query(Some("s1"), None, None));
            assert!(patches.contains("--- a/a.txt"), "got: {patches}");
            assert!(patches.contains("+++ b/a.txt"), "got: {patches}");
            assert!(patches.contains("+hello world"), "got: {patches}");
        });
    }

    #[test]
    fn replay_can_write_to_out_path() {
        with_temp_journal(|| {
            record_change(
                "s1",
                "c1",
                "write",
                "a.txt",
                "overwrite",
                None,
                Some("data\n"),
                "ok",
            );
            let out = tempfile::tempdir()
                .expect("tempdir")
                .path()
                .join("restored.txt");
            replay_to_path("a.txt", None, Some(&out)).expect("replay");
            assert_eq!(std::fs::read_to_string(&out).expect("read"), "data\n");
        });
    }
}
