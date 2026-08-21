//! 中立领域事件（DomainEvent）。
//!
//! - 事件按频道拆分（Control / Conversation / Tool），由统一枚举 `DomainEvent` 聚合。
//! - 每个事件类型通过 `delivery()` 显式声明可靠性等级（PLAN 硬规则）。
//! - 本模块不得引用 legacy 类型（`Agent2Ui`）或 wire 类型（`Ringing*Envelope`）。

use serde::{Deserialize, Serialize};

use qaqh_types::UsageInfo;
pub use qaqh_types::{ContentRef, ToolResult};

use crate::channel::RingingChannel;
use crate::delivery::Delivery;

// ─────────────────────────────────────────────────────────────────────────────
// 共享支持类型
// ─────────────────────────────────────────────────────────────────────────────

/// RoundDelta 的流式块种类（决策记录 Q2：保留 kind 作 replaceable 合并键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDeltaKind {
    Thinking,
    ToolCalling,
    Answering,
}

/// provider 内建/服务端工具状态（决策记录 Q3 定稿：封闭枚举，禁止自由字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolState {
    InProgress,
    Searching,
    Completed,
}

/// compact 终态（PLAN：completed/skipped/failed/cancelled 明确状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactStatus {
    Completed,
    Skipped,
    Failed,
    Cancelled,
}

/// 通知级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// 工具权限分类（legacy `category: "read"|"write"|"exec"|"net"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
    Read,
    Write,
    Exec,
    Net,
}

/// 工具动作内在影响等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

/// 会话生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Resumed,
    Closed,
    Archived,
    Unarchived,
    Deleted,
}

/// 会话活动状态（与 legacy `SessionActivityState` 同义，domain 化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Starting,
    Idle,
    Working,
    WaitingUser,
    Disconnected,
}

/// agent **进程**生命周期（决策记录 Q8：只含进程状态；回合结束走
/// `SessionActivityChanged(Idle)`，transport 状态另由客户端健康判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Booting,
    Ready,
    Stopping,
    Stopped,
}

/// A document visible in the dashboard. This intentionally mirrors only the
/// renderer-facing tracking state, not the legacy protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardDocument {
    pub tag: String,
    pub path: String,
    pub turns_since_read: u32,
    pub is_stale: bool,
}

/// One persisted task row for the native dashboard activity snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// Replaceable dashboard/activity payload. It is deliberately separate from
/// the transcript and is sufficient for the Electron dashboard without an
/// `Agent2Ui::Dashboard` projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub seed: String,
    pub documents: Vec<DashboardDocument>,
    pub recent_edits: Vec<String>,
    pub tasks: Vec<DashboardTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_todo_id: Option<String>,
}

/// 失败终态的错误域（PLAN：错误带 scope、code、retryable、dedupe_key）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainError {
    /// 唯一错误实例 id，用于 toast 去重与日志关联。
    pub error_id: String,
    /// 稳定错误码（如 "provider_http_500"）。
    pub code: String,
    /// 人类可读消息（脱敏，禁止含 API key / provider 原始响应）。
    pub message: String,
    /// 是否可重试。
    pub retryable: bool,
    /// 去重键；同键错误只产生一个前端 toast。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

/// 错误归属域（用于 OperationFailed 的 scope 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorScope {
    Control,
    Conversation,
    Tool,
    System,
}

/// ask_user 的提问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    Single,
    Batch,
}

/// ask_user 交互如何离队。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskResolution {
    Answered,
    Dismissed,
}

/// ask_user 中的单个问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    /// 本 ask 内唯一（如 "q1"）。
    pub id: String,
    /// 问题文本（支持 Markdown）。
    pub question: String,
    /// 预设选项；空 = 仅自由文本。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// 是否允许自定义输入。
    #[serde(default = "default_true")]
    pub allow_custom: bool,
}

fn default_true() -> bool {
    true
}

/// todo_activate 评审项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}

/// skill 目录条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// "project" | "user"
    pub scope: String,
    /// 相对 workspace 的展示路径。
    pub source: String,
}

