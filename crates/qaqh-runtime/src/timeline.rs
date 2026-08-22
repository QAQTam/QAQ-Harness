//! Single-writer native transcript timeline.
//!
//! The appender owns sequence allocation and materializes snapshots from the
//! same records it returns to transport. It intentionally does not depend on
//! `Agent2Ui` or the legacy Ringing conversation/tool projections.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use qaqh_domain::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineIntent, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolState, TimelineTurn,
    TimelineTurnState,
};

use crate::timeline_store::TimelineJournalOp;

/// A live Ringing V1 timeline delivery record. `entry.timeline_seq` is the sole SSE cursor for
/// this seed; no per-channel sequence is exposed to a transcript consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineLiveEntry {
    pub seed: String,
    pub entry: TimelineEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineError {
    DuplicateTurn(String),
    MissingTurn(String),
    MissingRound {
        turn_id: String,
        round_num: u32,
    },
    DuplicateBlock(String),
    MissingBlock(String),
    InvalidBlockShape(String),
    InvalidBlockKind(String),
    InvalidToolIdentity(String),
    SealedBlock(String),
    SealedRound {
        turn_id: String,
        round_num: u32,
    },
    SealedTurn(String),
    RoundOutOfOrder {
        turn_id: String,
        expected: u32,
        received: u32,
    },
    FragmentOutOfOrder {
        block_id: String,
        expected: u64,
        received: u64,
    },
    RoundNotReady {
        turn_id: String,
        round_num: u32,
    },
    TurnNotReady(String),
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTurn(turn_id) => write!(f, "timeline turn already exists: {turn_id}"),
            Self::MissingTurn(turn_id) => write!(f, "timeline turn does not exist: {turn_id}"),
            Self::MissingRound { turn_id, round_num } => {
                write!(f, "timeline round does not exist: {turn_id}/{round_num}")
            }
            Self::DuplicateBlock(block_id) => {
                write!(f, "timeline block already exists: {block_id}")
            }
            Self::MissingBlock(block_id) => write!(f, "timeline block does not exist: {block_id}"),
            Self::InvalidBlockShape(block_id) => {
                write!(f, "timeline block kind and payload disagree: {block_id}")
            }
            Self::InvalidBlockKind(block_id) => {
                write!(f, "timeline block cannot receive text: {block_id}")
            }
            Self::InvalidToolIdentity(block_id) => {
                write!(f, "timeline tool identity changed: {block_id}")
            }
            Self::SealedBlock(block_id) => write!(f, "timeline block is sealed: {block_id}"),
            Self::SealedRound { turn_id, round_num } => {
                write!(f, "timeline round is sealed: {turn_id}/{round_num}")
            }
            Self::SealedTurn(turn_id) => write!(f, "timeline turn is sealed: {turn_id}"),
            Self::RoundOutOfOrder {
                turn_id,
                expected,
                received,
            } => write!(
                f,
                "timeline round out of order for {turn_id}: expected {expected}, got {received}"
            ),
            Self::FragmentOutOfOrder {
                block_id,
                expected,
                received,
            } => write!(
                f,
                "timeline fragment out of order for {block_id}: expected {expected}, got {received}"
            ),
            Self::RoundNotReady { turn_id, round_num } => {
                write!(
                    f,
                    "timeline round has unsealed blocks: {turn_id}/{round_num}"
                )
            }
            Self::TurnNotReady(turn_id) => {
                write!(f, "timeline turn has unsealed rounds: {turn_id}")
            }
        }
    }
}

impl std::error::Error for TimelineError {}

#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
}

/// The only component allowed to allocate timeline sequences.
///
/// A future transport actor owns this mutably; keeping its API on `&mut self`
/// makes accidental concurrent producers impossible without an explicit queue.
#[derive(Debug, Default)]
pub struct TimelineAppender {
    seeds: HashMap<String, SeedTimeline>,
}

impl TimelineAppender {
    pub fn new() -> Self {
        Self::default()
    }

    /// 该 seed 的 timeline 是否已在内存（懒加载索引检查用）。
    pub fn contains(&self, seed: &str) -> bool {
        self.seeds.contains_key(seed)
    }

