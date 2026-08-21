//! One-time migration from legacy TOML session format to JSONL.
//!
//! Old format (v0.3.0):
//!   {sessions_dir}/{seed}-{date}/session.toml   — full SessionFile as TOML
//!   {sessions_dir}/index.toml                   — Vec<SessionMeta> as TOML
//!
//! New format (v0.4.0):
//!   {sessions_dir}/{seed}/meta.json
//!   {sessions_dir}/{seed}/messages.jsonl
//!   {sessions_dir}/index.json

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;

use qaqh_types::{Message, SessionMeta};

use crate::store;

// We only use the persistence SessionMeta from qaqh_types here.

// ── Legacy types (match v0.3.0 TOML format exactly) ──

#[derive(Debug, Deserialize)]
struct LegacySessionFile {
    seed: String,
    created_at: u64,
    updated_at: u64,
    model: String,
    #[serde(default)]
    effort: Option<String>,
    messages: Vec<Message>,
    #[serde(default)]
    last_summary: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    checksum: Option<String>,
}

/// Try to migrate old-format sessions found in `sessions_dir`.
/// Safe to call on every startup — skips already-migrated sessions.
pub fn run(sessions_dir: &Path) {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return;
    };

    let mut migrated = 0u32;

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        let toml_path = dir.join("session.toml");
        if !toml_path.exists() {
            continue;
        }

        // Already migrated — skip
        let meta_path = dir.join("meta.json");
        if meta_path.exists() {
            continue;
        }

        log::info!("[MIGRATE] found legacy session: {}", dir.display());

        match migrate_one(&dir, &toml_path) {
            Ok(seed) => {
                migrated += 1;
                log::info!("[MIGRATE] migrated session {seed} → JSONL");
            }
            Err(e) => {
                log::error!("[MIGRATE] failed {}: {e}", dir.display());
            }
        }
    }

    // Also handle the old index.toml
    let old_index = sessions_dir.join("index.toml");
    if old_index.exists() {
        let bak = sessions_dir.join("index.toml.bak");
        let _ = fs::rename(&old_index, &bak);
    }

    if migrated > 0 {
        log::info!("[MIGRATE] done — {} session(s) migrated", migrated);
    }
}

fn migrate_one(dir: &Path, toml_path: &Path) -> Result<String, String> {
    let data = fs::read_to_string(toml_path).map_err(|e| format!("read: {e}"))?;
    let legacy: LegacySessionFile =
        toml::from_str(&data).map_err(|e| format!("parse TOML: {e}"))?;

    let seed = legacy.seed.clone();

    // Write messages.jsonl
    let msg_path = dir.join("messages.jsonl");
    {
        let mut file =
            fs::File::create(&msg_path).map_err(|e| format!("create messages.jsonl: {e}"))?;
        for msg in &legacy.messages {
            let line = serde_json::to_string(msg).map_err(|e| format!("serialize msg: {e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("write msg: {e}"))?;
        }
        file.flush().map_err(|e| format!("flush: {e}"))?;
    }

    // Write meta.json
    let meta = SessionMeta {
        seed: legacy.seed.clone(),
        created_at: legacy.created_at,
        updated_at: legacy.updated_at,
        model: legacy.model.clone(),
        effort: legacy.effort.clone(),
        message_count: legacy.messages.len(),
        last_summary: legacy.last_summary.unwrap_or_default(),
        compact_skip: 0,
        ..Default::default()
    };
    store::write_meta(dir, &meta)?;

    // Rename old TOML to .bak before renaming the directory
    let bak = dir.join("session.toml.bak");
    let _ = fs::rename(toml_path, &bak);

    // Rename directory from {seed}-{date} to {seed} so session_dir() can find it.
    let parent = dir.parent().ok_or("no parent dir")?;
    let new_dir = parent.join(&seed);
    if new_dir != *dir {
        if new_dir.exists() {
            log::warn!(
                "[MIGRATE] target dir {} already exists, removing old",
                new_dir.display()
            );
            let _ = fs::remove_dir_all(&new_dir);
        }
        fs::rename(dir, &new_dir).map_err(|e| format!("rename dir: {e}"))?;
        log::info!(
            "[MIGRATE] renamed {} → {}",
            dir.display(),
            new_dir.display()
        );
    }

    Ok(seed)
}
