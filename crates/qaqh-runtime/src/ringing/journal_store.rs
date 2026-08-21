//! 持久化 journal 存储（daemon 重启后可靠事件不丢）。
//!
//! 磁盘格式为 append-only JSONL：每条记录对应一次内存变更
//! （`Append`/`Checkpoint`/`Compact`），启动时按序重放可完整重建
//! `ReliableJournal`/`ChannelRouter`/`SnapshotProjector` 状态。
//! 有界语义由重放时的容量上限自然复现，与内存行为一致；磁盘增长是已知取舍
//! （RoundCompleted 压缩只追加一条 `Compact` 记录，不删除旧行）。
//! I/O 失败返回 Err，由调用方记录日志，绝不阻塞事件发布路径。

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use qaqh_domain::RingingChannel;
use qaqh_ringing::RingingEventEnvelope;
use serde::{Deserialize, Serialize};

type JournalKey = (RingingChannel, String);

/// 磁盘操作日志条目（按序重放）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalOp {
    /// reliable 追加或 replaceable 覆盖（按 `envelope.delivery` 重放）。
    Append { envelope: RingingEventEnvelope },
    /// replaceable 稀疏 checkpoint。
    Checkpoint { identity: String, stream_seq: u64 },
    /// RoundCompleted 到达后压缩该 round 的 delta 条目。
    Compact { turn_id: String, round_num: u32 },
}

/// 装载结果：每 (channel, seed) 的 op 序列。
#[derive(Debug, Default)]
pub struct LoadedJournal {
    pub per_seed: Vec<(RingingChannel, String, Vec<JournalOp>)>,
}

/// 持久化 journal 存储（root/journal/{channel}/{seed}.jsonl）。
#[derive(Debug)]
pub struct JournalStore {
    root: PathBuf,
    files: HashMap<JournalKey, File>,
    /// 自上次 rewrite 以来追加的字节数（内存计数，热路径零 I/O 门控）。
    /// rewrite 后清零；超阈值由调用方触发整文件重写。
    pending_bytes: HashMap<JournalKey, u64>,
}

