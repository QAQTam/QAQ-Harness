//! Native, ordered transcript model.
//!
//! This is deliberately independent of `Agent2Ui`: a timeline is the
//! authoritative representation of what a desktop transcript displays, not a
//! projection of a legacy message protocol.

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// A display block in one model round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineBlockKind {
    Reasoning,
    Text,
    Tool,
    Notice,
}

/// Lifecycle of a display block. Markdown is rendered only after `Sealed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineBlockState {
    Open,
    Sealed,
}

/// State updates for a tool block; all updates retain the block's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineToolState {
    Prepared,
    Running,
    Succeeded,
    Failed,
    /// 工具被用户/系统取消（ToolStatus::Cancelled）。终态：不是失败——
    /// 前端应区分「失败（有错误输出）」与「取消（无输出或被中断）」。
    Cancelled,
    /// 工具转入后台继续运行（ToolStatus::Backgrounded）。终态：调用已
    /// 返回（副作用已发生），但进程/任务仍在输出，可经后续查询跟进。
    Backgrounded,
}

impl From<qaqh_types::ToolStatus> for TimelineToolState {
    fn from(status: qaqh_types::ToolStatus) -> Self {
        match status {
            qaqh_types::ToolStatus::Ok => Self::Succeeded,
            qaqh_types::ToolStatus::Error | qaqh_types::ToolStatus::Partial => Self::Failed,
            qaqh_types::ToolStatus::Cancelled => Self::Cancelled,
            qaqh_types::ToolStatus::Backgrounded => Self::Backgrounded,
        }
    }
}

/// Terminal state of a transcript turn. This is distinct from block sealing:
/// a cancelled or failed turn may have valid, already-sealed Markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Sanitised failure information retained with a transcript terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineFailure {
    pub code: String,
    pub message: String,
}

/// Tool permission data belongs to the transcript tool block, while the
/// interaction request/response lifecycle stays on the native control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineToolPermission {
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
}

/// Immutable identity and mutable presentation state for one tool block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Original structured arguments as supplied by the tool producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_json: Option<String>,
    /// 保留下来的工具输出片段。
    ///
    /// ⚠ 这不是"完整输出"，也**不是**靠 `output_ref` 补齐的：标准模式下模型
    /// 文本先被 `TOOL_MODEL_MAX_CHARS`（24K 字符）封顶，而内容外置的阈值是
    /// 10 MiB，二者差两个数量级——外置路径在标准模式下永远不会触发。因此
    /// `output` 就是前端能拿到的全部；想看更多应让模型用更窄的参数重调工具
    /// （截断标记里也是这么提示模型的），而不是期待一个 content 下载端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Display-plane unified diff (file-mutation tools). Never projected to the
    /// model; consumed by the transcript renderer (diff drawer / tool cards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<TimelineToolPermission>,
}