/// skill 运行时条目（catalog/requested/active/unavailable 生命周期状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeInfo {
    pub name: String,
    pub description: String,
    /// 生命周期状态：\"catalog\" | \"requested\" | \"active\" | \"unavailable\"。
    pub state: String,
    /// 展示源路径。
    pub source: String,
    /// skill 正文估算 token 数。
    pub token_count: usize,
    /// 加载失败时的错误信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 技能面板全量状态（frontend skills panel 展示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsStatus {
    /// 全部可发现技能。
    pub available: Vec<SkillInfo>,
    /// 当前已加载（显式 / $ 提及激活）的技能名。
    pub active: Vec<String>,
    #[serde(default)]
    pub catalog_revision: String,
    #[serde(default)]
    pub context_epoch: u64,
    #[serde(default)]
    pub operation_revision: u64,
    #[serde(default)]
    pub token_budget: usize,
    #[serde(default)]
    pub token_usage: usize,
    #[serde(default)]
    pub runtime: Vec<SkillRuntimeInfo>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversation 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Conversation 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEvent {
    /// 新回合开始（`ConversationSendMessage` accepted 后的权威开始事件）。
    TurnStarted { turn_id: String, user_text: String },
    /// 回合完成（成功）。`TurnFailed` 为失败终态。
    TurnCompleted {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageInfo>,
    },
    /// 回合失败（新领域事件；provider 最终失败只产生一个可靠失败终态）。
    TurnFailed { turn_id: String, error: DomainError },
    /// 流式增量（reliable：增量是追加语义，覆盖/合并会吞字；journal 在
    /// `RoundCompleted` 到达后按 round 压缩，见 `ReliableJournal::compact_round_deltas`）。
    RoundDelta {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        delta: String,
    },
    /// 流式块的周期**完整值**（replaceable，覆盖语义，治 D1）。
    ///
    /// `RoundDelta` 是追加增量（reliable，前端拼接）；本事件携带该 round
    /// 当前完整文本，乱序/丢 delta 由下一次 checkpoint 自愈，前端直接
    /// 覆盖赋值。`RoundCompleted` 仍是权威终态（到达后本事件可压缩）。
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        text: String,
        char_count: u32,
    },
    /// 一轮 API 调用完成的权威终态。正文大时经 `output_ref` 外置。
    RoundCompleted {
        turn_id: String,
        round_num: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<ContentRef>,
        /// true = 本回合最后一个 round。
        is_final: bool,
    },
    /// provider 请求瞬时失败将重试（非终态；retry 与最终失败不得共用 event_id）。
    ProviderRetrying {
        turn_id: String,
        round_num: u32,
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error_message: String,
    },
    /// provider 内建/服务端工具状态（决策记录 Q3：replaceable，合并键 = call_id）。
    ProviderToolStatus {
        turn_id: String,
        round_num: u32,
        /// provider 侧 call id（如 web_search_call id），**不是** QAQ-Harness tool_call_id。
        call_id: String,
        /// 目前固定 "web_search"，为未来 provider 内建工具预留。
        tool_kind: String,
        state: ProviderToolState,
    },
    /// provider 确认的用量（可多次发出；消费者按 turn/round 覆盖）。
    UsageUpdated {
        turn_id: String,
        round_num: u32,
        usage: UsageInfo,
        context_limit: u32,
        model: String,
    },
    /// compact 开始（携带 compact_id）。
    CompactStarted {
        compact_id: String,
        turns_total: u32,
        turns_keeping: u32,
    },
    /// compact 流式摘要（replaceable，按 compact_id 合并）。
    CompactProgress { compact_id: String, delta: String },
    /// compact 终态。`ConversationCompact` accepted 不代表成功，本事件才是终态。
    CompactFinished {
        compact_id: String,
        status: CompactStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_chars: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_compacted: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_removed: Option<u32>,
    },
    /// 回合被用户取消。
    ConversationCancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