impl JournalStore {
    /// 创建存储根目录。失败时由调用方降级为非持久模式。
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            std::fs::create_dir_all(root.join("journal").join(channel.as_str()))?;
            std::fs::create_dir_all(root.join("latest").join(channel.as_str()))?;
        }
        Ok(Self {
            root,
            files: HashMap::new(),
            pending_bytes: HashMap::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 追加一次 reliable 事件。Replaceable 使用独立有界槽位。
    pub fn append(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        envelope: &RingingEventEnvelope,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Append {
                envelope: envelope.clone(),
            },
        )
    }

    /// 记录 replaceable checkpoint。
    pub fn checkpoint(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Checkpoint {
                identity: identity.to_string(),
                stream_seq,
            },
        )
    }

    /// 记录 round delta 压缩。
    pub fn compact(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        turn_id: &str,
        round_num: u32,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Compact {
                turn_id: turn_id.to_string(),
                round_num,
            },
        )
    }

    /// Persist one bounded replaceable slot. Unlike the reliable JSONL this
    /// overwrites by identity, so per-token progress cannot grow the journal.
    pub fn replaceable(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        envelope: &RingingEventEnvelope,
    ) -> std::io::Result<()> {
        let path = self.replaceable_path(channel, seed, identity);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(envelope).map_err(io_error)?)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn remove_replaceable(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
    ) -> std::io::Result<()> {
        let path = self.replaceable_path(channel, seed, identity);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// 物理重写 jsonl：以内存中存活事件为准，丢弃已被
    /// `compact_round_deltas` 折叠的 RoundDelta 与淘汰记录。
    ///
    /// 先关闭打开句柄再原子替换（tmp + rename），避免 Windows 上 rename
    /// 与持有句柄的 append 竞态；后续写入经 `file()` 重新打开新文件。
    pub fn rewrite(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        envelopes: &[RingingEventEnvelope],
        checkpoints: &[(String, u64)],
    ) -> std::io::Result<()> {
        self.files.remove(&(channel, seed.to_string()));
        self.pending_bytes.insert((channel, seed.to_string()), 0);
        let path = self.path_for(channel, seed);
        let mut body = Vec::new();
        for envelope in envelopes {
            body.extend(
                serde_json::to_vec(&JournalOp::Append {
                    envelope: envelope.clone(),
                })
                .map_err(io_error)?,
            );
            body.push(b'\n');
        }
        for (identity, stream_seq) in checkpoints {
            body.extend(
                serde_json::to_vec(&JournalOp::Checkpoint {
                    identity: identity.clone(),
                    stream_seq: *stream_seq,
                })
                .map_err(io_error)?,
            );
            body.push(b'\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, body)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp, path)
    }

    /// 当前 jsonl 物理大小（字节）。
    pub fn file_size(&self, channel: RingingChannel, seed: &str) -> std::io::Result<u64> {
        Ok(std::fs::metadata(self.path_for(channel, seed))?.len())
    }

    /// 自上次 rewrite 以来追加的字节数（内存计数，无 I/O）。
    pub fn pending_bytes(&self, channel: RingingChannel, seed: &str) -> u64 {
        self.pending_bytes
            .get(&(channel, seed.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 磁盘上有历史记录的 (channel → seed) 清单（懒加载索引；不读取文件内容）。
    /// 覆盖 reliable jsonl 与 replaceable latest 槽目录。
    pub fn list_seeds(&self) -> HashMap<RingingChannel, HashSet<String>> {
        let mut out = HashMap::<RingingChannel, HashSet<String>>::new();
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            let journal_dir = self.root.join("journal").join(channel.as_str());
            if let Ok(entries) = std::fs::read_dir(&journal_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(seed) = path.file_stem().and_then(|s| s.to_str()) {
                        if !seed.is_empty() {
                            out.entry(channel).or_default().insert(seed.to_string());
                        }
                    }
                }
            }
            let latest_dir = self.root.join("latest").join(channel.as_str());
            if let Ok(entries) = std::fs::read_dir(&latest_dir) {
                for entry in entries.flatten() {
                    if entry.path().is_dir()
                        && let Some(seed) = entry.file_name().to_str()
                    {
                        out.entry(channel).or_default().insert(seed.to_string());
                    }
                }
            }
        }
        out
    }

    /// 装载单个 (channel, seed) 的磁盘操作序列（reliable jsonl + replaceable
    /// latest 槽，顺序与 `load()` 一致）。懒加载按需恢复用。
    pub fn load_seed(
        &self,
        channel: RingingChannel,
        seed: &str,
    ) -> std::io::Result<Vec<JournalOp>> {
        let mut ops = Vec::new();
        let path = self.path_for(channel, seed);
        if path.is_file() {
            ops = read_ops(&path);
        }
        let latest_dir = self
            .root
            .join("latest")
            .join(channel.as_str())
            .join(sanitize_seed(seed));
        if latest_dir.is_dir() {
            for entry in std::fs::read_dir(&latest_dir)? {
                let path = entry?.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<RingingEventEnvelope>(&bytes).ok())
                {
                    Some(envelope) => ops.push(JournalOp::Append { envelope }),
                    None => {
                        log::warn!("[ringing] skip corrupt replaceable slot {}", path.display())
                    }
                }
            }
        }
        Ok(ops)
    }

    /// 装载磁盘日志（损坏行跳过并记录，不整体失败）。
    pub fn load(root: impl AsRef<Path>) -> std::io::Result<LoadedJournal> {
        let root = root.as_ref().to_path_buf();
        let mut out = LoadedJournal::default();
        let journal_root = root.join("journal");
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            let dir = journal_root.join(channel.as_str());
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let seed = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                if seed.is_empty() {
                    continue;
                }
                let ops = read_ops(&path);
                if !ops.is_empty() {
                    out.per_seed.push((channel, seed, ops));
                }
            }

            let latest_dir = root.join("latest").join(channel.as_str());
            if latest_dir.is_dir() {
                for seed_entry in std::fs::read_dir(&latest_dir)? {
                    let seed_entry = seed_entry?;
                    if !seed_entry.path().is_dir() {
                        continue;
                    }
                    let seed = seed_entry.file_name().to_string_lossy().to_string();
                    let mut latest = Vec::new();
                    for entry in std::fs::read_dir(seed_entry.path())? {
                        let path = entry?.path();
                        if path.extension().and_then(|value| value.to_str()) != Some("json") {
                            continue;
                        }
                        match std::fs::read(&path).ok().and_then(|bytes| {
                            serde_json::from_slice::<RingingEventEnvelope>(&bytes).ok()
                        }) {
                            Some(envelope) => latest.push(JournalOp::Append { envelope }),
                            None => log::warn!(
                                "[ringing] skip corrupt replaceable slot {}",
                                path.display()
                            ),
                        }
                    }
                    if !latest.is_empty() {
                        if let Some((_, _, ops)) =
                            out.per_seed
                                .iter_mut()
                                .find(|(saved_channel, saved_seed, _)| {
                                    *saved_channel == channel && saved_seed == &seed
                                })
                        {
                            ops.extend(latest);
                        } else {
                            out.per_seed.push((channel, seed, latest));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn write_line(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        op: &JournalOp,
    ) -> std::io::Result<()> {
        let file = self.file(channel, seed)?;
        let mut line = serde_json::to_vec(op).map_err(io_error)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.flush()?;
        *self
            .pending_bytes
            .entry((channel, seed.to_string()))
            .or_default() += line.len() as u64;
        Ok(())
    }

    fn file(&mut self, channel: RingingChannel, seed: &str) -> std::io::Result<&mut File> {
        let key = (channel, seed.to_string());
        if !self.files.contains_key(&key) {
            let path = self.path_for(channel, seed);
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.files.insert(key.clone(), file);
        }
        Ok(self.files.get_mut(&key).expect("inserted above"))
    }

    fn path_for(&self, channel: RingingChannel, seed: &str) -> PathBuf {
        self.root
            .join("journal")
            .join(channel.as_str())
            .join(format!("{}.jsonl", sanitize_seed(seed)))
    }

    fn replaceable_path(&self, channel: RingingChannel, seed: &str, identity: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        identity.hash(&mut hasher);
        self.root
            .join("latest")
            .join(channel.as_str())
            .join(sanitize_seed(seed))
            .join(format!("{:016x}.json", hasher.finish()))
    }
}

fn read_ops(path: &Path) -> Vec<JournalOp> {
    let mut ops = Vec::new();
    let Ok(file) = File::open(path) else {
        return ops;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalOp>(line) {
            Ok(op) => ops.push(op),
            Err(error) => log::warn!(
                "[ringing] skip corrupt journal line in {}: {error}",
                path.display()
            ),
        }
    }
    ops
}

/// seed 是十六进制会话标识；防御性净化防止路径穿越。
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
    use qaqh_domain::{ConversationEvent, DomainEvent, RoundDeltaKind};

    fn env(seq: u64, event_id: &str) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "s",
            seq,
            seq,
            seq,
            event_id,
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Thinking,
                delta: "x".into(),
            })
            .into(),
        )
    }

    #[test]
    fn round_trip_reload_preserves_ops_in_order() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-ringing-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut store = JournalStore::new(&root).expect("create");
            store
                .append(RingingChannel::Conversation, "s", &env(1, "e1"))
                .expect("append 1");
            store
                .checkpoint(RingingChannel::Conversation, "s", "tool:c1", 1)
                .expect("checkpoint");
            store
                .append(RingingChannel::Conversation, "s", &env(2, "e2"))
                .expect("append 2");
            store
                .compact(RingingChannel::Conversation, "s", "t1", 0)
                .expect("compact");
        }
        let loaded = JournalStore::load(&root).expect("load");
        assert_eq!(loaded.per_seed.len(), 1);
        let (channel, seed, ops) = &loaded.per_seed[0];
        assert_eq!(*channel, RingingChannel::Conversation);
        assert_eq!(seed, "s");
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], JournalOp::Append { .. }));
        assert!(matches!(ops[1], JournalOp::Checkpoint { .. }));
        assert!(matches!(ops[2], JournalOp::Append { .. }));
        assert!(matches!(ops[3], JournalOp::Compact { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-ringing-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut store = JournalStore::new(&root).expect("create");
            store
                .append(RingingChannel::Control, "s", &env(1, "e1"))
                .expect("append");
            let path = store.path_for(RingingChannel::Control, "s");
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"{broken}\n")
                .expect("write corrupt");
        }
        let loaded = JournalStore::load(&root).expect("load");
        let (_, _, ops) = &loaded.per_seed[0];
        let loaded = JournalStore::load(&root).expect("load");
        let (_, _, ops) = &loaded.per_seed[0];
        assert_eq!(ops.len(), 1, "corrupt line skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_drops_compacted_deltas_and_preserves_survivors() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-ringing-rewrite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let delta = {
            let mut envelope = env(1, "delta-1");
            envelope.event = qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::RoundDelta {
                    turn_id: "t1".into(),
                    round_num: 0,
                    kind: qaqh_domain::RoundDeltaKind::Answering,
                    delta: "token".into(),
                },
            )
            .into();
            envelope
        };
        let turn_started = env(2, "turn-2");
        {
            let mut store = JournalStore::new(&root).expect("create");
            store
                .append(RingingChannel::Conversation, "s", &delta)
                .expect("append delta");
            store
                .append(RingingChannel::Conversation, "s", &turn_started)
                .expect("append turn");
            store
                .checkpoint(RingingChannel::Conversation, "s", "tool:c1", 9)
                .expect("checkpoint");
            let before = store
                .file_size(RingingChannel::Conversation, "s")
                .expect("size");
            // 重写：内存存活事件只有 turn_started（delta 已被 compact 折叠），
            // checkpoints 保留。重写后文件必须物理收缩。
            store
                .rewrite(
                    RingingChannel::Conversation,
                    "s",
                    &[turn_started.clone()],
                    &[("tool:c1".to_string(), 9)],
                )
                .expect("rewrite");
            let after = store
                .file_size(RingingChannel::Conversation, "s")
                .expect("size after");
            assert!(after < before, "rewrite must shrink the file");
            // 重写后句柄已关闭：再 append 必须落到新文件。
            store
                .append(RingingChannel::Conversation, "s", &env(3, "e3"))
                .expect("append after rewrite");
        }
        let loaded = JournalStore::load(&root).expect("load");
        let (_, _, ops) = &loaded.per_seed[0];
        // delta 的 Append 已被物理移除；turn_started 与后续 e3 保留；checkpoint 保留。
        let appends: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                JournalOp::Append { envelope } => Some(envelope.event_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(appends, vec!["turn-2", "e3"]);
        assert!(ops.iter().any(
            |op| matches!(op, JournalOp::Checkpoint { identity, .. } if identity == "tool:c1")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
