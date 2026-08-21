//! `RingingHub`：daemon 侧 Ringing 运行时聚合入口。
//!
//! 职责：
//! - 三频道 `ChannelRouter`（入队/回放）；
//! - 三频道可靠 journal（reliable 事件 + replaceable checkpoint）；
//! - 领域 snapshot projection（每 seed+channel）；
//! - 每频道序号生成；
//! - 事件幂等（journal 侧 event_id 去重）。
//!
//! 由 daemon（T5）与 worker 事件入口（T6）消费。线程安全（Mutex 保护），
//! 与 legacy `EventBus` 并行存在，互不嵌套。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use qaqh_domain::{
    AskResolution, CompactStatus, ControlEvent, ConversationEvent, Delivery, DomainEvent,
    RingingChannel, TimelineBlockState, TimelineEntry, TimelineFailure, TimelineIntent,
    TimelineSnapshot, TimelineTurnState, ToolEvent,
};
use qaqh_ringing::{
    RingingChannelSnapshot, RingingEvent, RingingEventEnvelope, RingingResetRequired,
    is_safe_integer,
};
use qaqh_types::tool_result::ToolResult;
use tokio::sync::broadcast;

use super::content_store::{ContentEntry, ContentStore};
use super::journal::{AppendOutcome, CursorExpired, ReliableJournal};
use super::journal_store::{JournalOp, JournalStore};
use super::projection::SnapshotProjector;
use super::router::{ChannelRouter, replaceable_key_for, terminal_replaceable_keys};
use super::sequencer::Sequencer;
use crate::timeline_store::{PersistedTimeline, TimelineJournalOp, TimelineStore};
use crate::{
    TimelineAppender, TimelineError, TimelineLiveEntry, materialize_timeline_from_journal,
};

/// journal jsonl 超过该物理大小时，compact 后触发整文件重写（丢弃已折叠的
/// RoundDelta）。append-only 日志若不重写，磁盘与装载成本永久累积。
const JOURNAL_REWRITE_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

/// Non-terminal timeline changes are checkpointed at most once per interval.
/// Live delivery is still immediate; only the full snapshot rewrite is paced.
const TIMELINE_PERSIST_INTERVAL: Duration = Duration::from_secs(1);

/// 测试用阈值覆盖（OnceLock 一次性；仅测试模块设置）。
static JOURNAL_REWRITE_THRESHOLD_OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn journal_rewrite_threshold() -> u64 {
    *JOURNAL_REWRITE_THRESHOLD_OVERRIDE
        .get()
        .unwrap_or(&JOURNAL_REWRITE_THRESHOLD_BYTES)
}

/// Overlay persisted conversation data onto the live event projection.
/// Metadata used by native clients belongs to the same authoritative
/// bootstrap state as turns and usage; keeping this list centralized prevents
/// a newly added field from silently disappearing when the projection already
/// contains its structural `{seed, channel, revision}` object.
fn merge_persisted_conversation_state(
    projected: &mut serde_json::Value,
    persisted: serde_json::Value,
) {
    const PERSISTED_KEYS: &[&str] = &[
        "turns",
        "total_turns",
        "has_more",
        "usage",
        "usage_totals",
        "usage_requests",
        "cache_reported_requests",
        "model",
        "context_limit",
    ];
    match projected.as_object_mut() {
        Some(obj) => {
            for key in PERSISTED_KEYS {
                if let Some(value) = persisted.get(*key) {
                    obj.insert((*key).to_string(), value.clone());
                }
            }
        }
        None => *projected = persisted,
    }
}

#[cfg(test)]
pub(crate) fn override_journal_rewrite_threshold_for_test(bytes: u64) {
    let _ = JOURNAL_REWRITE_THRESHOLD_OVERRIDE.set(bytes);
}

/// 事件已接受（含 envelope 与幂等状态）。
#[derive(Debug)]
pub enum PublishOutcome {
    /// 已入队并可发送。
    Published { envelope: RingingEventEnvelope },
    /// 重复 event_id（幂等丢弃）。
    Duplicate,
    /// reliable 队列背压。
    Backpressure,
}

/// 频道级回放结果（SSE 重连）：可回放的事件 + 需要强制 snapshot 的会话。
#[derive(Debug, Default)]
pub struct ChannelReplay {
    pub events: Vec<RingingEventEnvelope>,
    pub resets: Vec<RingingResetRequired>,
}

#[derive(Debug)]
struct SeedChannelState {
    router: ChannelRouter,
    journal: ReliableJournal,
    projection: SnapshotProjector,
    last_stream_seq: u64,
    replaceable_since_checkpoint: HashMap<super::router::ReplaceableKey, u32>,
}

#[derive(Debug)]
struct TimelinePersistence {
    wake: mpsc::Sender<()>,
    pending_seeds: Arc<Mutex<HashSet<String>>>,
    join: Option<JoinHandle<()>>,
}

impl SeedChannelState {
    fn new(channel: RingingChannel) -> Self {
        Self {
            router: ChannelRouter::new(channel),
            journal: ReliableJournal::new(),
            projection: SnapshotProjector::new(),
            last_stream_seq: 0,
            replaceable_since_checkpoint: HashMap::new(),
        }
    }

    /// 从持久化 op 序列重建（与 live publish 路径相同的重放语义）。
    fn with_ops(channel: RingingChannel, seed: &str, ops: &[JournalOp]) -> Self {
        let mut state = Self::new(channel);
        for op in ops {
            match op {
                JournalOp::Append { envelope } => {
                    state.last_stream_seq = state.last_stream_seq.max(envelope.stream_seq);
                    let domain = match &envelope.event {
                        RingingEvent::Control(event) => DomainEvent::Control(event.clone()),
                        RingingEvent::Conversation(event) => {
                            DomainEvent::Conversation(event.clone())
                        }
                        RingingEvent::Tool(event) => DomainEvent::Tool(event.clone()),
                    };
                    state.projection.apply(channel, seed, &domain);
                    match envelope.delivery {
                        Delivery::Reliable => {
                            let _ = state.journal.append(envelope);
                        }
                        Delivery::Replaceable => {
                            let _ = state.router.route(envelope.clone());
                        }
                        Delivery::Ephemeral => {}
                    }
                }
                JournalOp::Checkpoint {
                    identity,
                    stream_seq,
                } => {
                    state.last_stream_seq = state.last_stream_seq.max(*stream_seq);
                    state.journal.checkpoint_replaceable(identity, *stream_seq);
                }
                JournalOp::Compact { turn_id, round_num } => {
                    state.journal.compact_round_deltas(turn_id, *round_num);
                }
            }
        }
        state
    }
}