    pub fn open_turn(
        &mut self,
        seed: &str,
        turn_id: impl Into<String>,
        user_text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let turn_id = turn_id.into();
        let user_text = user_text.into();
        let timeline = self.seeds.entry(seed.to_string()).or_default();
        if timeline.turns.contains_key(&turn_id) {
            // Reopen allowance: the message store is the authoritative history
            // and counts every turn, while meta.turn_count only persists on
            // completion — so a daemon restart can leave the worker's restored
            // counter behind the timeline's recorded turns. The next input then
            // legitimately reuses an id the timeline already sealed, either
            // orphan-Cancelled by the sealer or Completed when the count lagged
            // further (observed: t14 reused as Completed after restart).
            // Rejecting the intent (DuplicateTurn) starves the frontend
            // transcript for that turn forever: every later block/text intent
            // fails against the sealed turn and the resumed stream stays blank.
            // Any sealed turn is terminal history; a fresh TurnOpened for the
            // same id means the worker reuses it for a new input, so reset it
            // in place. Only an unsealed (still running) duplicate is a genuine
            // error.
            let reopenable = {
                let turn = timeline.turns.get(&turn_id).expect("checked above");
                turn.sealed
            };
            if reopenable {
                let reopened_user_text = {
                    let turn = timeline.turns.get_mut(&turn_id).expect("checked above");
                    turn.user_text = user_text;
                    turn.sealed = false;
                    turn.state = TimelineTurnState::Running;
                    turn.failure = None;
                    turn.rounds.clear();
                    turn.user_text.clone()
                };
                // Fragment counters are keyed by (turn, round, block) and the
                // reopened turn may reuse the same round/block ids, so reset
                // them — otherwise the resumed stream's first TextDelta is
                // rejected with FragmentOutOfOrder and the transcript stalls
                // again.
                timeline
                    .next_fragment
                    .retain(|(turn, _, _), _| turn != &turn_id);
                return Ok(next_entry(
                    timeline,
                    turn_id,
                    None,
                    TimelineEvent::TurnOpened {
                        user_text: reopened_user_text,
                    },
                ));
            }
            return Err(TimelineError::DuplicateTurn(turn_id));
        }
        // 预分配：紧接其后的 TurnOpened entry 将占用 next_seq+1，作为该
        // turn 的权威创建序（快照排序依据，不依赖 turn_id 命名格式）。
        let created_seq = timeline.next_seq.saturating_add(1);
        timeline.turns.insert(
            turn_id.clone(),
            TimelineTurn {
                turn_id: turn_id.clone(),
                created_seq,
                user_text: user_text.clone(),
                sealed: false,
                state: TimelineTurnState::Running,
                failure: None,
                rounds: vec![],
            },
        );
        Ok(next_entry(
            timeline,
            turn_id,
            None,
            TimelineEvent::TurnOpened { user_text },
        ))
    }