/// Fully materialized display block saved in timeline snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineBlock {
    pub block_id: String,
    /// Stable order within one round. It never changes when the block updates.
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineRound {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<TimelineBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineTurn {
    pub turn_id: String,
    /// seq of the TurnOpened entry that created this turn — the authoritative
    /// time order across snapshots. `0` means unknown (legacy persisted data);
    /// consumers fall back to the turn_id numeric suffix in that case.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    pub state: TimelineTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

/// Authoritative recovery state, not an event array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineSnapshot {
    /// The largest timeline sequence included in `turns`.
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

/// One mutation of the ordered transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineEvent {
    TurnOpened {
        user_text: String,
    },
    BlockOpened {
        block: TimelineBlock,
    },
    /// `fragment_seq` is monotonic within a text/reasoning block.
    TextDelta {
        block_id: String,
        #[cfg_attr(feature = "ts", ts(as = "u32"))]
        fragment_seq: u64,
        delta: String,
    },
    /// Periodic **full value** of a reasoning/text block (replaceable,
    /// overwrite semantics). Self-heals lost/reordered text deltas: the next
    /// checkpoint replaces the accumulated text in full, while `fragment_seq`
    /// accounting keeps validating subsequent incremental deltas.
    BlockCheckpoint {
        block_id: String,
        text: String,
    },
    ToolUpdated {
        block_id: String,
        tool: TimelineTool,
    },
    /// A tool-output chunk, appended to the current progress buffer by the
    /// single transcript reducer.
    ToolProgress {
        block_id: String,
        chunk: String,
    },
    BlockSealed {
        block_id: String,
    },
    RoundSealed {
        is_final: bool,
    },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

/// A globally ordered record for one session seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineEntry {
    /// Strictly monotonic for one `(server epoch, seed)` across all display kinds.
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

/// Producer-to-writer command for the native transcript. Producers never
/// allocate `timeline_seq` or a text fragment sequence: those are assigned by
/// the single writer after intents from model and tool workers have been
/// serialized onto one queue.
///
/// This is deliberately not an `Agent2Ui` or Ringing-event wrapper. It has no
/// channel, delivery, SSE, or legacy message fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum TimelineIntent {
    TurnOpened {
        turn_id: String,
        user_text: String,
    },
    BlockOpened {
        turn_id: String,
        round_num: u32,
        block_id: String,
        kind: TimelineBlockKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<TimelineTool>,
    },
    TextDelta {
        turn_id: String,
        round_num: u32,
        block_id: String,
        delta: String,
    },
    /// Replaceable full value for one reasoning/text block. The writer
    /// overwrites `block.text`; fragment accounting is left untouched so
    /// later `TextDelta`s keep validating against the monotonic counter.
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        block_id: String,
        text: String,
    },
    ToolUpdated {
        turn_id: String,
        round_num: u32,
        block_id: String,
        tool: TimelineTool,
    },
    /// Append execution output without replacing the tool's identity or
    /// arguments. The transcript writer applies this patch to the block.
    ToolProgress {
        turn_id: String,
        round_num: u32,
        block_id: String,
        chunk: String,
    },
    BlockSealed {
        turn_id: String,
        round_num: u32,
        block_id: String,
    },
    RoundSealed {
        turn_id: String,
        round_num: u32,
        is_final: bool,
    },
    TurnSealed {
        turn_id: String,
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

// ═══════════════════════════════════════════════════════════
// Aggregate projections（PR-3-4 迁入：resume 路径的回合聚合树）
// ═══════════════════════════════════════════════════════════
//
// TurnData/RoundData/RoundBlock/ToolCallDef/ToolResultDef 是 resume /
// compact-context 检查点链使用的**聚合投影**（回合聚合树 ≠ domain 事件流）。
// 原 proto 同名类型原样迁入；刻意不加 ts-rs 导出（维持零前端曝光现状）。
// JSON/磁盘形状（含字段顺序与 skip_serializing_if）保持逐字节不变。

/// Tool call definition used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDef {
    pub id: String,
    pub name: String,
    /// Human-readable args summary (e.g. "foo.rs", "search pattern")
    pub args_display: String,
    /// Raw JSON arguments string
    pub args_json: String,
}

/// Tool execution result used in turn projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultDef {
    pub tool_call_id: String,
    pub output: String,
    pub success: bool,
    /// 工具侧五态（Ok/Error/Partial/Cancelled/Backgrounded），历史归档无此
    /// 字段（serde default 兼容旧 journal）；缺失时 rebuild 按 success 二值回退。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<qaqh_types::ToolStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileSnapshotInfo>,
}

/// File metadata snapshot for rich rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotInfo {
    pub path: String,
    pub lines: u32,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// One round of a turn (one API call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundData {
    pub round_num: u32,
    #[serde(default)]
    pub is_final: bool,
    pub thinking: Option<String>,
    pub answer: Option<String>,
    pub tool_calls: Vec<ToolCallDef>,
    pub tool_results: Vec<ToolResultDef>,
    /// Ordered blocks preserving the LLM's output sequence (reasoning ↔ text ↔ tool).
    #[serde(default)]
    pub blocks: Vec<RoundBlock>,
}

/// One full turn (user message + all rounds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnData {
    pub turn_id: String,
    pub user_text: String,
    pub rounds: Vec<RoundData>,
}

/// One block in a round, preserving the LLM's output order.
///
/// Blocks are streamed to the frontend in order so it can reconstruct
/// the exact sequence of reasoning → text → tool calls from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoundBlock {
    /// Model reasoning/thinking block (collapsible in UI).
    Reasoning { content: String },
    /// Plain text answer block.
    Text { content: String },
    /// A tool call the model wants to invoke.
    Tool { card: ToolCallDef },
    /// A server-side web search performed by the model's built-in tool
    /// (Responses API). Shown as a record line; the search itself ran on the
    /// provider, so there is no local tool card or result round-trip.
    WebSearch { action: String },
}