impl ConversationEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ConversationEvent::ProviderToolStatus { .. }
            | ConversationEvent::UsageUpdated { .. }
            | ConversationEvent::CompactProgress { .. }
            | ConversationEvent::BlockCheckpoint { .. } => Delivery::Replaceable,
            _ => Delivery::Reliable,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Tool 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolEvent {
    /// 流式响应中检测到工具调用（决策记录 Q1：replaceable 预览，可被 ToolStarted 覆盖）。
    ToolCallPrepared {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        name: String,
        args_so_far: String,
    },
    /// 工具真正开始执行（决策记录 Q1：permission 通过后，reliable）。
    ToolStarted {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        name: String,
    },
    /// 工具输出增量（replaceable；PLAN 定稿字段，16ms 合并、256 KiB 上限）。
    ToolProgress {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        stream: String,
        seq_start: u64,
        seq_end: u64,
        chunk: String,
        dropped_bytes: u64,
        truncated: bool,
    },
    /// 工具执行成功终态（terminal；发送前必须 flush/覆盖同工具 replaceable 进度）。
    ToolFinished {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        result: ToolResult,
    },
    /// 权限请求：agent 挂起回合等待用户批准/拒绝。
    ToolPermissionRequested {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        tool_name: String,
        reason: String,
        paths: Vec<String>,
        category: PermissionCategory,
        level: u8,
        risk: PermissionRisk,
        consequence: String,
    },
    /// 工具域通知（决策记录 Q6：留在 Tool 频道，不并入 SystemNotice）。
    ToolNotice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        level: NoticeLevel,
        message: String,
    },
    /// 审计记录（脱敏：args 只进 content store，事件仅携带引用）。
    AuditRecorded {
        tool_name: String,
        result_summary: String,
        success: bool,
        time: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_ref: Option<ContentRef>,
    },
    /// 文件操作后的实时代码统计增量。
    CodeChanged {
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        turn_id: String,
        #[serde(default)]
        round_num: u32,
        lines_added: usize,
        lines_removed: usize,
        files_created: usize,
        files_deleted: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
}

