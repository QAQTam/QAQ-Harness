//! ringing::timeline_hub — timeline 投影子系统（持久化/懒加载/发布/快照）。
//!
//! 由 `hub.rs` 拆分（Phase 2-6）：`impl RingingHub` 跨文件块 + `append_timeline_journal_tail_locked`。
//! 对外 API 不变；`Drop`/锁语义保留在 `hub.rs`。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use qaqh_domain::{TimelineEntry, TimelineIntent, TimelineSnapshot};
use tokio::sync::broadcast;

use super::hub::RingingHub;
use super::hub::{TIMELINE_PERSIST_INTERVAL, TimelinePersistence};
use crate::timeline_store::{PersistedTimeline, TimelineJournalOp, TimelineStore};
use crate::{
    TimelineAppender, TimelineError, TimelineLiveEntry, materialize_timeline_from_journal,
};

impl RingingHub {
    /// Move timeline checkpoint I/O off the producer/writer hot path.
    ///
    /// The live TimelineAppender remains the sole source of sequence allocation and
    /// broadcast ordering. Persistence is a best-effort, single-writer checkpoint
    /// queue: notifications are coalesced per seed for a fixed checkpoint window,
    /// and the worker snapshots the latest in-memory state only when the window
    /// expires. The on-disk record shape is unchanged, so bootstrap/replay
    /// compatibility is preserved. Terminal intents still use synchronous
    /// persistence as the recovery boundary.
    pub(super) fn start_timeline_persistence(&self) {
        let enabled = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !enabled {
            return;
        }

        let (wake, rx) = mpsc::channel::<()>();
        let pending_seeds = Arc::new(Mutex::new(HashSet::<String>::new()));
        let pending_for_worker = Arc::clone(&pending_seeds);
        let timeline = Arc::clone(&self.timeline);
        let timeline_store = Arc::clone(&self.timeline_store);
        let join = match std::thread::Builder::new()
            .name("qaqh-timeline-persist".into())
            .spawn(move || {
                let persist_pending = || {
                    let seeds: Vec<String> = {
                        let mut pending =
                            pending_for_worker.lock().unwrap_or_else(|e| e.into_inner());
                        pending.drain().collect()
                    };
                    for seed in seeds {
                        // Serialize snapshot selection and file replacement with
                        // terminal persistence. Taking the store lock first
                        // prevents an older async snapshot from overwriting a
                        // newer terminal checkpoint.
                        let mut store = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(store) = store.as_mut() else {
                            continue;
                        };
                        // 先追加 timeline journal 尾部（journal ≥ cache 不变量），
                        // 再写缓存文件。journal 追加失败时跳过缓存写入（fail-closed）：
                        // 否则崩溃重启后 journal 重放会丢失仅存在于缓存的尾部条目。
                        if let Err(error) =
                            append_timeline_journal_tail_locked(store, &timeline, &seed)
                        {
                            log::error!(
                                "[timeline] journal append failed for {seed}: {error}; skipping cache persist (fail-closed)"
                            );
                            continue;
                        }
                        let Some((snapshot, journal)) = ({
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.snapshot(&seed).map(|snapshot| {
                                let journal = timeline.replay_since(&seed, 0);
                                let journal =
                                    Self::prune_sealed_timeline_journal(&snapshot, journal);
                                (snapshot, journal)
                            })
                        }) else {
                            continue;
                        };
                        if let Err(error) = store.persist(&seed, &snapshot, journal) {
                            log::warn!("[timeline] persist failed for {seed}: {error}");
                        }
                    }
                };

                while rx.recv().is_ok() {
                    // Fixed window rather than a quiet-period debounce: a long,
                    // uninterrupted model stream still receives periodic crash
                    // checkpoints without rewriting at disk speed.
                    let deadline = Instant::now() + TIMELINE_PERSIST_INTERVAL;
                    let mut disconnected = false;
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(remaining) {
                            Ok(()) => {}
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }
                    persist_pending();
                    if disconnected {
                        return;
                    }
                }
                // Drain the final coalesced notifications before the worker exits.
                persist_pending();
            }) {
            Ok(join) => join,
            Err(error) => {
                log::warn!("[timeline] persistence worker unavailable: {error}");
                return;
            }
        };

        *self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(TimelinePersistence {
            wake,
            pending_seeds,
            join: Some(join),
        });
    }

    pub(super) fn request_timeline_persistence(&self, seed: &str) {
        let persistence = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(persistence) = persistence.as_ref() else {
            return;
        };
        let should_wake = persistence
            .pending_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
        if should_wake {
            let _ = persistence.wake.send(());
        }
    }