    pub fn open_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: impl Into<String>,
        kind: TimelineBlockKind,
        tool: Option<TimelineTool>,
    ) -> Result<TimelineEntry, TimelineError> {
        let block_id = block_id.into();
        if (kind == TimelineBlockKind::Tool) != tool.is_some() {
            return Err(TimelineError::InvalidBlockShape(block_id));
        }
        let timeline = self.timeline_mut(seed)?;
        let round = ensure_round_mut(timeline, turn_id, round_num)?;
        if round.sealed {
            return Err(TimelineError::SealedRound {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        if round.blocks.iter().any(|block| block.block_id == block_id) {
            return Err(TimelineError::DuplicateBlock(block_id));
        }
        let block = TimelineBlock {
            block_id: block_id.clone(),
            block_order: u32::try_from(round.blocks.len()).unwrap_or(u32::MAX),
            kind,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool,
        };
        round.blocks.push(block.clone());
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockOpened { block },
        ))
    }

    pub fn append_text(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        fragment_seq: u64,
        delta: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let key = (turn_id.to_string(), round_num, block_id.to_string());
        let expected = *timeline.next_fragment.get(&key).unwrap_or(&0);
        if fragment_seq != expected {
            return Err(TimelineError::FragmentOutOfOrder {
                block_id: block_id.to_string(),
                expected,
                received: fragment_seq,
            });
        }
        let delta = delta.into();
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        if !matches!(
            block.kind,
            TimelineBlockKind::Reasoning | TimelineBlockKind::Text
        ) {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        }
        block.text.push_str(&delta);
        timeline
            .next_fragment
            .insert(key, expected.saturating_add(1));
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::TextDelta {
                block_id: block_id.to_string(),
                fragment_seq,
                delta,
            },
        ))
    }

    /// Applies a replaceable full-value overwrite for one reasoning/text
    /// block. Self-healing for lost/reordered streamed deltas: the producer
    /// emits the complete accumulated text on a timer/token window, and a
    /// consumer replaces instead of appending. `next_fragment` accounting is
    /// intentionally left untouched so subsequent `TextDelta`s still validate
    /// against the monotonic counter (their text appends after the overwrite).
    pub fn checkpoint_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        if !matches!(
            block.kind,
            TimelineBlockKind::Reasoning | TimelineBlockKind::Text
        ) {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        }
        let text = text.into();
        block.text = text.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockCheckpoint {
                block_id: block_id.to_string(),
                text,
            },
        ))
    }

    pub fn update_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        state: TimelineToolState,
        summary: Option<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        tool.state = state;
        tool.summary = summary.or_else(|| tool.summary.clone());
        let tool = tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool,
            },
        ))
    }

    /// Updates mutable presentation fields while preserving the identity and
    /// any durable detail omitted by a lifecycle producer (notably retained
    /// execution progress and a pending permission record).
    pub fn replace_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        mut next_tool: TimelineTool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        if tool.tool_call_id != next_tool.tool_call_id || tool.name != next_tool.name {
            return Err(TimelineError::InvalidToolIdentity(block_id.to_string()));
        }
        if next_tool.progress.is_empty() {
            next_tool.progress = tool.progress.clone();
        }
        if next_tool.permission.is_none() {
            next_tool.permission = tool.permission.clone();
        }
        *tool = next_tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool: next_tool,
            },
        ))
    }

    /// Applies an append-only execution-output patch to an existing tool
    /// block. Identity, arguments, terminal output, and permission state stay
    /// untouched until their explicit lifecycle update arrives.
    pub fn append_tool_progress(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        chunk: String,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        tool.progress.push_str(&chunk);
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolProgress {
                block_id: block_id.to_string(),
                chunk,
            },
        ))
    }

    /// Applies one producer intent. The method is the only place that turns a
    /// producer's ordered intent into a numbered transcript record.
    pub fn apply_intent(
        &mut self,
        seed: &str,
        intent: TimelineIntent,
    ) -> Result<TimelineEntry, TimelineError> {
        match intent {
            TimelineIntent::TurnOpened { turn_id, user_text } => {
                self.open_turn(seed, turn_id, user_text)
            }
            TimelineIntent::BlockOpened {
                turn_id,
                round_num,
                block_id,
                kind,
                tool,
            } => self.open_block(seed, &turn_id, round_num, block_id, kind, tool),
            TimelineIntent::TextDelta {
                turn_id,
                round_num,
                block_id,
                delta,
            } => {
                let fragment_seq = self
                    .seeds
                    .get(seed)
                    .and_then(|timeline| {
                        timeline
                            .next_fragment
                            .get(&(turn_id.clone(), round_num, block_id.clone()))
                    })
                    .copied()
                    .unwrap_or(0);
                self.append_text(seed, &turn_id, round_num, &block_id, fragment_seq, delta)
            }
            TimelineIntent::BlockCheckpoint {
                turn_id,
                round_num,
                block_id,
                text,
            } => self.checkpoint_block(seed, &turn_id, round_num, &block_id, text),
            TimelineIntent::ToolUpdated {
                turn_id,
                round_num,
                block_id,
                tool,
            } => self.replace_tool(seed, &turn_id, round_num, &block_id, tool),
            TimelineIntent::ToolProgress {
                turn_id,
                round_num,
                block_id,
                chunk,
            } => self.append_tool_progress(seed, &turn_id, round_num, &block_id, chunk),
            TimelineIntent::BlockSealed {
                turn_id,
                round_num,
                block_id,
            } => self.seal_block(seed, &turn_id, round_num, &block_id),
            TimelineIntent::RoundSealed {
                turn_id,
                round_num,
                is_final,
            } => self.seal_round(seed, &turn_id, round_num, is_final),
            TimelineIntent::TurnSealed {
                turn_id,
                state,
                failure,
            } => self.seal_turn_with_state(seed, &turn_id, state, failure),
        }
    }

    pub fn seal_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        block.state = TimelineBlockState::Sealed;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockSealed {
                block_id: block_id.to_string(),
            },
        ))
    }

    pub fn seal_round(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        is_final: bool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        if round
            .blocks
            .iter()
            .any(|block| block.state != TimelineBlockState::Sealed)
        {
            return Err(TimelineError::RoundNotReady {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        round.sealed = true;
        round.is_final = is_final;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::RoundSealed { is_final },
        ))
    }

    pub fn seal_turn(&mut self, seed: &str, turn_id: &str) -> Result<TimelineEntry, TimelineError> {
        self.seal_turn_with_state(seed, turn_id, TimelineTurnState::Completed, None)
    }

    pub fn seal_turn_with_state(
        &mut self,
        seed: &str,
        turn_id: &str,
        state: TimelineTurnState,
        failure: Option<qaqh_domain::TimelineFailure>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let turn = timeline
            .turns
            .get_mut(turn_id)
            .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
        if turn.rounds.iter().any(|round| !round.sealed) {
            return Err(TimelineError::TurnNotReady(turn_id.to_string()));
        }
        turn.sealed = true;
        turn.state = state;
        turn.failure = failure.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        ))
    }

    pub fn replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {
        self.seeds.get(seed).map_or_else(Vec::new, |timeline| {
            timeline
                .journal
                .iter()
                .filter(|entry| entry.timeline_seq > watermark)
                .cloned()
                .collect()
        })
    }

    pub fn snapshot(&self, seed: &str) -> Option<TimelineSnapshot> {
        self.seeds.get(seed).map(|timeline| {
            let mut turns: Vec<TimelineTurn> = timeline.turns.values().cloned().collect();
            // turns 存于 HashMap（无序）——按 created_seq 排序（TurnOpened
            // entry 的 seq，权威时间序）；旧磁盘数据 created_seq=0 时退化为
            // turn_id 数值序（t1..tN 递增）。两者混合时旧 turn 在前、新 turn
            // 在后，时间序依然正确。快照数组序必须=时间序：前端恢复按"尾部
            // 窗口"取最新回合，顺序错乱会恢复出错误的回合集合（实测：40
            // turns 会话恢复窗口落在旧回合，最新消息缺失）。
            turns.sort_by_key(|t| (t.created_seq, turn_num(&t.turn_id)));
            TimelineSnapshot {
                watermark: timeline.next_seq,
                turns,
            }
        })
    }

    /// Restores a journal that was previously produced by this appender. The
    /// persisted materialized snapshot is authoritative; the journal is kept
    /// solely for replay after a reconnect watermark.
    pub fn restore(
        &mut self,
        seed: String,
        snapshot: TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) {
        let mut next_fragment = HashMap::new();
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
            {
                if let Some(round_num) = entry.round_num {
                    next_fragment.insert(
                        (entry.turn_id.clone(), round_num, block_id.clone()),
                        fragment_seq.saturating_add(1),
                    );
                }
            }
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
            },
        );
    }

    fn timeline_mut(&mut self, seed: &str) -> Result<&mut SeedTimeline, TimelineError> {
        self.seeds
            .get_mut(seed)
            .ok_or_else(|| TimelineError::MissingTurn(format!("seed:{seed}")))
    }
}

