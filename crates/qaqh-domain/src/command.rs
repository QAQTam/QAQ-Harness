//! 中立领域命令（DomainCommand）。
//!
//! legacy ingress（Ui2Agent → DomainCommand）与 Ringing ingress
//! （RingingCommandEnvelope → DomainCommand）分别校验并构造本类型；
//! Agent core 只消费本类型，不感知来源协议。

use serde::{Deserialize, Serialize};

use crate::channel::RingingChannel;
use crate::event::ContentRef;

/// 用户消息中的图片附件（multimodal）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// MIME type（如 "image/png"）。
    pub mime_type: String,
    /// Base64 编码的图像数据（不含 data URI 前缀）。
    pub data: String,
}

/// ask_user 表单中的单个答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub question_id: String,
    pub answer: String,
}

/// 会话工作模式（legacy `SetMode` 语义）。
/// 只有 Plan / Code 两个有效模式；`Code` 是默认值（旧值 `normal` 兼容反序列化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMode {
    Plan,
    #[serde(rename = "code", alias = "normal")]
    Code,
}

/// Control 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// 创建新会话。`close_current = true` 表示先结束当前会话
    /// （合并 legacy `CreateSession` 与 `NewSession` 语义，见决策记录 Q7）。
    SessionCreate {
        #[serde(default)]
        close_current: bool,
        /// 可选工作目录：daemon 拦截层透传给 `session.new` 的 cwd 参数，
        /// 落 SessionMeta.cwd 并触发 workspace 自动归属（None = 未分组）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// 可选工具模式预置（TUI/CLI 壳创建即锁定）：daemon 透传给
        /// `session.new`，先落盘再 spawn，worker 首轮即应用。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_mode: Option<String>,
        /// tool_mode == "custom" 时的自定义工具白名单。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        custom_tools: Vec<String>,
    },
    /// 恢复已保存会话。accepted 后由三个频道分别完成 snapshot/cursor 恢复。
    SessionResume { seed: String },
    /// 关闭指定会话。
    SessionClose { seed: String },
    /// 归档会话（标签 ×）：daemon 侧拦截——关闭 registry 实例 + meta
    /// `archived=true`（磁盘保留，左侧列表归档组可见可恢复）。
    SessionArchive { seed: String },
    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（对齐 resume 语义）。
    SessionUnarchive { seed: String },
    /// 彻底删除会话（左侧列表 ×）：daemon 侧拦截——先关实例再删磁盘目录。
    SessionDelete { seed: String },
    /// 优雅关闭整个 agent 进程。
    SessionShutdown,
    /// 重载配置（provider、model、permission 等）。
    AgentReloadConfig,
    /// 切换工具模式（standard/minimal/custom，PLAN-TOOL-MODES.md）。
    /// meta 持久化由 daemon 侧完成（persist_tool_mode），worker 只应用：
    /// set_allowed_tools + 刷新 tool_defs（模型侧工具清单源头过滤）。
    SetToolMode {
        #[serde(default)]
        tool_mode: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        custom_tools: Vec<String>,
    },
    /// 提交 ask_user 交互的答案（对应 InteractionRequested）。
    InteractionAskRespond {
        interaction_id: String,
        answers: Vec<AskAnswer>,
    },
    /// 关闭 ask_user 交互而不作答（中止被挂起的回合）。
    InteractionAskDismiss { interaction_id: String },
    /// 提交 plan review 决策（对应 PlanReviewRequested）。
    PlanReviewRespond {
        interaction_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default)]
        autonomous: bool,
    },
    /// 显式激活 skill（等价 $skill-name 提及）。
    SkillsActivate { name: String },
    /// 从磁盘重载 skill 目录并刷新目录系统消息。
    SkillsReload,
    /// 带 operation id/revision 保护的 skill UI 操作。
    /// revision 本身统一位于 Ringing command envelope。
    SkillsOperation {
        operation_id: String,
        action: String,
        name: String,
    },
}

/// Conversation 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationCommand {
    /// 发送用户消息。accepted 仅代表输入已被 session actor 接收；
    /// `TurnStarted` 是开始执行的权威事件。
    ConversationSendMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
        /// Electron main 上传后的会话附件引用；命令中不允许出现本地路径。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<ContentRef>>,
        /// 系统级注入（如子代理结果回传）：以 system 角色进入 transcript 并
        /// 触发新回合，跳过用户输入专属处理（compliance guard / 技能激活 /
        /// todo 模式切换）。false 时行为与普通用户消息完全一致。
        #[serde(default)]
        as_system: bool,
    },
    /// 取消当前回合（停止 gate 流式输出与工具执行）。
    ConversationCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// 移除指定回合。
    ConversationUndoTurn { turn_id: String },
    /// 触发上下文压缩。accepted 不代表成功；`CompactFinished` 才是终态。
    ConversationCompact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// 加载更早（已归档）的回合（只读查询，结果经 HTTP 直接返回）。
    ConversationLoadMore {
        before_turn_id: String,
        #[serde(default = "default_load_count")]
        count: u32,
    },
    /// 设置会话工作模式。
    ConversationSetMode { mode: ConversationMode },
}

fn default_load_count() -> u32 {
    20
}

/// Tool 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCommand {
    /// 前端主动触发工具执行（UI 按钮/内联操作）。
    ToolInvoke {
        tool_call_id: String,
        name: String,
        action: String,
        args: serde_json::Value,
    },
    /// 权限请求响应。必须携带对应 interaction/tool_call 的 id；
    /// revision-safe 语义统一由 Ringing command envelope 承载。
    ToolPermissionRespond {
        tool_call_id: String,
        approved: bool,
        #[serde(default)]
        trust_folder: bool,
    },
}

/// 统一领域命令入口。`channel()` 决定命令进入哪个 actor/router。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum DomainCommand {
    Control(ControlCommand),
    Conversation(ConversationCommand),
    Tool(ToolCommand),
}

impl DomainCommand {
    pub fn channel(&self) -> RingingChannel {
        match self {
            DomainCommand::Control(_) => RingingChannel::Control,
            DomainCommand::Conversation(_) => RingingChannel::Conversation,
            DomainCommand::Tool(_) => RingingChannel::Tool,
        }
    }
}
