//! Durable Ringing V1 timeline state. One atomically replaced record per session contains
//! the materialized recovery snapshot and its replay tail.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::path::PathBuf;

use qaqh_domain::{TimelineEntry, TimelineSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTimeline {
    pub seed: String,
    pub snapshot: TimelineSnapshot,
    pub journal: Vec<TimelineEntry>,
}

/// 磁盘 timeline 日志操作（append-only，刀 2 阶段 1 起为 timeline 的权威来源）。
///
/// 同一文件内按序追加：每次 persist 只追加 `watermark` 之后的新条目，因此
/// 从该流可**无损重放**出与 `TimelineAppender` 内存态完全一致的快照（前端
/// 看到的 transcript 形状不变）。与三频道 `JournalStore` 的 rewrite/compact
/// 有界语义隔离——timeline 日志从不物理删除行。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TimelineJournalOp {
    /// 恢复基点：完整物化快照（一次性历史迁移/未来压缩时写）。其 watermark
    /// 表示 `snapshot.turns` 已包含的最大 timeline_seq；其后只需追加该 seq
    /// 之后的增量条目。
    Snapshot { snapshot: TimelineSnapshot },
    /// 一条已分配 seq 的 timeline 记录（每次 persist 追加大于 watermark 的尾部）。
    Append { entry: TimelineEntry },
}

/// Timeline 持久化：`ringing-timeline/{seed}.json` 缓存 + `timeline-journal/{seed}.jsonl` 权威日志。
#[derive(Debug)]
pub struct TimelineStore {
    root: PathBuf,
    journal_root: PathBuf,
    /// 每个 seed 的 timeline journal 文件已追加到的最大 seq（懒计算缓存，防重复 append）。
    journal_watermarks: HashMap<String, u64>,
}