impl ToolEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ToolEvent::ToolCallPrepared { .. } | ToolEvent::ToolProgress { .. } => {
                Delivery::Replaceable
            }
            _ => Delivery::Reliable,
        }
    }

    /// 该事件关联的 tool_call_id（ToolCallPrepared/Started/Progress 恒有）。
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            ToolEvent::ToolCallPrepared { tool_call_id, .. }
            | ToolEvent::ToolStarted { tool_call_id, .. }
            | ToolEvent::ToolProgress { tool_call_id, .. }
            | ToolEvent::ToolFinished { tool_call_id, .. }
            | ToolEvent::ToolPermissionRequested { tool_call_id, .. } => Some(tool_call_id),
            ToolEvent::ToolNotice { tool_call_id, .. } => tool_call_id.as_deref(),
            ToolEvent::CodeChanged { tool_call_id, .. } if !tool_call_id.is_empty() => {
                Some(tool_call_id)
            }
            ToolEvent::AuditRecorded { .. } | ToolEvent::CodeChanged { .. } => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Control 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Control 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    /// 会话生命周期状态变更。
    SessionStateChanged { seed: String, state: SessionState },
    /// 会话活动状态变更（WaitingUser 汇总 interaction/permission 挂起）。
    SessionActivityChanged {
        seed: String,
        state: ActivityState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        seq: u64,
        updated_at: u64,
    },
    /// 会话元数据变更（标题生成/重命名）——前端收到后重拉 session.list。
    SessionMetaChanged {
        seed: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// agent **进程**生命周期（决策记录 Q8：不含回合状态）。
    AgentLifecycleChanged { state: AgentLifecycleState },
    /// 会话仪表盘（replaceable，覆盖式）。
    DashboardUpdated {
        hp_connected: bool,
        session_seed: String,
        tool_calls_total: u32,
        tool_failures: u32,
        current_phase: String,
        streaming: bool,
    },
    /// Full native dashboard activity state, replaceable by session seed.
    DashboardSnapshot { snapshot: DashboardSnapshot },
    /// ask_user 交互请求（决策记录 Q10：ask/plan 归 Control，permission 归 Tool）。
    InteractionRequested {
        interaction_id: String,
        turn_id: String,
        mode: AskMode,
        questions: Vec<AskQuestion>,
    },
    /// ask_user 交互终结。
    InteractionResolved {
        interaction_id: String,
        resolution: AskResolution,
    },
    /// plan review 请求（plan_submit 或 todo_activation）。
    PlanReviewRequested {
        interaction_id: String,
        turn_id: String,
        plan_content: String,
        #[serde(default)]
        review_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        todo_items: Option<Vec<TodoItem>>,
    },
    /// plan review 已裁决。
    PlanReviewResolved {
        interaction_id: String,
        approved: bool,
    },
    /// skill 目录/激活状态变更。
    SkillsUpdated {
        available: Vec<SkillInfo>,
        active: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catalog_revision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_revision: Option<u64>,
        #[serde(default)]
        context_epoch: usize,
        #[serde(default)]
        token_budget: usize,
        #[serde(default)]
        token_usage: usize,
        #[serde(default)]
        runtime: Vec<SkillRuntimeInfo>,
        #[serde(default)]
        diagnostics: Vec<String>,
    },
    /// 系统级通知（决策记录 Q6：最小集——升级、维护、daemon 重启等）。
    SystemNotice {
        notice_id: String,
        level: NoticeLevel,
        message: String,
    },
    /// 子代理终态推送：注入被回合 lap 边界吸收（无独立注入回合）时，
    /// 前端 tracker 的唯一收敛信号仍缺失——本事件补发轻量终态，不进入
    /// 回合状态机、不进模型上下文。`state` 为注入标签原样
    /// （COMPLETED / ERROR / TIMEOUT / CANCELLED）。
    SubagentStatus {
        seed: String,
        name: String,
        state: String,
    },
    /// 无专用领域终态载荷的命令已完成。用于 undo/set-mode/reload 等
    /// 操作的 receipt 收口，不承担 UI 通知语义。
    OperationCompleted {
        occurrence_id: String,
        scope: ErrorScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
    /// 业务失败终态（结构化、可关联、可去重）。
    OperationFailed {
        occurrence_id: String,
        scope: ErrorScope,
        error: DomainError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
}

impl ControlEvent {
    pub fn delivery(&self) -> Delivery {
        match self {
            ControlEvent::DashboardUpdated { .. } | ControlEvent::DashboardSnapshot { .. } => {
                Delivery::Replaceable
            }
            _ => Delivery::Reliable,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 统一领域事件入口
// ─────────────────────────────────────────────────────────────────────────────

/// 统一领域事件。`channel()` 决定进入哪个频道 router；
/// `delivery()` 声明可靠性等级，供 wire envelope 与 daemon 队列使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum DomainEvent {
    Control(ControlEvent),
    Conversation(ConversationEvent),
    Tool(ToolEvent),
}

impl DomainEvent {
    pub fn channel(&self) -> RingingChannel {
        match self {
            DomainEvent::Control(_) => RingingChannel::Control,
            DomainEvent::Conversation(_) => RingingChannel::Conversation,
            DomainEvent::Tool(_) => RingingChannel::Tool,
        }
    }

    pub fn delivery(&self) -> Delivery {
        match self {
            DomainEvent::Control(e) => e.delivery(),
            DomainEvent::Conversation(e) => e.delivery(),
            DomainEvent::Tool(e) => e.delivery(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_round_trip_keeps_fields() {
        let event = DomainEvent::Conversation(ConversationEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "hello".into(),
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"channel\":\"conversation\""));
        assert!(json.contains("\"type\":\"turn_started\""));
        let back: DomainEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back,
            DomainEvent::Conversation(ConversationEvent::TurnStarted { ref user_text, .. })
                if user_text == "hello"
        ));
    }

    #[test]
    fn tool_progress_carries_plan_fields() {
        let event = ToolEvent::ToolProgress {
            tool_call_id: "call-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            stream: "stdout".into(),
            seq_start: 10,
            seq_end: 20,
            chunk: "abc".into(),
            dropped_bytes: 0,
            truncated: false,
        };
        assert_eq!(event.delivery(), Delivery::Replaceable);
        assert_eq!(event.tool_call_id(), Some("call-1"));
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"seq_start\":10"));
        assert!(json.contains("\"seq_end\":20"));
        assert!(json.contains("\"dropped_bytes\":0"));
    }

    #[test]
    fn code_changed_accepts_legacy_shape_and_targets_new_events() {
        let legacy: ToolEvent = serde_json::from_value(serde_json::json!({
            "type": "code_changed",
            "lines_added": 2,
            "lines_removed": 1,
            "files_created": 0,
            "files_deleted": 0,
            "file": "src/lib.rs"
        }))
        .expect("legacy event remains readable");
        assert_eq!(legacy.tool_call_id(), None);

        let current = ToolEvent::CodeChanged {
            tool_call_id: "edit-1".into(),
            turn_id: "t1".into(),
            round_num: 0,
            lines_added: 2,
            lines_removed: 1,
            files_created: 0,
            files_deleted: 0,
            file: Some("src/lib.rs".into()),
        };
        assert_eq!(current.tool_call_id(), Some("edit-1"));
        let json = serde_json::to_string(&current).expect("serialize");
        assert!(json.contains("\"turn_id\":\"t1\""));
    }

    #[test]
    fn dashboard_task_round_trip_keeps_evidence_and_accepts_legacy_rows() {
        let current = DashboardTask {
            id: "T1".into(),
            subject: "Verify".into(),
            description: "Run checks".into(),
            status: "completed".into(),
            evidence: Some("all checks passed".into()),
        };
        let json = serde_json::to_string(&current).expect("serialize dashboard task");
        assert!(json.contains("\"evidence\":\"all checks passed\""));
        let back: DashboardTask = serde_json::from_str(&json).expect("deserialize dashboard task");
        assert_eq!(back.evidence.as_deref(), Some("all checks passed"));

        let legacy: DashboardTask = serde_json::from_value(serde_json::json!({
            "id": "T2",
            "subject": "Legacy",
            "description": "",
            "status": "idle"
        }))
        .expect("legacy dashboard row without evidence remains readable");
        assert!(legacy.evidence.is_none());
    }

    #[test]
    fn delivery_classification_matches_plan() {
        assert_eq!(
            ConversationEvent::RoundDelta {
                turn_id: "t".into(),
                round_num: 0,
                kind: RoundDeltaKind::Answering,
                delta: "x".into(),
            }
            .delivery(),
            // 增量文本必须可靠投递；覆盖/合并会在断线重连时吞字。
            Delivery::Reliable
        );
        assert_eq!(
            ToolEvent::ToolStarted {
                tool_call_id: "c".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }
            .delivery(),
            Delivery::Reliable
        );
        assert_eq!(
            ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_seed: "s".into(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "idle".into(),
                streaming: false,
            }
            .delivery(),
            Delivery::Replaceable
        );
    }

    #[test]
    fn provider_tool_status_is_replaceable() {
        let event = ConversationEvent::ProviderToolStatus {
            turn_id: "t".into(),
            round_num: 0,
            call_id: "ws-1".into(),
            tool_kind: "web_search".into(),
            state: ProviderToolState::Completed,
        };
        assert_eq!(event.delivery(), Delivery::Replaceable);
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"state\":\"completed\""));
        assert!(json.contains("\"call_id\":\"ws-1\""));
    }

    #[test]
    fn operation_failed_error_round_trip() {
        let event = ControlEvent::OperationFailed {
            occurrence_id: "occ-1".into(),
            scope: ErrorScope::Tool,
            error: DomainError {
                error_id: "e-1".into(),
                code: "provider_http_500".into(),
                message: "upstream".into(),
                retryable: true,
                dedupe_key: Some("k".into()),
            },
            operation_id: Some("op-1".into()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: ControlEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back,
            ControlEvent::OperationFailed {
                scope: ErrorScope::Tool,
                ..
            }
        ));
    }

    #[test]
    fn domain_event_channel_and_delivery_delegation() {
        let ev = DomainEvent::Tool(ToolEvent::ToolProgress {
            tool_call_id: "c".into(),
            turn_id: "t".into(),
            round_num: 0,
            stream: "stderr".into(),
            seq_start: 0,
            seq_end: 1,
            chunk: "!".into(),
            dropped_bytes: 0,
            truncated: false,
        });
        assert_eq!(ev.channel(), RingingChannel::Tool);
        assert_eq!(ev.delivery(), Delivery::Replaceable);
    }
}