/// turn_id → 数值序（t1/t10 → 1/10）；无数字后缀按 0（保持原序兜底）。
fn turn_num(id: &str) -> u64 {
    id.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0)
}

fn next_entry(
    timeline: &mut SeedTimeline,
    turn_id: String,
    round_num: Option<u32>,
    event: TimelineEvent,
) -> TimelineEntry {
    timeline.next_seq = timeline.next_seq.saturating_add(1);
    let entry = TimelineEntry {
        timeline_seq: timeline.next_seq,
        turn_id,
        round_num,
        event,
    };
    timeline.journal.push(entry.clone());
    entry
}

fn existing_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    let index = turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
        .ok_or_else(|| TimelineError::MissingRound {
            turn_id: turn_id.to_string(),
            round_num,
        })?;
    Ok(&mut turn.rounds[index])
}

fn ensure_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    if turn.sealed {
        return Err(TimelineError::SealedTurn(turn_id.to_string()));
    }
    let index = match turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
    {
        Some(index) => index,
        None => {
            let expected = turn
                .rounds
                .last()
                .map_or(0, |round| round.round_num.saturating_add(1));
            if round_num != expected {
                return Err(TimelineError::RoundOutOfOrder {
                    turn_id: turn_id.to_string(),
                    expected,
                    received: round_num,
                });
            }
            turn.rounds.push(TimelineRound {
                round_num,
                sealed: false,
                is_final: false,
                blocks: vec![],
            });
            turn.rounds.len() - 1
        }
    };
    Ok(&mut turn.rounds[index])
}

fn block_mut<'a>(
    round: &'a mut TimelineRound,
    block_id: &str,
) -> Result<&'a mut TimelineBlock, TimelineError> {
    round
        .blocks
        .iter_mut()
        .find(|block| block.block_id == block_id)
        .ok_or_else(|| TimelineError::MissingBlock(block_id.to_string()))
}