impl TimelineStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let parent = root.into();
        let root = parent.join("ringing-timeline");
        // Preserve replay recovery across the one-time pre-V1 → Ringing V1 rename.
        // The legacy name is migration-only; all new reads and writes use the
        // versionless Ringing timeline directory.
        let legacy = parent.join("timeline-v3");
        if !root.exists() && legacy.is_dir() {
            std::fs::rename(&legacy, &root)?;
        }
        std::fs::create_dir_all(&root)?;
        let journal_root = parent.join("timeline-journal");
        std::fs::create_dir_all(&journal_root)?;
        Ok(Self {
            root,
            journal_root,
            journal_watermarks: HashMap::new(),
        })
    }

    pub fn persist(
        &self,
        seed: &str,
        snapshot: &TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> std::io::Result<()> {
        let path = self.path_for(seed);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(&PersistedTimeline {
            seed: seed.to_string(),
            snapshot: snapshot.clone(),
            journal,
        })
        .map_err(io_error)?;
        std::fs::write(&tmp, body)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp, path)
    }

    /// 全量装载（仅测试用；生产走 `list_seeds` + `load_seed` 懒加载）。
    #[cfg(test)]
    pub fn load(&self) -> std::io::Result<HashMap<String, PersistedTimeline>> {
        let mut timelines = HashMap::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
            {
                Some(timeline) => {
                    if timeline.seed.is_empty() {
                        log::warn!("[timeline] skip record without seed {}", path.display());
                    } else {
                        timelines.insert(timeline.seed.clone(), timeline);
                    }
                }
                None => log::warn!(
                    "[timeline] skip corrupt persistent record {}",
                    path.display()
                ),
            }
        }
        Ok(timelines)
    }

    /// 磁盘上的 timeline seed 清单（懒加载索引；不读取文件内容）。
    pub fn list_seeds(&self) -> std::io::Result<Vec<String>> {
        let mut seeds = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(seed) = path.file_stem().and_then(|s| s.to_str()) {
                if !seed.is_empty() {
                    seeds.push(seed.to_string());
                }
            }
        }
        Ok(seeds)
    }

    /// 装载单个 seed 的持久化 timeline（懒加载按需恢复用）。
    pub fn load_seed(&self, seed: &str) -> Option<PersistedTimeline> {
        let path = self.path_for(seed);
        std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
    }

    fn path_for(&self, seed: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_seed(seed)))
    }

    /// 追加 timeline journal 尾部。按当前 `journal_watermark` 去重：只追加 seq 大于
    /// watermark 的条目（hub 已用 `replay_since(seed, watermark)` 过滤，此处再防御
    /// 一次，防止因调用方状态滞后把已落盘条目重复写入）。追加后更新该 seed 的
    /// watermark，使 `persist_timeline_sync` 与异步 checkpoint 线程不会重复写。
    pub fn append_journal(&mut self, seed: &str, entries: &[TimelineEntry]) -> std::io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let watermark = self.journal_watermark(seed);
        let new: Vec<&TimelineEntry> = entries
            .iter()
            .filter(|entry| entry.timeline_seq > watermark)
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        let path = self.journal_path_for(seed);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for entry in &new {
            write_journal_line(
                &mut file,
                &TimelineJournalOp::Append {
                    entry: (*entry).clone(),
                },
            )?;
        }
        file.flush()?;
        let max = new
            .iter()
            .map(|entry| entry.timeline_seq)
            .max()
            .unwrap_or(watermark);
        self.journal_watermarks.insert(seed.to_string(), max);
        Ok(())
    }

    /// 该 seed 的 timeline journal 当前最大 seq（无文件则为 0）。懒计算并缓存。
    pub fn journal_watermark(&mut self, seed: &str) -> u64 {
        if let Some(&watermark) = self.journal_watermarks.get(seed) {
            return watermark;
        }
        let watermark = self.journal_max_seq(seed);
        self.journal_watermarks.insert(seed.to_string(), watermark);
        watermark
    }

    /// 一次性历史迁移：把旧 `PersistedTimeline` 转成 `Snapshot` 基点 + `Append`
    /// 尾部写入 timeline journal。幂等：目标文件已存在则跳过（不重复追加）。
    pub fn backfill_journal(
        &mut self,
        seed: &str,
        ops: &[TimelineJournalOp],
    ) -> std::io::Result<()> {
        let path = self.journal_path_for(seed);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&path)?;
        for op in ops {
            write_journal_line(&mut file, op)?;
        }
        file.flush()?;
        let max = ops
            .iter()
            .filter_map(|op| match op {
                TimelineJournalOp::Snapshot { snapshot } => Some(snapshot.watermark),
                TimelineJournalOp::Append { entry } => Some(entry.timeline_seq),
            })
            .max()
            .unwrap_or(0);
        self.journal_watermarks.insert(seed.to_string(), max);
        Ok(())
    }

    /// 读取该 seed 的 timeline journal ops（损坏行跳过并记录，不整体失败）。
    pub fn read_journal(&self, seed: &str) -> std::io::Result<Vec<TimelineJournalOp>> {
        let path = self.journal_path_for(seed);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        Ok(read_journal_ops(&path))
    }

    /// timeline journal 磁盘上的 seed 清单（懒加载索引；不读取文件内容）。
    pub fn list_journal_seeds(&self) -> std::io::Result<Vec<String>> {
        let mut seeds = Vec::new();
        for entry in std::fs::read_dir(&self.journal_root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(seed) = path.file_stem().and_then(|s| s.to_str()) {
                if !seed.is_empty() {
                    seeds.push(seed.to_string());
                }
            }
        }
        Ok(seeds)
    }

    fn journal_max_seq(&self, seed: &str) -> u64 {
        read_journal_ops(&self.journal_path_for(seed))
            .into_iter()
            .filter_map(|op| match op {
                TimelineJournalOp::Snapshot { snapshot } => Some(snapshot.watermark),
                TimelineJournalOp::Append { entry } => Some(entry.timeline_seq),
            })
            .max()
            .unwrap_or(0)
    }

    fn journal_path_for(&self, seed: &str) -> PathBuf {
        self.journal_root
            .join(format!("{}.jsonl", sanitize_seed(seed)))
    }
}

fn write_journal_line(file: &mut std::fs::File, op: &TimelineJournalOp) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(op).map_err(io_error)?;
    line.push(b'\n');
    file.write_all(&line)
}

fn read_journal_ops(path: &Path) -> Vec<TimelineJournalOp> {
    let mut ops = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return ops;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TimelineJournalOp>(line) {
            Ok(op) => ops.push(op),
            Err(error) => log::warn!(
                "[timeline] skip corrupt journal line in {}: {error}",
                path.display()
            ),
        }
    }
    ops
}