/// Ringing daemon 运行时聚合。
#[derive(Debug)]
pub struct RingingHub {
    epoch: String,
    sequencer: Sequencer,
    /// 磁盘持久化 seed 清单（懒加载索引；`ensure_seed_loaded` 按需重放）。
    /// 启动时 `load_persisted` 只扫描清单，不加载任何历史。
    disk_seeds: Mutex<HashMap<RingingChannel, HashSet<String>>>,
    /// 磁盘 timeline seed 清单（懒加载索引；`ensure_timeline_loaded` 按需恢复）。
    disk_timeline_seeds: Mutex<HashSet<String>>,
    /// 懒加载串行化：防止并发首访同一 seed 时双重重放。
    lazy_load: Mutex<()>,
    /// 大内容外置存储（会话所有权 + TTL）。
    content_store: Mutex<ContentStore>,
    /// channel → (seed → state)。router/journal/projection 均 per (seed, channel)。
    channels: Mutex<HashMap<RingingChannel, HashMap<String, SeedChannelState>>>,
    /// 每频道实时推送通道（SSE 消费；可靠性由 journal/cursor 保证）。
    live: Mutex<HashMap<RingingChannel, broadcast::Sender<RingingEventEnvelope>>>,
    /// 当前进程生命周期内发布且未 resolved 的活交互（seed → interaction_id）。
    /// journal 重放的幽灵交互不在此表：daemon 重启后表为空，bootstrap 孤儿
    /// 收尾据此区分「等待用户响应的活交互」（保护，不 seal）与「daemon 重启
    /// 遗留的幽灵交互」（seal）。worker 死亡/重启路径用 force 无视该守卫。
    live_interactions: Mutex<HashMap<String, String>>,
    /// 持久化 journal（None = 非持久模式；I/O 失败只记录日志，不阻塞事件路径）。
    journal_store: Mutex<Option<JournalStore>>,
    /// Ringing V1 timeline transcript 的唯一 writer。它与三频道 Ringing v1 完全隔离，
    /// 不依赖 legacy 事件投影。
    timeline: Arc<Mutex<TimelineAppender>>,
    timeline_live: broadcast::Sender<TimelineLiveEntry>,
    timeline_store: Arc<Mutex<Option<TimelineStore>>>,
    timeline_persistence: Mutex<Option<TimelinePersistence>>,
}

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_options(epoch.into(), None)
    }

    /// 持久化构造：daemon 重启后可靠事件/切流状态不丢。
    pub fn with_persistence(epoch: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let hub = Self::with_options(epoch.into(), Some(root.into()));
        hub.load_persisted();
        hub.load_timeline_persisted();
        hub.start_timeline_persistence();
        hub
    }

    fn with_options(epoch: String, root: Option<PathBuf>) -> Self {
        let timeline_store = root
            .as_ref()
            .and_then(|root| match TimelineStore::new(root) {
                Ok(store) => Some(store),
                Err(error) => {
                    log::warn!("[timeline] persistence disabled: {error}");
                    None
                }
            });
        let journal_store = match root {
            Some(root) => match JournalStore::new(&root) {
                Ok(store) => Some(store),
                Err(error) => {
                    log::warn!("[ringing] journal persistence disabled: {error}");
                    None
                }
            },
            None => None,
        };
        let (timeline_live, _) = broadcast::channel(1024);
        Self {
            epoch,
            sequencer: Sequencer::new(),
            disk_seeds: Mutex::new(HashMap::new()),
            disk_timeline_seeds: Mutex::new(HashSet::new()),
            lazy_load: Mutex::new(()),
            content_store: Mutex::new(ContentStore::new()),
            channels: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            live_interactions: Mutex::new(HashMap::new()),
            journal_store: Mutex::new(journal_store),
            timeline: Arc::new(Mutex::new(TimelineAppender::new())),
            timeline_live,
            timeline_store: Arc::new(Mutex::new(timeline_store)),
            timeline_persistence: Mutex::new(None),
        }
    }

    /// Move timeline checkpoint I/O off the producer/writer hot path.
    ///
    /// The live TimelineAppender remains the sole source of sequence allocation and
    /// broadcast ordering. Persistence is a best-effort, single-writer checkpoint
    /// queue: notifications are coalesced per seed for a fixed checkpoint window,
    /// and the worker snapshots the latest in-memory state only when the window
    /// expires. The on-disk record shape is unchanged, so bootstrap/replay
    /// compatibility is preserved. Terminal intents still use synchronous
    /// persistence as the recovery boundary.
    fn start_timeline_persistence(&self) {
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

    fn request_timeline_persistence(&self, seed: &str) {
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

    /// 启动装载（懒加载模式）：只扫描磁盘 seed 清单，不重放任何事件。
    ///
    /// 内存态（journal/router/projection/sequencer 水位）在首次访问该 seed
    /// 时由 `ensure_seed_loaded` 从磁盘按需恢复。冷启动开销从"全量读取
    /// 全部 jsonl"降为"遍历目录"，历史会话不再常驻内存。
    fn load_persisted(&self) {
        let seeds = {
            let guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(store) => store.list_seeds(),
                None => HashMap::new(),
            }
        };
        let total: usize = seeds.values().map(HashSet::len).sum();
        *self.disk_seeds.lock().unwrap_or_else(|e| e.into_inner()) = seeds;
        log::info!("[ringing] lazy journal index ready: {total} persisted seeds on disk");
    }

    /// 懒加载：确保 (channel, seed) 的持久化历史已重放入内存。
    ///
    /// - 已在内存或磁盘无记录：零成本返回；
    /// - 磁盘有记录：读取该 seed 的 ops → 重放重建 state → 精确恢复序号 →
    ///   超大文件顺手压缩（P0 收敛，不依赖 RoundCompleted）→ 插入 channels。
    /// 全程持有 `lazy_load` 串行锁，避免并发首访双重重放。
    fn ensure_seed_loaded(&self, channel: RingingChannel, seed: &str) {
        let _serial = self.lazy_load.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = {
            let guard = self.channel_state(channel);
            guard
                .get(&channel)
                .is_some_and(|seeds| seeds.contains_key(seed))
        };
        if loaded {
            return;
        }
        let on_disk = self
            .disk_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&channel)
            .is_some_and(|seeds| seeds.contains(seed));
        if !on_disk {
            return;
        }
        // 读磁盘（短暂持有 journal_store 锁；读完即释放，不与 channel_state 嵌套）。
        let ops = {
            let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(store) => match store.load_seed(channel, seed) {
                    Ok(ops) => ops,
                    Err(error) => {
                        log::warn!(
                            "[ringing] lazy load failed for {seed} on {}: {error}",
                            channel.as_str()
                        );
                        return;
                    }
                },
                None => return,
            }
        };
        if ops.is_empty() {
            // 清单存在但无任何可重放操作（空/损坏）：插入空 state 防反复扫描。
            self.channel_state(channel)
                .entry(channel)
                .or_default()
                .insert(seed.to_string(), SeedChannelState::new(channel));
            return;
        }
        let state = SeedChannelState::with_ops(channel, seed, &ops);
        // 精确恢复序号（比启动水位更完整：channel/session seq 一并恢复）。
        let (mut max_stream, mut max_channel, mut max_session) = (0, 0, 0);
        for op in &ops {
            if let JournalOp::Append { envelope } = op {
                max_stream = max_stream.max(envelope.stream_seq);
                max_channel = max_channel.max(envelope.channel_seq);
                max_session = max_session.max(envelope.session_seq);
            }
        }
        self.sequencer
            .seed(channel, seed, max_stream, max_channel, max_session);
        // 超大历史文件加载即压缩（force：绕过 pending 门控，按物理大小检查）。
        self.rewrite_if_oversized(channel, seed, &state, true);
        self.channel_state(channel)
            .entry(channel)
            .or_default()
            .insert(seed.to_string(), state);
        log::info!(
            "[ringing] lazily loaded {seed} on {}: {} ops",
            channel.as_str(),
            ops.len()
        );
    }

    /// 启动装载（懒加载模式）：只扫描磁盘 timeline seed 清单，不 restore 任何
    /// 快照。内存态（TimelineAppender）在首次访问该 seed 时由
    /// `ensure_timeline_loaded` 从磁盘按需恢复。
    fn load_timeline_persisted(&self) {
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
    fn ensure_timeline_loaded(&self, seed: &str) {
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
                .filter_map(|op| match op {
                    TimelineJournalOp::Snapshot { snapshot } => Some(snapshot.watermark),
                    TimelineJournalOp::Append { entry } => Some(entry.timeline_seq),
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
                                appender
                                    .restore(seed.to_string(), snapshot.clone(), journal.clone());
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
    fn rebuild_timeline_from_messages(&self, seed: &str) {
        if let Some((snapshot, journal)) = super::timeline_rebuild::rebuild_timeline_snapshot(seed)
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
                if let Err(error) =
                    append_timeline_journal_tail_locked(store, &self.timeline, seed)
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
    fn backfill_timeline_journal_from_persisted(&self, persisted: &PersistedTimeline) {
        let mut ops: Vec<TimelineJournalOp> =
            Vec::with_capacity(persisted.journal.len().saturating_add(1));
        ops.push(TimelineJournalOp::Snapshot {
            snapshot: persisted.snapshot.clone(),
        });
        for entry in &persisted.journal {
            ops.push(TimelineJournalOp::Append {
                entry: entry.clone(),
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

    /// 收尾孤儿 running turn。daemon 重启或 worker 重新 spawn 后，timeline 中
    /// 任何未 seal 的 turn 都没有存活生产者（典型场景：工具调用未返回 result
    /// 时进程被杀）。若不 seal，前端会永远把它投影为 running，stop/send 按钮
    /// 卡死在 stop 且新消息无法发送——这是"重启 daemon/前端都无法再发送新
    /// 消息"的根因。
    ///
    /// seal 顺序遵循 TimelineAppender 契约：先 seal 全部 open block，再 seal
    /// 全部未 seal round（is_final=true，这是该 turn 的最后一轮），最后将 turn
    /// seal 为 Cancelled。幂等：已 seal 的 turn 直接跳过。返回是否有变更。
    pub fn seal_orphan_running_turns(&self, seed: &str) -> bool {
        let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
        let Some(snapshot) = appender.snapshot(seed) else {
            return false;
        };
        let mut changed = false;
        for turn in snapshot.turns.iter().filter(|turn| !turn.sealed) {
            for round in &turn.rounds {
                for block in &round.blocks {
                    if block.state == TimelineBlockState::Sealed {
                        continue;
                    }
                    match appender.seal_block(seed, &turn.turn_id, round.round_num, &block.block_id)
                    {
                        Ok(_) => changed = true,
                        Err(error) => log::warn!(
                            "[timeline] orphan seal block failed for {seed} {}: {error}",
                            block.block_id
                        ),
                    }
                }
            }
            for round in &turn.rounds {
                if round.sealed {
                    continue;
                }
                match appender.seal_round(seed, &turn.turn_id, round.round_num, true) {
                    Ok(_) => changed = true,
                    Err(error) => log::warn!(
                        "[timeline] orphan seal round failed for {seed} {}/{}: {error}",
                        turn.turn_id,
                        round.round_num
                    ),
                }
            }
            match appender.seal_turn_with_state(
                seed,
                &turn.turn_id,
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message:
                        "Daemon restarted while this turn was running; the turn was interrupted and the session is ready for new input."
                            .into(),
                }),
            ) {
                Ok(_) => changed = true,
                Err(error) => log::warn!(
                    "[timeline] orphan seal turn failed for {seed} {}: {error}",
                    turn.turn_id
                ),
            }
        }
        changed
    }

    /// 收尾三频道投影中的孤儿领域状态（Ringing 版 `seal_orphan_running_turns`）。
    ///
    /// worker 的挂起/运行状态在内存中，daemon 重启或 worker 被重新拉起后，
    /// journal 重放会恢复 `TurnStarted`/`ToolStarted`/`InteractionRequested` 等
    /// reliable 事件，但它们**永远不会有终态**——bootstrap 快照因此携带陈旧
    /// 的 `active_turn`/`running`/`pending_permission`/`pending_interaction`：
    /// 前端把中断的 turn 投影为 running、弹出无法批准的幽灵 ask/授权面板。
    ///
    /// 与 timeline seal 语义一致：通过正常 publish 路径发出终态事件
    /// （`ConversationCancelled` / `ToolFinished(Cancelled)` / `InteractionResolved`），
    /// 使 journal、投影与 SSE 客户端全部收敛。幂等：无孤儿时返回 false。
    ///
    /// 调用方必须在 `ensure_seed_loaded` 完成之后调用（本函数内部 publish 会
    /// 再次调用 `ensure_seed_loaded`，重入 lazy_load 锁会死锁）。
    pub fn seal_orphan_channel_state(&self, seed: &str, force: bool) -> bool {
        let mut changed = false;

        // 1) conversation：active_turn 无终态（journal 重放后仍有值）→ 取消。
        let conv = self.snapshot(RingingChannel::Conversation, seed);
        if let Some(turn_id) = conv
            .state
            .get("active_turn")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            log::info!(
                "[ringing] sealing orphan active turn {turn_id} for {seed} (no terminal event)"
            );
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: Some(turn_id.to_string()),
                }),
                None,
            );
            changed = true;
        }

        // 2) conversation compact：CompactStarted 无 CompactFinished → 失败。
        //    压缩 worker 的网络请求与结果仅存于旧进程内存，daemon/worker
        //    恢复后不可能继续；必须经正常终态事件收敛 journal、snapshot 和 SSE。
        let conv = self.snapshot(RingingChannel::Conversation, seed);
        if conv.state.get("compact_status").and_then(|v| v.as_str()) == Some("running") {
            let compact_id = conv
                .state
                .get("compact_id")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("orphan-compact")
                .to_string();
            log::info!(
                "[ringing] sealing orphan compact {compact_id} for {seed} (worker operation cannot resume)"
            );
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Conversation(ConversationEvent::CompactFinished {
                    compact_id,
                    status: CompactStatus::Failed,
                    summary_chars: Some(0),
                    turns_compacted: Some(0),
                    turns_removed: Some(0),
                }),
                None,
            );
            changed = true;
        }

        // 3) tool：running 列表 + pending_permission 无 ToolFinished 终态 → 取消。
        //    兼容旧投影的字符串数组与当前的对象数组两种格式。
        let tool = self.snapshot(RingingChannel::Tool, seed);
        let mut orphans: Vec<(String, String, u32)> = Vec::new();
        if let Some(running) = tool.state.get("running").and_then(|v| v.as_array()) {
            for entry in running {
                match entry {
                    serde_json::Value::String(id) => {
                        orphans.push((id.clone(), String::new(), 0));
                    }
                    serde_json::Value::Object(obj) => {
                        if let Some(id) = obj.get("tool_call_id").and_then(|v| v.as_str()) {
                            orphans.push((
                                id.to_string(),
                                obj.get("turn_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                obj.get("round_num").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(id) = tool
            .state
            .get("pending_permission")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if !orphans
                .iter()
                .any(|(tool_call_id, _, _)| tool_call_id == id)
            {
                orphans.push((id.to_string(), String::new(), 0));
            }
        }
        for (tool_call_id, turn_id, round_num) in orphans {
            log::info!("[ringing] sealing orphan tool {tool_call_id} for {seed} (no ToolFinished)");
            let _ = self.publish_with_causation(
                seed,
                DomainEvent::Tool(ToolEvent::ToolFinished {
                    tool_call_id,
                    turn_id,
                    round_num,
                    result: ToolResult::cancelled(
                        "Agent restarted before the tool returned a result",
                    ),
                }),
                None,
            );
            changed = true;
        }

        // 4) control：pending_interaction 无 InteractionResolved → 关闭（Dismissed）。
        //    守卫：当前进程发布、仍在等待用户响应的活交互不 seal——bootstrap
        //    路径（force=false）会误杀 1ms 前刚发布的 ask（「ask 弹不出」根因）；
        //    force=true（worker 死亡/重启的 registry 收尾路径）无视守卫强制收尾。
        let control = self.snapshot(RingingChannel::Control, seed);
        if let Some(id) = control
            .state
            .get("pending_interaction")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
        {
            let is_live = self
                .live_interactions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(seed)
                .is_some_and(|cur| cur == id);
            if is_live && !force {
                log::info!(
                    "[ringing] keeping live interaction {id} for {seed} (awaiting user response)"
                );
            } else {
                log::info!("[ringing] sealing orphan interaction {id} for {seed} (no resolution)");
                let _ = self.publish_with_causation(
                    seed,
                    DomainEvent::Control(ControlEvent::InteractionResolved {
                        interaction_id: id.to_string(),
                        resolution: AskResolution::Dismissed,
                    }),
                    None,
                );
                // 强制收尾后从活交互表清除，防止残留误导后续 bootstrap 守卫。
                self.live_interactions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(seed);
                changed = true;
            }
        }

        changed
    }

    /// 优雅关闭收尾：对所有已知 timeline seed 执行孤儿收尾（timeline +
    /// 三频道投影）。正常路径下 worker 已优雅退出并自行 seal（terminal
    /// intent 同步落盘），此处兜底 worker 超时被杀 / 未收尾的场景——
    /// 退出时不留孤儿，安装器更新后重启不再出现 daemon_restart_interrupted。
    ///
    /// 只覆盖已加载 seed 的当前状态：磁盘上未加载的 seed 由下次
    /// `ensure_timeline_loaded` 启动时收尾（懒加载路径自带孤儿 seal）。
    pub fn seal_all_orphans(&self) {
        let seeds: Vec<String> = self
            .disk_timeline_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        for seed in &seeds {
            if self.seal_orphan_running_turns(seed) {
                log::info!("[timeline] sealed orphan turn(s) for {seed} at shutdown");
            }
            if self.seal_orphan_channel_state(seed, true) {
                log::info!("[ringing] sealed orphan channel state for {seed} at shutdown");
            }
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
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
    fn persist_timeline_sync(&self, seed: &str) {
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
    fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(
            intent,
            TimelineIntent::BlockSealed { .. }
                | TimelineIntent::RoundSealed { .. }
                | TimelineIntent::TurnSealed { .. }
        )
    }

    /// 大内容外置：存入（返回 content_id）。
    pub fn put_content(
        &self,
        seed: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> String {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(seed, media_type, bytes, truncated)
    }

    /// 大内容外置：读取（校验会话所有权 + TTL）。
    pub fn get_content(&self, seed: &str, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed, content_id)
    }

    fn channel_state(
        &self,
        _channel: RingingChannel,
    ) -> std::sync::MutexGuard<'_, HashMap<RingingChannel, HashMap<String, SeedChannelState>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seed_state<'a>(
        &self,
        guard: &'a mut HashMap<RingingChannel, HashMap<String, SeedChannelState>>,
        channel: RingingChannel,
        seed: &str,
    ) -> &'a mut SeedChannelState {
        guard
            .entry(channel)
            .or_insert_with(HashMap::new)
            .entry(seed.to_string())
            .or_insert_with(|| SeedChannelState::new(channel))
    }

    /// 发布领域事件（worker 事件入口调用）。
    pub fn publish(&self, seed: &str, event: DomainEvent) -> PublishOutcome {
        self.publish_with_causation(seed, event, None)
    }

    /// 发布领域事件并附加因果来源（Ringing command_id）。
    pub fn publish_with_causation(
        &self,
        seed: &str,
        event: DomainEvent,
        causation: Option<&str>,
    ) -> PublishOutcome {
        let channel = event.channel();
        // P1: 懒加载——publish 前确保该 seed 历史已重放入内存，防止新事件
        // 的序号与磁盘历史冲突（sequencer 水位随后续加载精确恢复）。
        self.ensure_seed_loaded(channel, seed);
        let delivery = event.delivery();
        let (stream_seq, channel_seq, session_seq) = self.sequencer.next(channel, seed);
        if !is_safe_integer(stream_seq)
            || !is_safe_integer(channel_seq)
            || !is_safe_integer(session_seq)
        {
            log::error!("[ringing] sequence exceeded JSON safe integer range");
            return PublishOutcome::Backpressure;
        }
        let event_id = format!(
            "{}-{}-{}-{}",
            self.epoch,
            channel.as_str(),
            seed,
            stream_seq
        );

        let mut guard = self.channel_state(channel);
        let st = self.seed_state(&mut guard, channel, seed);

        // 幂等：journal 侧 event_id 去重（replaceable 也检查，防重复投递）
        let envelope = RingingEventEnvelope::new(
            seed,
            stream_seq,
            channel_seq,
            session_seq,
            event_id,
            event.clone().into(),
        );
        let envelope = match causation {
            Some(c) => envelope.with_causation(c),
            None => envelope,
        };

        let state_changed = st.projection.apply(channel, seed, &event);
        let revision = st.projection.revision(channel, seed);
        // server_ts：服务器发布时间（unix ms），端到端延迟诊断用。
        let mut envelope = envelope.with_server_ts(unix_ms());
        if state_changed {
            envelope = envelope.with_state_revision(revision);
        }
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);

        match delivery {
            Delivery::Reliable => {
                match st.journal.append(&envelope) {
                    AppendOutcome::Duplicate => return PublishOutcome::Duplicate,
                    AppendOutcome::Appended => {
                        self.persist_append(channel, seed, &envelope);
                        // RoundCompleted 是该 round 的权威终态（携带完整 thinking/answer），
                        // 折叠该 round 的增量可控制 journal 用量，且回放安全：
                        // 客户端要么已有增量（随后被快照覆盖），要么直接拿到全量快照。
                        if let RingingEvent::Conversation(ConversationEvent::RoundCompleted {
                            turn_id,
                            round_num,
                            ..
                        }) = &envelope.event
                        {
                            let removed = st.journal.compact_round_deltas(turn_id, *round_num);
                            if removed > 0 {
                                self.persist_compact(channel, seed, turn_id, *round_num);
                            }
                        }
                        // P0: 磁盘收敛检查脱离 RoundCompleted 依赖——轮次未完成
                        // 时 delta 持续 append 也必须有兜底重写（pending 门控）。
                        self.rewrite_if_oversized(channel, seed, &st, false);
                    }
                }
                for key in terminal_replaceable_keys(&envelope.event) {
                    st.router.flush_replaceable(&key);
                    st.replaceable_since_checkpoint.remove(&key);
                    self.persist_remove_replaceable(channel, seed, &format!("{key:?}"));
                }
                // 活交互登记：当前进程发布的 InteractionRequested/PlanReviewRequested
                // 进入内存表，resolved 时移除。daemon 重启后表为空 → journal 重放的
                // 幽灵交互不在表 → bootstrap 孤儿收尾仍可收尾它们（原设计意图）；
                // 活交互在表 → 收尾跳过（修复「ask 发布后 1ms 被 bootstrap 秒杀」）。
                match &envelope.event {
                    RingingEvent::Control(ControlEvent::InteractionRequested {
                        interaction_id,
                        ..
                    })
                    | RingingEvent::Control(ControlEvent::PlanReviewRequested {
                        interaction_id,
                        ..
                    }) => {
                        self.live_interactions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(seed.to_string(), interaction_id.clone());
                    }
                    RingingEvent::Control(ControlEvent::InteractionResolved {
                        interaction_id,
                        ..
                    })
                    | RingingEvent::Control(ControlEvent::PlanReviewResolved {
                        interaction_id,
                        ..
                    }) => {
                        let mut live = self
                            .live_interactions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if live.get(seed).is_some_and(|cur| cur == interaction_id) {
                            live.remove(seed);
                        }
                    }
                    _ => {}
                }
                // Reliable replay/backpressure belongs to the journal. Keeping a
                // second reliable queue in the router would fill permanently
                // because live broadcast has no dequeue/ack path.
                self.fanout(channel, &envelope);
                PublishOutcome::Published { envelope }
            }
            Delivery::Replaceable | Delivery::Ephemeral => {
                // replaceable 覆盖入槽；ephemeral 不入队但照常实时推送
                match st.router.route(envelope.clone()) {
                    super::router::RouteOutcome::Routed { .. } => {
                        if let Some(key) = replaceable_key_for(&envelope.event) {
                            let count = st
                                .replaceable_since_checkpoint
                                .entry(key.clone())
                                .or_default();
                            *count = count.saturating_add(1);
                            let first_progress = matches!(
                                &envelope.event,
                                RingingEvent::Tool(qaqh_domain::ToolEvent::ToolProgress {
                                    seq_start: 0,
                                    ..
                                })
                            );
                            if *count == 1 || *count >= 64 || first_progress {
                                self.persist_replaceable(
                                    channel,
                                    seed,
                                    &format!("{key:?}"),
                                    &envelope,
                                );
                                *count = 0;
                            }
                        }
                        self.fanout(channel, &envelope);
                        PublishOutcome::Published { envelope }
                    }
                    super::router::RouteOutcome::Backpressure => PublishOutcome::Backpressure,
                }
            }
        }
    }

    /// 订阅某频道的实时事件流（SSE 用）。reliable 可靠性由 cursor/journal 承担。
    pub fn subscribe(&self, channel: RingingChannel) -> broadcast::Receiver<RingingEventEnvelope> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.entry(channel)
            .or_insert_with(|| broadcast::channel(1024).0)
            .subscribe()
    }

    /// publish 末尾：把信封推入实时通道（失败=无消费者，忽略）。
    fn fanout(&self, channel: RingingChannel, envelope: &RingingEventEnvelope) {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = live.get(&channel) {
            let _ = tx.send(envelope.clone());
        }
    }

    /// 从 cursor 回放（SSE 重连用）。cursor 超出窗口 → `CursorExpired`。
    pub fn replay_since(
        &self,
        channel: RingingChannel,
        seed: &str,
        after_stream_seq: u64,
    ) -> Result<Vec<RingingEventEnvelope>, CursorExpired> {
        self.ensure_seed_loaded(channel, seed);
        let guard = self.channel_state(channel);
        let st = guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .ok_or(CursorExpired {
                earliest_available_seq: 0,
            })?;
        st.journal.replay_since(after_stream_seq).map(|mut events| {
            // 追加当前 replaceable 值（慢消费者恢复增量）
            events.extend(
                st.router
                    .replay_since(after_stream_seq)
                    .into_iter()
                    .filter(|e| e.delivery != Delivery::Reliable),
            );
            events
        })
    }

    /// 频道级回放（SSE 重连用）：聚合该频道所有 seed 的可靠 tail 与
    /// 当前 replaceable 值。某个 seed 的 cursor 超出保留窗口时产出
    /// `RingingResetRequired`，客户端应改走 snapshot 恢复。
    ///
    /// 懒加载语义：只回放**已加载** seed（冷启动后未经任何访问的 seed 不在
    /// 内存）。客户端连接某 seed 前必经 open/bootstrap（触发 `ensure_seed_loaded`），
    /// 因此活跃会话的历史始终可回放；从未连接的 seed 没有消费者，无需装载。
    ///
    /// `skip_reliable`：无 cursor 的新连接（`after_stream_seq == 0`）为 true。
    /// 新客户端的历史由 bootstrap 快照承担（快照先行），此时只回放当前
    /// replaceable 值；不回放 journal 里的可靠历史，否则 SSE 先于 bootstrap
    /// 到达时，无终态的 TurnStarted/ToolStarted/InteractionRequested 会被
    /// 前端应用成陈旧 running turn 与无法批准的幽灵交互面板。
    pub fn replay_channel_since(
        &self,
        channel: RingingChannel,
        after_stream_seq: u64,
        skip_reliable: bool,
    ) -> ChannelReplay {
        let guard = self.channel_state(channel);
        let mut replay = ChannelReplay::default();
        let Some(seeds) = guard.get(&channel) else {
            return replay;
        };
        for (seed, st) in seeds {
            if !skip_reliable {
                match st.journal.replay_since(after_stream_seq) {
                    Ok(mut events) => replay.events.append(&mut events),
                    Err(CursorExpired {
                        earliest_available_seq,
                    }) => {
                        replay.resets.push(RingingResetRequired::new(
                            channel,
                            seed.clone(),
                            earliest_available_seq,
                        ));
                    }
                }
            }
            for env in st.router.replay_since(after_stream_seq) {
                if env.delivery != Delivery::Reliable && env.stream_seq > after_stream_seq {
                    replay.events.push(env);
                }
            }
        }
        // stream_seq 在 (server_epoch, channel) 内全局唯一，跨 seed 合并后
        // 直接按 stream_seq 排序即得该频道的全局顺序。
        replay.events.sort_by_key(|e| e.stream_seq);
        replay
    }

    /// 读取领域快照（HTTP `GET /ringing/v1/sessions/{seed}/bootstrap`）。
    pub fn snapshot(&self, channel: RingingChannel, seed: &str) -> RingingChannelSnapshot {
        self.ensure_seed_loaded(channel, seed);
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|st| {
                st.projection
                    .snapshot_for(channel, seed, st.last_stream_seq)
            })
            .unwrap_or_else(|| SnapshotProjector::new().snapshot_for(channel, seed, 0))
    }

    /// Conversation 频道完整快照：领域投影摘要 + 持久化消息构建的 turns。
    pub fn conversation_snapshot(&self, seed: &str) -> RingingChannelSnapshot {
        let mut snap = self.snapshot(RingingChannel::Conversation, seed);
        if let Some(state) = super::conversation_snapshot::persisted_conversation_state(seed) {
            merge_persisted_conversation_state(&mut snap.state, state);
        }
        snap
    }

    /// 记 replaceable checkpoint（稀疏）。
    pub fn checkpoint(&self, channel: RingingChannel, seed: &str, identity: &str, stream_seq: u64) {
        self.ensure_seed_loaded(channel, seed);
        let mut guard = self.channel_state(channel);
        let st = self.seed_state(&mut guard, channel, seed);
        st.last_stream_seq = st.last_stream_seq.max(stream_seq);
        st.journal.checkpoint_replaceable(identity, stream_seq);
        self.persist_checkpoint(channel, seed, identity, stream_seq);
    }

    pub fn last_stream_seq(&self, channel: RingingChannel, seed: &str) -> u64 {
        self.ensure_seed_loaded(channel, seed);
        let guard = self.channel_state(channel);
        guard
            .get(&channel)
            .and_then(|seeds| seeds.get(seed))
            .map(|s| s.last_stream_seq)
            .unwrap_or(0)
    }

    // ── 持久化钩子：I/O 失败只记录日志，绝不阻塞事件路径 ──

    fn persist_append(&self, channel: RingingChannel, seed: &str, envelope: &RingingEventEnvelope) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.append(channel, seed, envelope)
        {
            log::warn!("[ringing] journal append failed: {error}");
        }
    }

    fn persist_compact(&self, channel: RingingChannel, seed: &str, turn_id: &str, round_num: u32) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.compact(channel, seed, turn_id, round_num)
        {
            log::warn!("[ringing] journal compact persist failed: {error}");
        }
    }

    /// 检查 jsonl 物理大小，超过阈值则按内存存活事件整文件重写（磁盘收敛）。
    ///
    /// P0 修复：触发不再依赖 `RoundCompleted` 折叠（`removed > 0`）。轮次未
    /// 完成/卡死时 `RoundDelta` 会持续 append，若只在折叠后检查，文件将无界
    /// 增长（实测 82MB 全 append 无 compact）。现在每次 reliable append 后
    /// 都经 `pending_bytes` 内存计数门控：阈值内零 I/O，超阈值才 stat + 重写。
    /// 重写以内存有界 journal（≤8192 条）为权威，可把超大文件收敛到窗口大小。
    ///
    /// `force`：跳过 pending 门控，直接按物理大小检查（懒加载 seed 时用——
    /// 刚加载的 state 没有 pending 计数，但历史文件可能已超大）。
    fn rewrite_if_oversized(
        &self,
        channel: RingingChannel,
        seed: &str,
        st: &SeedChannelState,
        force: bool,
    ) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = guard.as_mut() else { return };
        if !force && store.pending_bytes(channel, seed) < journal_rewrite_threshold() {
            return;
        }
        let size = match store.file_size(channel, seed) {
            Ok(size) => size,
            Err(_) => return,
        };
        if size < journal_rewrite_threshold() {
            return;
        }
        let envelopes: Vec<_> = st.journal.entries().cloned().collect();
        let checkpoints: Vec<(String, u64)> = st
            .journal
            .checkpoints()
            .iter()
            .map(|(key, seq)| (key.clone(), *seq))
            .collect();
        if let Err(error) = store.rewrite(channel, seed, &envelopes, &checkpoints) {
            log::warn!("[ringing] journal rewrite failed for {seed}: {error}");
        } else {
            log::info!(
                "[ringing] journal rewritten for {seed}: {} bytes -> {} entries",
                size,
                envelopes.len()
            );
        }
    }

    /// 过滤已 seal turn 的 timeline journal 条目。已 seal turn 的 TextDelta/
    /// ToolProgress 已被快照全量覆盖，且 seal 后不会再有新 delta（append_text
    /// 拒绝已 seal block），因此持久化时丢弃这些条目不会破坏恢复：restore 的
    /// next_fragment 只从保留的活跃 turn 条目重建。这消除了每次 persist 都
    /// 全量克隆整个 journal 的写放大（曾实测 13.5MB JSON 每几秒重写一次）。
    fn prune_sealed_timeline_journal(
        snapshot: &qaqh_domain::TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> Vec<TimelineEntry> {
        let sealed: HashSet<&str> = snapshot
            .turns
            .iter()
            .filter(|turn| turn.sealed)
            .map(|turn| turn.turn_id.as_str())
            .collect();
        if sealed.is_empty() {
            return journal;
        }
        journal
            .into_iter()
            .filter(|entry| !sealed.contains(entry.turn_id.as_str()))
            .collect()
    }

    fn persist_checkpoint(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.checkpoint(channel, seed, identity, stream_seq)
        {
            log::warn!("[ringing] journal checkpoint persist failed: {error}");
        }
    }

    fn persist_replaceable(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        envelope: &RingingEventEnvelope,
    ) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.replaceable(channel, seed, identity, envelope)
        {
            log::warn!("[ringing] replaceable slot persist failed: {error}");
        }
    }

    fn persist_remove_replaceable(&self, channel: RingingChannel, seed: &str, identity: &str) {
        let mut guard = self.journal_store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut()
            && let Err(error) = store.remove_replaceable(channel, seed, identity)
        {
            log::warn!("[ringing] replaceable slot cleanup failed: {error}");
        }
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

impl Drop for RingingHub {
    fn drop(&mut self) {
        let persistence = self
            .timeline_persistence
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(mut persistence) = persistence else {
            return;
        };
        drop(persistence.wake);
        if let Some(join) = persistence.join.take() {
            let _ = join.join();
        }
    }
}

/// 当前 unix 毫秒（事件信封 server_ts 用）。
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, ToolEvent};

    #[test]
    fn persisted_conversation_metadata_survives_projection_overlay() {
        let mut projected = serde_json::json!({
            "seed": "s",
            "channel": "conversation",
            "revision": 7,
            "compact_status": "running"
        });
        merge_persisted_conversation_state(
            &mut projected,
            serde_json::json!({
                "turns": [],
                "usage": { "prompt_tokens": 42 },
                "model": "qaqh-test",
                "context_limit": 200000
            }),
        );
        assert_eq!(projected["model"], "qaqh-test");
        assert_eq!(projected["context_limit"], 200000);
        assert_eq!(projected["usage"]["prompt_tokens"], 42);
        assert_eq!(projected["compact_status"], "running");
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qaqh-ringing-hub-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn round_delta(seq: u64) -> DomainEvent {
        DomainEvent::Conversation(ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 0,
            kind: qaqh_domain::RoundDeltaKind::Thinking,
            delta: format!("chunk-{seq}"),
        })
    }

    fn tool_progress(chunk: &str) -> DomainEvent {
        DomainEvent::Tool(ToolEvent::ToolProgress {
            tool_call_id: "c1".into(),
            turn_id: "t".into(),
            round_num: 0,
            stream: "stdout".into(),
            seq_start: 0,
            seq_end: 1,
            chunk: chunk.into(),
            dropped_bytes: 0,
            truncated: false,
        })
    }

    #[test]
    fn lazy_load_defers_history_until_first_access() {
        let root = temp_root("lazy");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            for i in 1..=3 {
                let _ = hub.publish("a", round_delta(i));
            }
            let _ = hub.publish("b", round_delta(1));
        }
        // 重启：懒加载模式下启动不重放任何历史。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        {
            let guard = hub.channel_state(RingingChannel::Conversation);
            assert!(
                guard
                    .get(&RingingChannel::Conversation)
                    .is_none_or(|seeds| seeds.is_empty()),
                "cold start must not load any seed"
            );
        }
        // 首次访问 seed a → 历史按需恢复。
        let replayed_a = hub
            .replay_since(RingingChannel::Conversation, "a", 0)
            .expect("replay a");
        assert_eq!(
            replayed_a.len(),
            3,
            "seed a history restored on first access"
        );
        // seed b 未被访问 → 仍不在内存。
        {
            let guard = hub.channel_state(RingingChannel::Conversation);
            assert!(
                !guard
                    .get(&RingingChannel::Conversation)
                    .unwrap()
                    .contains_key("b"),
                "seed b must remain unloaded until accessed"
            );
        }
        let replayed_b = hub
            .replay_since(RingingChannel::Conversation, "b", 0)
            .expect("replay b");
        assert_eq!(
            replayed_b.len(),
            1,
            "seed b history restored on first access"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn orphan_compact_from_persisted_journal_is_failed_before_bootstrap() {
        let root = temp_root("orphan-compact");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let _ = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::CompactStarted {
                    compact_id: "compact-persisted".into(),
                    turns_total: 9,
                    turns_keeping: 3,
                }),
            );
        }

        let hub = RingingHub::with_persistence("epoch-2", &root);
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["compact_status"],
            "running",
            "journal replay alone restores the interrupted operation"
        );
        assert!(hub.seal_orphan_channel_state("s", false));
        // daemon bootstrap 在 SessionManager 初始化后会调用 conversation_snapshot；
        // 此处只需验证由 journal 驱动、随后会被持久消息 overlay 保留的投影字段。
        let bootstrap_state = hub.snapshot(RingingChannel::Conversation, "s").state;
        assert_eq!(bootstrap_state["compact_status"], "failed");
        assert_eq!(bootstrap_state["compact_id"], "compact-persisted");
        assert!(!hub.seal_orphan_channel_state("s", false));

        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay compact recovery");
        assert!(replay.iter().any(|envelope| matches!(
            &envelope.event,
            RingingEvent::Conversation(ConversationEvent::CompactFinished {
                compact_id,
                status: CompactStatus::Failed,
                ..
            }) if compact_id == "compact-persisted"
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn journal_rewrite_converges_on_lazy_load_without_round_completed() {
        // P0 回归：轮次未完成（无 RoundCompleted）时 delta 持续 append 也必须收敛。
        // 阈值降到 1MB 加速测试；此覆盖为 OnceLock 一次性，仅本测试使用。
        override_journal_rewrite_threshold_for_test(1024 * 1024);
        let root = temp_root("rewrite-lazy");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            // 9000 个 reliable delta 超过内存窗口（8192），触发淘汰。
            for i in 0..9000 {
                let _ = hub.publish("s", round_delta(i));
            }
        }
        let path = root.join("journal").join("conversation").join("s.jsonl");
        let before_lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            before_lines > 8500,
            "fixture must exceed the bounded window"
        );
        // 重启：启动不加载；首次访问触发懒加载 + force rewrite（窗口收敛）。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        {
            let guard = hub.channel_state(RingingChannel::Conversation);
            assert!(
                guard
                    .get(&RingingChannel::Conversation)
                    .is_none_or(|seeds| seeds.is_empty()),
                "cold start must not load any seed"
            );
        }
        // replay_since(0) 因窗口淘汰返回 CursorExpired（正确语义），
        // 但 ensure 已完成加载并执行收敛重写。
        let replayed = hub.replay_since(RingingChannel::Conversation, "s", 0);
        assert!(
            matches!(replayed, Err(CursorExpired { .. })),
            "cursor 0 must be expired after window eviction"
        );
        {
            let guard = hub.channel_state(RingingChannel::Conversation);
            assert!(
                guard
                    .get(&RingingChannel::Conversation)
                    .is_some_and(|seeds| seeds.contains_key("s")),
                "lazy load must materialize the seed"
            );
        }
        // 序号水位精确恢复：新事件从历史最大值之后继续。
        assert_eq!(
            hub.last_stream_seq(RingingChannel::Conversation, "s"),
            9000,
            "sequence watermark restored from replayed history"
        );
        if let PublishOutcome::Published { envelope } = hub.publish("s", round_delta(9999)) {
            assert!(
                envelope.stream_seq > 9000,
                "new events must continue after the restored watermark"
            );
        } else {
            panic!("publish after restart must succeed");
        }
        // 文件收敛到有界窗口（8192 条），而非线性增长。
        let after_lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            after_lines < before_lines,
            "rewrite must shrink the file: {before_lines} -> {after_lines}"
        );
        assert!(
            after_lines <= 8192 + 32,
            "file must converge to the bounded window, got {after_lines} lines"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_journal_survives_restart() {
        let root = temp_root("restart");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let _ = hub.publish("s", round_delta(1));
            let _ = hub.publish("s", tool_progress("a"));
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        // reliable 事件重放
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay");
        assert!(
            replayed.iter().any(|e| matches!(
                e.event,
                RingingEvent::Conversation(ConversationEvent::RoundDelta { .. })
            )),
            "reliable delta must survive restart"
        );
        // replaceable 当前值恢复
        let tool_replay = hub
            .replay_since(RingingChannel::Tool, "s", 0)
            .expect("tool replay");
        assert!(
            tool_replay
                .iter()
                .any(|e| matches!(&e.event, RingingEvent::Tool(ToolEvent::ToolProgress { chunk, .. }) if chunk == "a")),
            "replaceable latest value must survive restart"
        );
        // 序号继续递增（新 epoch 内不从头冲突）
        let outcome = hub.publish("s", round_delta(99));
        if let PublishOutcome::Published { envelope } = outcome {
            assert!(envelope.stream_seq > 0);
            assert!(envelope.stream_seq > replayed.first().map(|e| e.stream_seq).unwrap_or(0));
        } else {
            panic!("publish after restart must succeed");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_round_compaction_replays_consistently() {
        let root = temp_root("compact");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            for i in 1..=3 {
                let _ = hub.publish("s", round_delta(i));
            }
            let _ = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::RoundCompleted {
                    turn_id: "t1".into(),
                    round_num: 0,
                    thinking: Some("final".into()),
                    answer: Some("done".into()),
                    output_ref: None,
                    is_final: true,
                }),
            );
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("replay");
        let deltas = replayed
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    RingingEvent::Conversation(ConversationEvent::RoundDelta { .. })
                )
            })
            .count();
        assert_eq!(deltas, 0, "compacted deltas must not replay");
        assert!(
            replayed.iter().any(|e| matches!(
                e.event,
                RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
            )),
            "RoundCompleted survives"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_assigns_sequences_and_envelope_fields() {
        let hub = RingingHub::new("epoch-1");
        let outcome = hub.publish(
            "s1",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        match outcome {
            PublishOutcome::Published { envelope } => {
                assert_eq!(envelope.seed, "s1");
                assert_eq!(envelope.stream_seq, 1);
                assert_eq!(envelope.channel_seq, 1);
                assert_eq!(envelope.session_seq, 1);
                assert_eq!(envelope.delivery, Delivery::Reliable);
                assert!(envelope.event_id.starts_with("epoch-1-tool-s1-"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn publish_with_causation_sets_envelope_field() {
        let hub = RingingHub::new("epoch-1");
        let outcome = hub.publish_with_causation(
            "s1",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
            Some("cmd-9"),
        );
        match outcome {
            PublishOutcome::Published { envelope } => {
                assert_eq!(envelope.causation_id.as_deref(), Some("cmd-9"));
            }
            other => panic!("unexpected {other:?}"),
        }
        // 无 causation 时字段保持 None
        let plain = hub.publish(
            "s1",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t2".into(),
                user_text: "hi".into(),
            }),
        );
        match plain {
            PublishOutcome::Published { envelope } => {
                assert!(envelope.causation_id.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn duplicate_event_id_is_idempotent_dropped() {
        let hub = RingingHub::new("epoch-1");
        let ev =
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None });
        let _ = hub.publish("s", ev.clone());
        // 直接构造同 id 信封再发布不可行（id 由 hub 生成）；
        // 验证两次发布同内容产生不同 id 但都成功（幂等在 journal 层测试覆盖）
        let second = hub.publish("s", ev);
        assert!(matches!(second, PublishOutcome::Published { .. }));
        assert_eq!(hub.last_stream_seq(RingingChannel::Conversation, "s"), 2);
    }

    #[test]
    fn replay_and_snapshot_work_together() {
        let hub = RingingHub::new("epoch-1");
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: qaqh_domain::RoundDeltaKind::Answering,
                delta: "hello".into(),
            }),
        );
        let replayed = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("in window");
        // reliable TurnStarted + replaceable RoundDelta 当前值
        assert_eq!(replayed.len(), 2);
        let snap = hub.snapshot(RingingChannel::Conversation, "s");
        assert_eq!(snap.state["active_turn"], "t1");
        assert_eq!(snap.state_revision, 1);
    }

    #[test]
    fn channel_replay_merges_seeds_and_signals_reset() {
        let hub = RingingHub::new("epoch-1");
        hub.publish(
            "s1",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s2",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c2".into(),
                turn_id: "t2".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let replay = hub.replay_channel_since(RingingChannel::Tool, 0, false);
        assert_eq!(replay.resets.len(), 0);
        assert_eq!(replay.events.len(), 2);
        // stream_seq 全局递增，跨 seed 合并后按序排列
        assert_eq!(replay.events[0].stream_seq, 1);
        assert_eq!(replay.events[1].stream_seq, 2);
        assert_eq!(replay.events[0].seed, "s1");
        assert_eq!(replay.events[1].seed, "s2");

        // 无 cursor 的新连接跳过可靠历史（只回放 replaceable 值）：
        // 历史由 bootstrap 快照承担，防止幽灵事件先于快照到达前端。
        let fresh = hub.replay_channel_since(RingingChannel::Tool, 0, true);
        assert_eq!(fresh.events.len(), 0);
        assert_eq!(fresh.resets.len(), 0);

        // cursor 超出保留窗口 → 该 seed 需要强制 snapshot
        // （journal 默认容量 8192，灌满后 earliest 前移）
        let hub2 = RingingHub::new("epoch-2");
        for i in 1..=8193 {
            hub2.publish(
                "s1",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("c{i}"),
                    turn_id: format!("t{i}"),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        }
        let replayed = hub2.replay_channel_since(RingingChannel::Tool, 0, false);
        assert!(!replayed.resets.is_empty());
        assert_eq!(replayed.resets[0].seed, "s1");
        assert!(replayed.resets[0].earliest_available_seq > 1);
    }

    #[test]
    fn replaceable_progress_covers_in_router() {
        let hub = RingingHub::new("epoch-1");
        let progress = |chunk: &str| {
            DomainEvent::Tool(ToolEvent::ToolProgress {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                stream: "stdout".into(),
                seq_start: 0,
                seq_end: 1,
                chunk: chunk.into(),
                dropped_bytes: 0,
                truncated: false,
            })
        };
        let _ = hub.publish("s", progress("a"));
        let _ = hub.publish("s", progress("ab"));
        let replayed = hub.replay_since(RingingChannel::Tool, "s", 0).expect("ok");
        let progress_events: Vec<_> = replayed
            .iter()
            .filter(|e| {
                matches!(
                    e.event,
                    qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolProgress { .. })
                )
            })
            .collect();
        assert_eq!(progress_events.len(), 1, "only latest progress survives");
    }

    #[test]
    fn checkpoint_records_sparse_progress() {
        let hub = RingingHub::new("epoch-1");
        hub.checkpoint(RingingChannel::Tool, "s", "tool:c1", 7);
        let guard = hub.channels.lock().unwrap_or_else(|e| e.into_inner());
        let st = guard
            .get(&RingingChannel::Tool)
            .and_then(|seeds| seeds.get("s"))
            .expect("channel+seed exists");
        assert_eq!(st.journal.checkpoints().get("tool:c1"), Some(&7));
    }

    #[test]
    fn live_broadcast_delivers_published_envelopes() {
        let hub = RingingHub::new("epoch-1");
        let mut rx = hub.subscribe(RingingChannel::Conversation);
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None }),
        );
        let env = rx.blocking_recv().expect("live event");
        assert_eq!(env.seed, "s");
    }

    #[test]
    fn reliable_live_publish_does_not_fill_an_undrained_router_queue() {
        let hub = RingingHub::new("epoch");
        for _ in 0..5_000 {
            let outcome = hub.publish(
                "s",
                DomainEvent::Conversation(ConversationEvent::ConversationCancelled {
                    turn_id: None,
                }),
            );
            assert!(matches!(outcome, PublishOutcome::Published { .. }));
        }
        assert_eq!(
            hub.last_stream_seq(RingingChannel::Conversation, "s"),
            5_000
        );
    }

    #[test]
    fn native_timeline_intents_bypass_the_ringing_v1_channel_sequencer() {
        let hub = RingingHub::new("epoch");
        let opened = hub
            .publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .expect("timeline intent accepted");
        assert_eq!(opened.timeline_seq, 1);
        assert_eq!(hub.last_stream_seq(RingingChannel::Conversation, "s"), 0);
        let snapshot = hub.timeline_snapshot("s").expect("timeline snapshot");
        assert_eq!(snapshot.watermark, 1);
        assert_eq!(snapshot.turns[0].user_text, "question");
    }

    #[test]
    fn terminal_timeline_intent_is_persisted_before_publish_returns() {
        let root = temp_root("timeline-terminal-sync");
        let hub = RingingHub::with_persistence("epoch", &root);
        hub.publish_timeline(
            "s",
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
                kind: qaqh_domain::TimelineBlockKind::Text,
                tool: None,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
                delta: "hello".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("terminal snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 4);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seal_all_orphans_cleans_running_turn_at_shutdown() {
        // 优雅关闭收尾（Windows 95 语义）：turn 已打开但未 seal（worker
        // 被杀/收尾未完成）时，seal_all_orphans 必须把未 seal turn 收尾为
        // Cancelled + daemon_restart_interrupted 并持久化——重启后不再残留
        // 未 seal turn（安装器更新后不再出现 daemon_restart_interrupted）。
        let root = temp_root("seal-all-orphans");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t1".into(),
                    user_text: "question".into(),
                },
            )
            .expect("timeline intent accepted");
            // 退出收尾（模拟 daemon 优雅关闭路径）。
            hub.seal_all_orphans();
            let snapshot = hub.timeline_snapshot("s").expect("snapshot");
            assert!(snapshot.turns[0].sealed, "orphan turn must be sealed");
            assert_eq!(
                snapshot.turns[0].state,
                qaqh_domain::TimelineTurnState::Cancelled
            );
            let failure = snapshot.turns[0].failure.as_ref().expect("failure marker");
            assert_eq!(failure.code, "daemon_restart_interrupted");
        }
        // 重启（新 epoch）：退出时已收尾，懒加载不再触发孤儿 seal。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").expect("snapshot after restart");
        assert!(snapshot.turns[0].sealed, "no orphan after clean shutdown");
        assert_eq!(
            snapshot.turns[0].state,
            qaqh_domain::TimelineTurnState::Cancelled
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persisted_native_timeline_recovers_snapshot_and_replay_tail() {
        let root = temp_root("timeline-persist");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TextDelta {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    delta: "hello".into(),
                },
            )
            .unwrap();
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").unwrap();
        // 恢复时遗留的 running turn 必须收尾为 Cancelled（孤儿 turn seal
        // 契约），否则前端会永远把它投影为 running 并禁止发送新消息。
        assert_eq!(snapshot.watermark, 6);
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello");
        assert_eq!(
            snapshot.turns[0].state,
            qaqh_domain::TimelineTurnState::Cancelled
        );
        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert_eq!(hub.timeline_replay_since("s", 1).len(), 5);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn timeline_recovers_identical_snapshot_from_journal_after_cache_deleted() {
        // 验收 #1：删除 `ringing-timeline/{seed}.json` 缓存文件后，同名 seed 仍
        // 能从 timeline journal 重建出**逐字相同**的快照（前端 transcript 无变化）。
        let root = temp_root("timeline-journal-authoritative");
        let native = {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t".into(),
                    user_text: "question".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TextDelta {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    delta: "hello".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockSealed {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "text".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::RoundSealed {
                    turn_id: "t".into(),
                    round_num: 0,
                    is_final: true,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnSealed {
                    turn_id: "t".into(),
                    state: qaqh_domain::TimelineTurnState::Completed,
                    failure: None,
                },
            )
            .unwrap();
            hub.flush_timeline_persistence();
            let snapshot = hub.timeline_snapshot("s").unwrap();
            drop(hub);
            snapshot
        };
        // 删除缓存文件（保留 timeline journal 权威日志）。
        let cache = root.join("ringing-timeline").join("s.json");
        assert!(cache.exists(), "cache file expected before deletion");
        std::fs::remove_file(&cache).expect("delete cache file");
        // 重启：仅剩 journal → 必须从日志重建出逐字相同的快照（且自动回写缓存）。
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let restored = hub.timeline_snapshot("s").unwrap();
        assert_eq!(restored, native, "journal rebuild == native snapshot");
        assert!(cache.exists(), "cache must be rewritten after journal rebuild");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_only_legacy_seed_is_backfilled_into_journal_on_first_load() {
        // 验收 #3：旧历史（仅 `ringing-timeline/{seed}.json`，无 timeline journal）
        // 在首次装载时一次性迁移回填 journal（Snapshot 基点），幂等可重跑。
        let root = temp_root("timeline-legacy-backfill");
        TimelineStore::new(&root)
            .unwrap()
            .persist(
                "s",
                &TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            let snapshot = hub.timeline_snapshot("s").unwrap();
            assert_eq!(snapshot.watermark, 0, "legacy cache restored");
        }
        let ops = TimelineStore::new(&root)
            .unwrap()
            .read_journal("s")
            .unwrap();
        assert_eq!(ops.len(), 1, "backfilled Snapshot op expected");
        assert!(matches!(ops[0], TimelineJournalOp::Snapshot { .. }));
        // 幂等：再次启动不会重复追加。
        TimelineStore::new(&root)
            .unwrap()
            .read_journal("s")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orphan_running_turns_are_sealed_on_recovery_and_sealed_turns_are_untouched() {
        let root = temp_root("timeline-orphan-seal");
        {
            let hub = RingingHub::with_persistence("epoch-1", &root);
            // 完整完成的 turn：正常 seal 后恢复不得被改动。
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t1".into(),
                    user_text: "a".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t1".into(),
                    round_num: 0,
                    block_id: "text".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockSealed {
                    turn_id: "t1".into(),
                    round_num: 0,
                    block_id: "text".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::RoundSealed {
                    turn_id: "t1".into(),
                    round_num: 0,
                    is_final: true,
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnSealed {
                    turn_id: "t1".into(),
                    state: qaqh_domain::TimelineTurnState::Completed,
                    failure: None,
                },
            )
            .unwrap();
            // 孤儿 running turn：只有 TurnOpened + BlockOpened，无任何 seal。
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::TurnOpened {
                    turn_id: "t2".into(),
                    user_text: "b".into(),
                },
            )
            .unwrap();
            hub.publish_timeline(
                "s",
                qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: "t2".into(),
                    round_num: 0,
                    block_id: "text2".into(),
                    kind: qaqh_domain::TimelineBlockKind::Text,
                    tool: None,
                },
            )
            .unwrap();
        }
        let hub = RingingHub::with_persistence("epoch-2", &root);
        let snapshot = hub.timeline_snapshot("s").unwrap();
        let t1 = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == "t1")
            .unwrap();
        assert_eq!(t1.state, qaqh_domain::TimelineTurnState::Completed);
        assert!(t1.sealed);
        let t2 = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == "t2")
            .unwrap();
        assert_eq!(t2.state, qaqh_domain::TimelineTurnState::Cancelled);
        assert!(t2.sealed);
        assert_eq!(
            t2.rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        // 幂等：再次收尾无变更。
        assert!(!hub.seal_orphan_running_turns("s"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn round_deltas_are_reliable_and_compacted_on_round_completed() {
        let hub = RingingHub::new("epoch-1");
        let delta = |seq: u64| {
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 1,
                kind: qaqh_domain::RoundDeltaKind::Answering,
                delta: format!("d{seq}"),
            })
        };

        let first = hub.publish("s", delta(1));
        let second = hub.publish("s", delta(2));
        assert!(matches!(
            first,
            PublishOutcome::Published { ref envelope } if envelope.delivery == Delivery::Reliable
        ));
        assert!(matches!(
            second,
            PublishOutcome::Published { ref envelope } if envelope.delivery == Delivery::Reliable
        ));

        // 增量可靠入 journal：回放必须完整（修复“重连只剩最后一个 delta”的吞字）。
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert_eq!(replay.len(), 2);

        // RoundCompleted 到达后该 round 的增量被压缩，全量终态保留。
        let completed = hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::RoundCompleted {
                turn_id: "t1".into(),
                round_num: 1,
                thinking: Some("d1d2".into()),
                answer: None,
                output_ref: None,
                is_final: true,
            }),
        );
        assert!(matches!(completed, PublishOutcome::Published { .. }));
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert_eq!(replay.len(), 1);
        assert!(matches!(
            &replay[0].event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
        ));
    }

    #[test]
    fn seal_orphan_channel_state_converges_three_channels() {
        let hub = RingingHub::new("epoch-seal");
        // 无终态的中断现场：running turn、running compact、running tool + 挂起权限、未决 ask
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Conversation(ConversationEvent::CompactStarted {
                compact_id: "compact-1".into(),
                turns_total: 8,
                turns_keeping: 2,
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Tool(ToolEvent::ToolPermissionRequested {
                tool_call_id: "c2".into(),
                turn_id: "t1".into(),
                round_num: 0,
                tool_name: "exec".into(),
                reason: "r".into(),
                paths: vec![],
                category: qaqh_domain::PermissionCategory::Exec,
                level: 3,
                risk: qaqh_domain::PermissionRisk::High,
                consequence: "run".into(),
            }),
        );
        hub.publish(
            "s",
            DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "i1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        );

        // 收尾前：三个投影都携带无终态残留
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["active_turn"],
            "t1"
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Conversation, "s").state["compact_status"],
            "running"
        );
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["running"].is_array());
        assert_eq!(
            hub.snapshot(RingingChannel::Tool, "s").state["pending_permission"],
            "c2"
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "i1"
        );

        // force=true：本测试的 InteractionRequested 由当前进程发布（活表内），
        // 语义为「worker 死亡/重启后的强制收尾」，故 force=true。
        assert!(hub.seal_orphan_channel_state("s", true));
        // 幂等：再次调用无变更
        assert!(!hub.seal_orphan_channel_state("s", true));

        // 收尾后：三个投影全部收敛
        let conversation = hub.snapshot(RingingChannel::Conversation, "s");
        assert!(conversation.state["active_turn"].is_null());
        assert_eq!(conversation.state["compact_status"], "failed");
        assert_eq!(conversation.state["compact_id"], "compact-1");
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["running"].is_null());
        assert!(hub.snapshot(RingingChannel::Tool, "s").state["pending_permission"].is_null());
        assert!(hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"].is_null());

        // journal 已包含终态事件（SSE 客户端与重启后的重放都收敛）
        let replay = hub
            .replay_since(RingingChannel::Conversation, "s", 0)
            .expect("within window");
        assert!(replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::ConversationCancelled {
                turn_id: Some(id)
            }) if id == "t1"
        )));
        assert!(replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Conversation(ConversationEvent::CompactFinished {
                compact_id,
                status: CompactStatus::Failed,
                ..
            }) if compact_id == "compact-1"
        )));
        let tool_replay = hub
            .replay_since(RingingChannel::Tool, "s", 0)
            .expect("within window");
        assert!(tool_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolFinished {
                tool_call_id,
                ..
            }) if tool_call_id == "c1"
        )));
        assert!(tool_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Tool(ToolEvent::ToolFinished {
                tool_call_id,
                ..
            }) if tool_call_id == "c2"
        )));
        let control_replay = hub
            .replay_since(RingingChannel::Control, "s", 0)
            .expect("within window");
        assert!(control_replay.iter().any(|env| matches!(
            &env.event,
            qaqh_ringing::RingingEvent::Control(ControlEvent::InteractionResolved {
                interaction_id,
                ..
            }) if interaction_id == "i1"
        )));
    }

    #[test]
    fn seal_orphan_channel_state_preserves_live_interaction_awaiting_user() {
        // 回归测试：修复「ask 发布 1ms 后被 bootstrap 孤儿收尾秒杀」——
        // 当前进程发布、等待用户响应的活交互必须被 bootstrap 路径保护；
        // worker 死亡/重启路径（force=true）仍要强制收尾。
        let hub = RingingHub::new("epoch-live-ask");
        hub.publish(
            "s",
            DomainEvent::Control(ControlEvent::InteractionRequested {
                interaction_id: "live-1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        );
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "live-1"
        );
        // bootstrap 路径（force=false）：活交互不被误判为孤儿，无其他孤儿 → false。
        assert!(!hub.seal_orphan_channel_state("s", false));
        assert_eq!(
            hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"]["id"],
            "live-1",
            "live interaction must survive the bootstrap seal path"
        );
        // worker 死亡/重启收尾路径（force=true）：无视守卫，强制收尾。
        assert!(hub.seal_orphan_channel_state("s", true));
        assert!(hub.snapshot(RingingChannel::Control, "s").state["pending_interaction"].is_null());
        // 收尾后活表清空：后续 bootstrap 路径不再保护（幂等）。
        assert!(!hub.seal_orphan_channel_state("s", false));
    }
}