/// 从 append-only timeline journal 纯重放重建 `TimelineSnapshot`（刀 2 阶段 1）。
///
/// - 存在 `Snapshot` 基点（一次性历史迁移/未来压缩）时：`turns` 以基点为权威，
///   其 watermark 之后的 `Append` 增量应用；watermark 及之前的 `Append` 已被
///   物化进基点快照，只保留不再重放（避免折叠条目的 TextDelta 二次叠加）。
/// - 无基点（原生 append-only 流）：全量重放 `Append` 重建 turns。
///
/// 返回 `(snapshot, journal)`：`journal` 是与 appender 内存 journal 等价的全部
/// 条目序列——`restore` 从它重建 `next_fragment`（fragment 校验）与 replay tail。
/// 重放是**宽容**的：未知 turn/block 的增量条目直接跳过（对应磁盘损坏行），
/// 绝不 panic、绝不因单条坏记录丢弃整条 timeline。
pub fn materialize_timeline_from_journal(
    ops: &[TimelineJournalOp],
) -> Option<(TimelineSnapshot, Vec<TimelineEntry>)> {
    let mut base: Option<(TimelineSnapshot, u64)> = None;
    for op in ops {
        if let TimelineJournalOp::Snapshot { snapshot } = op {
            base = Some((snapshot.clone(), snapshot.watermark));
        }
    }
    let base_watermark = base.as_ref().map(|(_, watermark)| *watermark).unwrap_or(0);
    let mut turns: BTreeMap<String, TimelineTurn> = match &base {
        Some((snapshot, _)) => snapshot
            .turns
            .iter()
            .map(|turn| (turn.turn_id.clone(), turn.clone()))
            .collect(),
        None => BTreeMap::new(),
    };
    let mut journal: Vec<TimelineEntry> = Vec::new();
    // seq 单调守卫：append 半途失败（部分行已落盘）后重试会从旧 watermark
    // 重新追加，文件里留下重复条目——而 `TextDelta`/`ToolProgress` 重放不是
    // 幂等的（push_str 会双写）。seq 必须严格递增，重复/乱序条目直接跳过。
    let mut last_seq = 0u64;
    for op in ops {
        if let TimelineJournalOp::Append { entry } = op {
            if entry.timeline_seq <= last_seq {
                log::warn!(
                    "[timeline] skipping non-monotonic journal entry seq={} (last={})",
                    entry.timeline_seq,
                    last_seq
                );
                continue;
            }
            last_seq = entry.timeline_seq;
            // watermark 及之前的条目折叠在基点快照内；只回放其后的增量。
            if base.is_none() || entry.timeline_seq > base_watermark {
                apply_journal_entry(&mut turns, entry);
            }
            journal.push(entry.clone());
        }
    }
    let mut snapshot_turns: Vec<TimelineTurn> = turns.into_values().collect();
    if snapshot_turns.is_empty() && journal.is_empty() && base_watermark == 0 {
        return None;
    }
    // 与 `TimelineAppender::snapshot` 保持同一排序键（字节级一致）。
    snapshot_turns.sort_by_key(|turn| (turn.created_seq, turn_num(&turn.turn_id)));
    let watermark = journal
        .last()
        .map_or(base_watermark, |entry| entry.timeline_seq);
    Some((
        TimelineSnapshot {
            watermark,
            turns: snapshot_turns,
        },
        journal,
    ))
}

/// 把一条已分配 seq 的 `TimelineEntry` 直接物化为 `turns` 状态（幂等重放）。
/// 语义与 `TimelineAppender` 各 writer 方法完全一致：按 entry 记录逐项应用。
fn apply_journal_entry(turns: &mut BTreeMap<String, TimelineTurn>, entry: &TimelineEntry) {
    match &entry.event {
        TimelineEvent::TurnOpened { user_text } => {
            // 同 id 重开 = 原地重置（与 live `open_turn` 的 reopen 分支一致）。
            let turn = TimelineTurn {
                turn_id: entry.turn_id.clone(),
                created_seq: entry.timeline_seq,
                user_text: user_text.clone(),
                sealed: false,
                state: TimelineTurnState::Running,
                failure: None,
                rounds: Vec::new(),
            };
            turns.insert(entry.turn_id.clone(), turn);
        }
        TimelineEvent::BlockOpened { block } => {
            let Some(round_num) = entry.round_num else {
                return;
            };
            let Some(turn) = turns.get_mut(&entry.turn_id) else {
                return;
            };
            if turn.sealed {
                return;
            }
            let round = match turn.rounds.iter().position(|r| r.round_num == round_num) {
                Some(index) => &mut turn.rounds[index],
                None => {
                    turn.rounds.push(TimelineRound {
                        round_num,
                        sealed: false,
                        is_final: false,
                        blocks: Vec::new(),
                    });
                    let index = turn.rounds.len() - 1;
                    &mut turn.rounds[index]
                }
            };
            if !round.blocks.iter().any(|b| b.block_id == block.block_id) {
                round.blocks.push(block.clone());
            }
        }
        TimelineEvent::TextDelta {
            block_id, delta, ..
        } => {
            if let Some(block) = block_mut_replay(turns, &entry.turn_id, entry.round_num, block_id)
            {
                block.text.push_str(delta);
            }
        }
        TimelineEvent::BlockCheckpoint { block_id, text } => {
            if let Some(block) = block_mut_replay(turns, &entry.turn_id, entry.round_num, block_id)
            {
                block.text = text.clone();
            }
        }
        TimelineEvent::ToolUpdated { block_id, tool } => {
            if let Some(block) = block_mut_replay(turns, &entry.turn_id, entry.round_num, block_id)
            {
                block.tool = Some(tool.clone());
            }
        }
        TimelineEvent::ToolProgress { block_id, chunk } => {
            if let Some(block) = block_mut_replay(turns, &entry.turn_id, entry.round_num, block_id)
                && let Some(tool) = block.tool.as_mut()
            {
                tool.progress.push_str(chunk);
            }
        }
        TimelineEvent::BlockSealed { block_id } => {
            if let Some(block) = block_mut_replay(turns, &entry.turn_id, entry.round_num, block_id)
            {
                block.state = TimelineBlockState::Sealed;
            }
        }
        TimelineEvent::RoundSealed { is_final } => {
            let Some(turn) = turns.get_mut(&entry.turn_id) else {
                return;
            };
            let Some(round_num) = entry.round_num else {
                return;
            };
            if let Some(round) = turn.rounds.iter_mut().find(|r| r.round_num == round_num) {
                round.sealed = true;
                round.is_final = *is_final;
            }
        }
        TimelineEvent::TurnSealed { state, failure } => {
            if let Some(turn) = turns.get_mut(&entry.turn_id) {
                turn.sealed = true;
                turn.state = *state;
                turn.failure = failure.clone();
            }
        }
    }
}