    /// 启动装载（懒加载模式）：只扫描磁盘 timeline seed 清单，不 restore 任何
    /// 快照。内存态（TimelineAppender）在首次访问该 seed 时由
    /// `ensure_timeline_loaded` 从磁盘按需恢复。
    pub(super) fn load_timeline_persisted(&self) {
        let mut seeds = HashSet::new();
        {
            let guard = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(store) => {
                    // 缓存文件 + timeline journal 的并集：journal 为阶段 1 的权威
                    // 来源，缓存缺失（被删/损坏）时仍需能从 journal 懒加载。
                    match store.list_seeds() {
                        Ok(cache_seeds) => seeds.extend(cache_seeds),
                        Err(error) => log::warn!("[timeline] cache index failed: {error}"),
                    }
                    match store.list_journal_seeds() {
                        Ok(journal_seeds) => seeds.extend(journal_seeds),
                        Err(error) => log::warn!("[timeline] journal index failed: {error}"),
                    }
                }
                None => return,
            }
        }
        let total = seeds.len();
        *self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = seeds;
        log::info!("[ringing] lazy timeline index ready: {total} persisted timelines on disk");
    }

    /// 懒加载：确保 seed 的 timeline 快照 + replay tail 已 restore 入内存。
    ///
    /// - 已在内存或磁盘无记录：零成本返回；
    /// - 磁盘有记录：读取该 seed 的持久化快照 → restore → 收尾孤儿 running
    ///   turn（原 `load_timeline_persisted` 语义，有变更则同步写回）。
    pub(super) fn ensure_timeline_loaded(&self, seed: &str) {
        // 登记（幂等）：退出时 seal_all_orphans 需要覆盖全部已知 seed——
        // 包括本次运行新建、尚未异步落盘的 seed（异步 checkpoint 落盘前
        // 磁盘清单还没有它，但内存里已有未 seal turn）。
        self.disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string());
        let _serial = self.lazy_load.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
        {
            return;
        }
        if !self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
        {
            return;
        }
        // 阶段 1：timeline journal 为权威来源；老 `{seed}.json` 降级为缓存与
        // 兼容回退（无 journal 的旧历史在首载时一次性迁移回填）。
        let (journal_ops, persisted_cache) = {
            let mut store = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match store.as_mut() {
                Some(store) => (
                    store.read_journal(seed).unwrap_or_default(),
                    store.load_seed(seed),
                ),
                None => return,
            }
        };

        let mut cache_missing = persisted_cache.is_none();
        if !journal_ops.is_empty() {
            // 快路径：缓存与 journal 对齐（watermark == journal 最大 seq，二者描述
            // 同一状态）→ 直接 restore 缓存，免去对超大历史的全量重放。
            let journal_last = journal_ops
                .iter()
                .map(|op| match op {
                    TimelineJournalOp::Snapshot { snapshot } => snapshot.watermark,
                    TimelineJournalOp::Append { entry, .. } => entry.timeline_seq,
                })
                .max()
                .unwrap_or(0);
            if let Some(persisted) = &persisted_cache
                && persisted.snapshot.watermark == journal_last
            {
                {
                    let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                    if !appender.contains(seed) {
                        appender.restore(
                            persisted.seed.clone(),
                            persisted.snapshot.clone(),
                            persisted.journal.clone(),
                        );
                    }
                }
                log::info!("[ringing] lazily loaded timeline {seed} from cache (journal aligned)");
            } else {
                // journal 权威：纯重放重建（与原生写一致，前端快照形状逐字不变）。
                match materialize_timeline_from_journal(&journal_ops) {
                    Some((snapshot, journal)) => {
                        {
                            let mut appender =
                                self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                            if !appender.contains(seed) {
                                appender.restore(
                                    seed.to_string(),
                                    snapshot.clone(),
                                    journal.clone(),
                                );
                            }
                        }
                        // 缓存缺失或滞后于 journal → 收尾后补写缓存（保前端快照路径）。
                        cache_missing = true;
                        log::info!("[ringing] lazily rebuilt timeline {seed} from journal");
                    }
                    None => {
                        // 有 journal 记录但不可重放（防御分支）→ 回退缓存载荷。
                        if let Some(persisted) = persisted_cache {
                            let mut appender =
                                self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                            appender.restore(
                                persisted.seed.clone(),
                                persisted.snapshot.clone(),
                                persisted.journal.clone(),
                            );
                        } else {
                            self.rebuild_timeline_from_messages(seed);
                            return;
                        }
                    }
                }
            }
        } else if let Some(persisted) = persisted_cache {
            // 无 journal 的旧历史：兼容 restore + 一次性迁移（回填 journal）。
            {
                let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                if !appender.contains(seed) {
                    appender.restore(
                        persisted.seed.clone(),
                        persisted.snapshot.clone(),
                        persisted.journal.clone(),
                    );
                }
            }
            self.backfill_timeline_journal_from_persisted(&persisted);
        } else {
            // 两者皆无 → BUG-006：从 messages/compact 可重建投影。
            self.rebuild_timeline_from_messages(seed);
            return;
        }

        // 上次运行遗留的孤儿 running turn 在此收尾（见 seal_orphan_running_turns）。
        // 有变更、或缓存缺失/滞后时同步落盘（journal 权威：先追 journal 再写缓存）。
        if self.seal_orphan_running_turns(seed) || cache_missing {
            self.persist_timeline_sync(seed);
        }
        log::info!("[ringing] lazily loaded timeline {seed}");
    }

    /// BUG-006：timeline 目录缺失/记录损坏时，它必须能从 messages.jsonl /
    /// compact-context 重建，否则 timeline 就不是"可重建投影"，而会变成第二份
    /// 事实源。重建结果与 conversation snapshot 同一基线（compact 优先），
    /// 并同步写回 timeline 缓存 + timeline journal（保证下次也 journal 权威）。
    pub(super) fn rebuild_timeline_from_messages(&self, seed: &str) {
        if let Some((snapshot, journal)) =
            super::timeline_rebuild::rebuild_timeline_snapshot(self.sessions.as_deref(), seed)
        {
            {
                let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
                if !appender.contains(seed) {
                    appender.restore(seed.to_string(), snapshot.clone(), journal.clone());
                }
            }
            let mut store = self
                .timeline_store
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(store) = store.as_mut() {
                if let Err(error) = append_timeline_journal_tail_locked(store, &self.timeline, seed)
                {
                    log::error!(
                        "[timeline] journal append failed for {seed}: {error}; skipping rebuild persist (fail-closed)"
                    );
                    return;
                }
                if let Err(error) = store.persist(seed, &snapshot, journal) {
                    log::warn!("[timeline] rebuild persist failed for {seed}: {error}");
                }
            }
            log::info!(
                "[ringing] rebuilt timeline {seed} from persisted messages (BUG-006 fallback)"
            );
        }
    }

    /// 一次性历史迁移：把旧 `PersistedTimeline`（snapshot + replay tail）转写为
    /// timeline journal（`Snapshot` 基点 + `Append` 尾部）。幂等：目标文件已
    /// 存在则跳过。
    pub(super) fn backfill_timeline_journal_from_persisted(&self, persisted: &PersistedTimeline) {
        let mut ops: Vec<TimelineJournalOp> =
            Vec::with_capacity(persisted.journal.len().saturating_add(1));
        ops.push(TimelineJournalOp::Snapshot {
            snapshot: persisted.snapshot.clone(),
        });
        for entry in &persisted.journal {
            // 缓存重建路径：原始落盘 ts 不在缓存内，置 None（诚实缺省）
            ops.push(TimelineJournalOp::Append {
                entry: entry.clone(),
                ts: None,
            });
        }
        let mut store = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(store) = store.as_mut()
            && let Err(error) = store.backfill_journal(&persisted.seed, &ops)
        {
            log::warn!(
                "[timeline] journal backfill failed for {}: {error}",
                persisted.seed
            );
        }
    }

    /// 接收原生 Ringing V1 timeline producer intent。此路径不接受 Agent2Ui 或 RingingEvent，
    /// 因而不会形成旧协议包装链。
    pub fn publish_timeline(
        &self,
        seed: &str,
        intent: TimelineIntent,
    ) -> Result<TimelineEntry, TimelineError> {
        // P1: 懒加载——publish 前确保该 seed 历史 timeline 已 restore，
        // 否则新条目会与磁盘快照断链（replay tail 丢失历史）。
        self.ensure_timeline_loaded(seed);
        // Terminal intents (block/round/turn sealed) are the recovery boundary
        // for a restarting client: persisting them synchronously shrinks the
        // window in which a crash can lose the transcript tail from "the whole
        // turn" to "the current open blocks". Everything else keeps the
        // coalesced async checkpoint to stay off the streaming hot path.
        let terminal = Self::timeline_intent_is_terminal(&intent);
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if terminal {
            self.persist_timeline_sync(seed);
        } else {
            self.request_timeline_persistence(seed);
        }
        let _ = self.timeline_live.send(TimelineLiveEntry {
            seed: seed.to_string(),
            entry: entry.clone(),
        });
        Ok(entry)
    }

    /// 同步写入一个 seed 的 timeline 快照 + replay tail（daemon 优雅关闭或
    /// terminal intent 时调用）。从 pending 集合移除，避免异步线程重复写。
    pub(super) fn persist_timeline_sync(&self, seed: &str) {
        // Drop the pending flag so the async worker does not rewrite the same
        // seed again; the synchronous write below is strictly newer.
        if let Some(persistence) = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            persistence
                .pending_seeds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(seed);
        }
        let mut store_guard = self
            .timeline_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(store) = store_guard.as_mut() else {
            return;
        };
        // 先追加 timeline journal 尾部（journal ≥ cache 不变量），再写缓存。
        // journal 追加失败时跳过缓存写入（fail-closed），避免缓存比 journal 新
        // 导致崩溃重启后 journal 重放丢失尾部条目。
        if let Err(error) = append_timeline_journal_tail_locked(store, &self.timeline, seed) {
            log::error!(
                "[timeline] journal append failed for {seed}: {error}; skipping cache persist (fail-closed)"
            );
            return;
        }
        // IIFE：条件块中部复用 `?` 提前返回（clippy redundant_closure_call 豁免）
        #[allow(clippy::redundant_closure_call)]
        let Some((snapshot, journal)) = (|| {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.snapshot(seed).map(|snapshot| {
                let journal = timeline.replay_since(seed, 0);
                let journal = Self::prune_sealed_timeline_journal(&snapshot, journal);
                (snapshot, journal)
            })
        })() else {
            return;
        };
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }

    /// 同步落盘所有待写 seed（daemon 优雅关闭收尾；Drop 只 join 异步线程，
    /// 而 Arc 引用可能仍在 tokio task 中存活，必须显式 flush）。
    pub fn flush_timeline_persistence(&self) {
        let seeds: Vec<String> = self
            .timeline_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|persistence| {
                persistence
                    .pending_seeds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain()
                    .collect()
            })
            .unwrap_or_default();
        for seed in seeds {
            self.persist_timeline_sync(&seed);
        }
    }

    /// Ringing V1 bootstrap 的权威 transcript 快照。
    pub fn timeline_snapshot(&self, seed: &str) -> Option<TimelineSnapshot> {
        self.ensure_timeline_loaded(seed);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(seed)
    }

    /// Ringing V1 reconnect tail。调用方用 snapshot watermark 作为 after 参数。
    pub fn timeline_replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {
        self.ensure_timeline_loaded(seed);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replay_since(seed, watermark)
    }

    /// Live Ringing V1 timeline transcript feed. Reliability comes from `timeline_replay_since`
    /// and snapshot watermark; a lagged receiver must reconnect and replay.
    pub fn subscribe_timeline(&self) -> broadcast::Receiver<TimelineLiveEntry> {
        self.timeline_live.subscribe()
    }

    /// Terminal intents seal a block/round/turn — the client's recovery
    /// boundary. They are persisted synchronously so a crash between the seal
    /// and the next async checkpoint cannot drop a completed unit of work.
    pub(super) fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(
            intent,
            TimelineIntent::BlockSealed { .. }
                | TimelineIntent::RoundSealed { .. }
                | TimelineIntent::TurnSealed { .. }
        )
    }
}

/// 在 `timeline_store` 锁内追加该 seed 的 timeline journal 尾部（权威日志）。
///
/// 调用方必须已持有 `timeline_store` 锁（传入 `&mut Option<TimelineStore>`），
/// 从而与缓存文件的读写保持同一临界区：任何"追加 journal"与"写缓存"都不会
/// 交错，保证 journal ≥ cache 的不变量（崩溃后 journal 永不落后于缓存）。
/// 锁顺序（store → timeline）与 `persist_timeline_sync` / checkpoint 线程一致。
///
/// 返回 `Err` 表示 journal 追加失败（磁盘满/权限等）：调用方必须**跳过缓存
/// 写入**（fail-closed）——否则缓存会比 journal 新，崩溃重启后 journal 重放
/// 会丢失仅存在于缓存的尾部条目。
fn append_timeline_journal_tail_locked(
    store: &mut TimelineStore,
    timeline: &Arc<Mutex<TimelineAppender>>,
    seed: &str,
) -> std::io::Result<()> {
    let watermark = store.journal_watermark(seed);
    let entries = {
        let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
        timeline.replay_since(seed, watermark)
    };
    if !entries.is_empty() {
        store.append_journal(seed, &entries)?;
    }
    Ok(())
}