fn sanitize_seed(seed: &str) -> String {
    seed.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_loads_a_native_timeline_without_channel_envelopes() {
        let root = std::env::temp_dir().join(format!("qaqh-timeline-store-{}", std::process::id()));
        let store = TimelineStore::new(&root).unwrap();
        store
            .persist(
                "seed",
                &TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded["seed"].snapshot.watermark, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migrates_legacy_timeline_storage_into_the_ringing_v1_root() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-timeline-store-migration-{}",
            std::process::id()
        ));
        let legacy = root.join("timeline-v3");
        std::fs::create_dir_all(&legacy).expect("create legacy directory");
        let record = PersistedTimeline {
            seed: "seed".into(),
            snapshot: TimelineSnapshot {
                watermark: 7,
                turns: vec![],
            },
            journal: vec![],
        };
        std::fs::write(
            legacy.join("seed.json"),
            serde_json::to_vec(&record).expect("serialize legacy record"),
        )
        .expect("write legacy record");

        let store = TimelineStore::new(&root).expect("migrate legacy directory");
        let loaded = store.load().expect("load migrated record");
        assert_eq!(loaded["seed"].snapshot.watermark, 7);
        assert!(root.join("ringing-timeline").is_dir());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qaqh-timeline-store-{}-{}",
            label,
            std::process::id()
        ))
    }

    fn native_entries() -> (TimelineSnapshot, Vec<TimelineEntry>) {
        let mut appender = crate::TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block(
                "s",
                "t",
                0,
                "answer",
                qaqh_domain::TimelineBlockKind::Text,
                None,
            )
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 1, "lo")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        let entries = appender.replay_since("s", 0);
        (snapshot, entries)
    }

    #[test]
    fn timeline_journal_appends_and_reloads_in_order() {
        let root = temp_root("journal-order");
        let (_, entries) = native_entries();
        {
            let mut store = TimelineStore::new(&root).unwrap();
            store.append_journal("s", &entries[..2]).unwrap();
            store.append_journal("s", &entries[2..]).unwrap();
            assert_eq!(store.journal_watermark("s"), 4, "watermark tracks max seq");
            // 重复调用（同一 watermark）不重复追加。
            store.append_journal("s", &entries).unwrap();
            assert_eq!(store.journal_watermark("s"), 4);
        }
        let loaded = TimelineStore::new(&root).unwrap();
        let ops = loaded.read_journal("s").unwrap();
        let appends: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                TimelineJournalOp::Append { entry } => Some(entry.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            appends, entries,
            "journal reload preserves order and no duplicates"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn timeline_journal_backfill_is_idempotent() {
        let root = temp_root("backfill-idempotent");
        let (snapshot, entries) = native_entries();
        let ops: Vec<TimelineJournalOp> = std::iter::once(TimelineJournalOp::Snapshot { snapshot })
            .chain(
                entries
                    .into_iter()
                    .map(|entry| TimelineJournalOp::Append { entry }),
            )
            .collect();
        {
            let mut store = TimelineStore::new(&root).unwrap();
            store.backfill_journal("s", &ops).unwrap();
            store.backfill_journal("s", &ops).unwrap(); // 幂等：文件已存在跳过
        }
        let loaded = TimelineStore::new(&root).unwrap();
        let reloaded = loaded.read_journal("s").unwrap();
        assert_eq!(reloaded.len(), ops.len(), "backfill must not duplicate");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_cache_file_still_rebuilds_from_journal() {
        // 验收 #1：删除 `ringing-timeline/{seed}.json` 后，同名 seed 仍能从
        // timeline journal 无损重放重建出逐字相同的快照（前端可见无变化）。
        let root = temp_root("cache-delete-rebuild");
        let (snapshot, entries) = native_entries();
        {
            let mut store = TimelineStore::new(&root).unwrap();
            store.persist("s", &snapshot, entries.clone()).unwrap();
            store.append_journal("s", &entries).unwrap();
        }
        std::fs::remove_file(root.join("ringing-timeline").join("s.json"))
            .expect("remove cache file");
        let loaded = TimelineStore::new(&root).unwrap();
        let ops = loaded.read_journal("s").unwrap();
        assert!(!ops.is_empty(), "journal must survive cache deletion");
        let (rebuilt, journal) =
            crate::timeline::materialize_timeline_from_journal(&ops).expect("rebuild");
        assert_eq!(rebuilt, snapshot, "journal rebuild == native snapshot");
        assert_eq!(journal.len(), entries.len());
        let _ = std::fs::remove_dir_all(&root);
    }
}