/// 重放时按 (turn, round, block) 定位可变 block（宽松：任一缺失直接 None）。
fn block_mut_replay<'a>(
    turns: &'a mut BTreeMap<String, TimelineTurn>,
    turn_id: &str,
    round_num: Option<u32>,
    block_id: &str,
) -> Option<&'a mut TimelineBlock> {
    let turn = turns.get_mut(turn_id)?;
    let round_num = round_num?;
    let round = turn.rounds.iter_mut().find(|r| r.round_num == round_num)?;
    round.blocks.iter_mut().find(|b| b.block_id == block_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::TimelineFailure;

    fn tool() -> TimelineTool {
        TimelineTool {
            tool_call_id: "call-1".into(),
            name: "read".into(),
            state: TimelineToolState::Prepared,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            failure: None,
            permission: None,
        }
    }

    #[test]
    fn orphan_cancelled_turn_is_reopened_by_the_next_input() {
        // Regression: after a daemon restart the orphan-sealer marks the
        // interrupted turn Cancelled, but the message store still counts it,
        // so the worker's next input reuses the same turn_id. open_turn must
        // reset that placeholder instead of rejecting with DuplicateTurn —
        // otherwise every timeline intent for the turn is dropped and the
        // frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender
            .open_turn("s", "t1", "interrupted question")
            .unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "partial")
            .unwrap();
        appender.seal_block("s", "t1", 0, "answer").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        // Simulate daemon restart orphan seal.
        appender
            .seal_turn_with_state(
                "s",
                "t1",
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message: "interrupted".into(),
                }),
            )
            .unwrap();

        // The worker reuses t1 for the resumed conversation.
        let reopened = appender
            .open_turn("s", "t1", "next question")
            .expect("orphan cancelled turn must be reopenable");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));

        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "next question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
        assert!(turn.failure.is_none(), "failure marker is cleared");

        // Subsequent intents for the reopened turn flow normally.
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "full reply")
            .unwrap();
        let final_snapshot = appender.snapshot("s").unwrap();
        assert_eq!(
            final_snapshot.turns[0].rounds[0].blocks[0].text,
            "full reply"
        );
    }

    #[test]
    fn completed_turn_is_reopened_when_id_is_reused() {
        // Regression: after a daemon restart the worker's restored turn
        // counter (message store) can lag behind the timeline's recorded
        // turns, so the next input reuses an id the timeline already sealed
        // as Completed (observed: t14 reused after restart). The timeline must
        // reset that terminal turn in place instead of rejecting with
        // DuplicateTurn — otherwise every intent for the resumed turn is
        // dropped and the frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        appender.seal_turn("s", "t1").unwrap();
        let reopened = appender
            .open_turn("s", "t1", "reused question")
            .expect("sealed turns must be reopenable on id reuse");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "reused question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.failure.is_none());
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
    }

    #[test]
    fn running_turn_is_not_reopened() {
        // An unsealed (still running) turn is a live producer; a second
        // TurnOpened for the same id is a genuine duplicate and must fail.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        let err = appender
            .open_turn("s", "t1", "duplicate")
            .expect_err("running turns must not be reopened");
        assert!(matches!(err, TimelineError::DuplicateTurn(_)));
    }

    #[test]
    fn appender_assigns_a_single_order_across_text_and_tools() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "reasoning", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "reasoning", 0, "inspect")
            .unwrap();
        appender.seal_block("s", "t", 0, "reasoning").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .update_tool(
                "s",
                "t",
                0,
                "tool",
                TimelineToolState::Succeeded,
                Some("read file".into()),
            )
            .unwrap();
        appender.seal_block("s", "t", 0, "tool").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "done")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        appender.seal_round("s", "t", 0, true).unwrap();
        appender.seal_turn("s", "t").unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let blocks = &snapshot.turns[0].rounds[0].blocks;
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.block_id.as_str())
                .collect::<Vec<_>>(),
            ["reasoning", "tool", "answer"]
        );
        assert!(
            blocks
                .iter()
                .all(|block| block.state == TimelineBlockState::Sealed)
        );
        assert_eq!(blocks[2].text, "done");
        assert_eq!(
            blocks[1].tool.as_ref().unwrap().state,
            TimelineToolState::Succeeded
        );
        assert_eq!(
            appender.replay_since("s", 0).last().unwrap().timeline_seq,
            snapshot.watermark
        );
    }

    #[test]
    fn appender_rejects_missing_or_reordered_text_fragments() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "late"),
            Err(TimelineError::FragmentOutOfOrder {
                expected: 0,
                received: 1,
                ..
            })
        ));
        appender
            .append_text("s", "t", 0, "answer", 0, "first")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "after seal"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn tool_progress_survives_a_terminal_lifecycle_update() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        let progress = appender
            .apply_intent(
                "s",
                TimelineIntent::ToolProgress {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "tool".into(),
                    chunk: "executing\\n".into(),
                },
            )
            .unwrap();
        let mut final_tool = tool();
        final_tool.state = TimelineToolState::Succeeded;
        final_tool.output = Some("done".into());
        appender
            .replace_tool("s", "t", 0, "tool", final_tool)
            .unwrap();

        assert!(matches!(progress.event, TimelineEvent::ToolProgress { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let tool = snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress, "executing\\n");
        assert_eq!(tool.output.as_deref(), Some("done"));
        assert_eq!(tool.state, TimelineToolState::Succeeded);
    }

    #[test]
    fn appender_rejects_ambiguous_block_shapes_and_late_rounds() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        assert!(matches!(
            appender.open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, None),
            Err(TimelineError::InvalidBlockShape(_))
        ));
        assert!(matches!(
            appender.open_block("s", "t", 2, "text", TimelineBlockKind::Text, None),
            Err(TimelineError::RoundOutOfOrder {
                expected: 0,
                received: 2,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_and_replay_form_a_lossless_recovery_boundary() {
        let mut appender = TimelineAppender::new();
        let opened = appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        let first = appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        let second = appender
            .append_text("s", "t", 0, "answer", 1, "lo")
            .unwrap();

        assert_eq!(opened.timeline_seq, 1);
        assert!(first.timeline_seq <= snapshot.watermark);
        let tail = appender.replay_since("s", snapshot.watermark);
        assert_eq!(tail, vec![second]);
    }

    #[test]
    fn intents_allocate_fragment_and_timeline_sequences_at_the_single_writer() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                kind: TimelineBlockKind::Text,
                tool: None,
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "hel".into(),
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "lo".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.timeline_seq)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::TextDelta {
                fragment_seq: 0,
                ..
            }
        ));
        assert!(matches!(
            &entries[3].event,
            TimelineEvent::TextDelta {
                fragment_seq: 1,
                ..
            }
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "hello"
        );
    }

    #[test]
    fn checkpoint_overwrites_text_and_later_deltas_keep_appending() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        // Replaceable full value: simulates self-healing after a lost delta.
        let checkpoint = appender
            .checkpoint_block("s", "t", 0, "answer", "hello wor")
            .unwrap();
        assert!(matches!(
            checkpoint.event,
            TimelineEvent::BlockCheckpoint {
                ref block_id,
                ref text,
            } if block_id == "answer" && text == "hello wor"
        ));
        // fragment accounting is untouched: next delta validates against 1.
        appender
            .append_text("s", "t", 0, "answer", 1, "ld")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello world");
    }

    #[test]
    fn checkpoint_via_intent_roundtrips_as_event() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                kind: TimelineBlockKind::Reasoning,
                tool: None,
            },
            TimelineIntent::BlockCheckpoint {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                text: "full thinking".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::BlockCheckpoint { block_id, text }
                if block_id == "reasoning" && text == "full thinking"
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "full thinking"
        );
    }

    #[test]
    fn checkpoint_rejected_on_sealed_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "answer", "late"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn checkpoint_rejected_on_tool_or_missing_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool:1", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "tool:1", "nope"),
            Err(TimelineError::InvalidBlockKind(_))
        ));
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "ghost", "nope"),
            Err(TimelineError::MissingBlock(_))
        ));
    }

    #[test]
    fn journal_rebuild_equals_native_snapshot_roundtrip() {
        // 验收 #2：原生写入路径产生的 timeline 与 journal 重放路径一致（往返）。
        let mut appender = TimelineAppender::new();
        // t1：完整完成的 turn（reasoning 流式 + checkpoint + tool + answer）。
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block(
                "s",
                "t1",
                0,
                "reasoning",
                TimelineBlockKind::Reasoning,
                None,
            )
            .unwrap();
        appender
            .append_text("s", "t1", 0, "reasoning", 0, "think")
            .unwrap();
        appender
            .checkpoint_block("s", "t1", 0, "reasoning", "think deeply")
            .unwrap();
        appender
            .append_text("s", "t1", 0, "reasoning", 1, "!")
            .unwrap();
        appender.seal_block("s", "t1", 0, "reasoning").unwrap();
        appender
            .open_block("s", "t1", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .append_tool_progress("s", "t1", 0, "tool", "running...\n".to_string())
            .unwrap();
        appender
            .update_tool(
                "s",
                "t1",
                0,
                "tool",
                TimelineToolState::Succeeded,
                Some("done".into()),
            )
            .unwrap();
        appender.seal_block("s", "t1", 0, "tool").unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "ok")
            .unwrap();
        appender.seal_block("s", "t1", 0, "answer").unwrap();
        appender.seal_round("s", "t1", 0, true).unwrap();
        appender
            .seal_turn_with_state(
                "s",
                "t1",
                TimelineTurnState::Completed,
                Some(TimelineFailure {
                    code: "ok".into(),
                    message: "done".into(),
                }),
            )
            .unwrap();
        // t2：仍 running 的 turn（reopen 语义也在 `TurnOpened` 内保留了 reset）。
        appender.open_turn("s", "t2", "q2").unwrap();
        appender
            .open_block("s", "t2", 0, "note", TimelineBlockKind::Text, None)
            .unwrap();
        appender.append_text("s", "t2", 0, "note", 0, "in").unwrap();

        let native = appender.snapshot("s").unwrap();
        let entries = appender.replay_since("s", 0);
        let ops: Vec<TimelineJournalOp> = entries
            .iter()
            .map(|entry| TimelineJournalOp::Append {
                entry: entry.clone(),
            })
            .collect();
        let (rebuilt, journal) =
            materialize_timeline_from_journal(&ops).expect("full replay must succeed");
        assert_eq!(rebuilt, native, "journal full replay == native snapshot");
        assert_eq!(journal, entries, "journal stream preserved verbatim");
        assert_eq!(rebuilt.watermark, native.watermark);
    }

    #[test]
    fn journal_base_snapshot_with_folded_tail_is_not_double_applied() {
        // 迁移文件形状：`Snapshot` 基点（完整物化，watermark 折叠了此前的全部
        // delta）+ `Append` 尾部条目。折叠条目不得二次叠加文本；watermark 之后
        // 的真实新条目必须正常增量应用。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q").unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "hel")
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 1, "lo")
            .unwrap();

        // 迁移快照点：快照完整，尾部条目（seq ≤ watermark）全部折叠。
        let native = appender.snapshot("s").unwrap();
        let tail = appender.replay_since("s", 0);
        let mut ops: Vec<TimelineJournalOp> = vec![TimelineJournalOp::Snapshot {
            snapshot: native.clone(),
        }];
        ops.extend(tail.iter().map(|entry| TimelineJournalOp::Append {
            entry: entry.clone(),
        }));
        let (rebuilt, _) = materialize_timeline_from_journal(&ops).unwrap();
        assert_eq!(
            rebuilt.turns[0].rounds[0].blocks[0].text, "hello",
            "folded deltas must not double-append"
        );
        assert_eq!(rebuilt, native);

        // 迁移后真实续写（seq > watermark）：text 增量正常追加。
        appender
            .append_text("s", "t1", 0, "answer", 2, "!")
            .unwrap();
        let after = appender.snapshot("s").unwrap();
        ops.extend(
            appender
                .replay_since("s", native.watermark)
                .into_iter()
                .map(|entry| TimelineJournalOp::Append { entry }),
        );
        let (rebuilt2, _) = materialize_timeline_from_journal(&ops).unwrap();
        assert_eq!(rebuilt2.turns[0].rounds[0].blocks[0].text, "hello!");
        assert_eq!(rebuilt2, after, "rebuilt == live beyond the base point");
    }

    #[test]
    fn journal_duplicate_append_entries_are_not_double_applied() {
        // append 半途失败（部分行已落盘）后重试会从旧 watermark 重新追加，
        // 文件里出现重复条目。`TextDelta` 重放非幂等（push_str），必须靠
        // seq 单调守卫跳过重复条目，否则文本会双写。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q").unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "hello")
            .unwrap();
        let entries = appender.replay_since("s", 0);
        // 模拟：第一次追加写入了全部条目，但 watermark 未更新；重试把同一批
        // 条目又追加了一遍（文件里出现两份）。
        let mut ops: Vec<TimelineJournalOp> = entries
            .iter()
            .map(|entry| TimelineJournalOp::Append {
                entry: entry.clone(),
            })
            .collect();
        ops.extend(entries.iter().map(|entry| TimelineJournalOp::Append {
            entry: entry.clone(),
        }));
        let (rebuilt, journal) = materialize_timeline_from_journal(&ops).unwrap();
        let text = &rebuilt.turns[0].rounds[0].blocks[0].text;
        assert_eq!(text, "hello", "duplicate deltas must not double-append");
        assert_eq!(
            journal.len(),
            entries.len(),
            "duplicates dropped from replay tail"
        );
        assert_eq!(rebuilt.watermark, entries.last().unwrap().timeline_seq);
    }
}
