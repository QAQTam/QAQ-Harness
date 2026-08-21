use serde::{Deserialize, Serialize};

/// Activation state of a single skill within a session.
///
/// Tracks whether a skill is currently loaded and available in the
/// agent's context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillSessionEntryState {
    /// Skill is loaded and active in the current session.
    Active,
    /// Skill was previously available but is now unavailable
    /// (e.g. file deleted, scope changed).
    Unavailable,
}

/// Runtime tracking for one skill in a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionEntry {
    /// Skill name matching SKILL.md metadata.
    pub name: String,
    /// Monotonic counter for determining activation order across sessions.
    pub activation_order: u64,
    /// Path or identifier of the skill source directory (project/user scope).
    pub source: String,
    /// Current activation state.
    pub state: SkillSessionEntryState,
}

/// Snapshot of skill activation state for a session, persisted in meta.json.
///
/// Version 2 adds `context_epoch` and `operation_revision` for tracking
/// skill activation/deactivation across context compaction cycles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionStateV2 {
    /// Schema version (always 2).
    pub version: u8,
    /// Epoch counter incremented on context compaction. Used to detect
    /// whether stale skill contexts need refresh.
    pub context_epoch: u64,
    /// Monotonic revision counter for operation ordering across restarts.
    pub operation_revision: u64,
    /// Active skill entries in activation order.
    pub entries: Vec<SkillSessionEntry>,
}

impl Default for SkillSessionStateV2 {
    fn default() -> Self {
        Self {
            version: 2,
            context_epoch: 0,
            operation_revision: 0,
            entries: Vec::new(),
        }
    }
}

/// Session metadata — unified persistence + runtime state.
///
/// Fields marked `#[serde(skip)]` are runtime-only and not persisted to meta.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    // ── Persisted fields ──
    pub seed: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub message_count: usize,
    /// Number of conversation turns (one user query + its assistant/tool chain).
    #[serde(default)]
    pub turn_count: usize,
    #[serde(default)]
    pub last_summary: String,
    /// Number of earliest turns compacted (skipped in LLM context).
    #[serde(default)]
    pub compact_skip: usize,
    /// Agent operating mode: 0=Code(默认), 1=Plan, 2=Code(旧编码兼容).
    /// Persisted so PLAN/CODE mode survives agent restart within the same session.
    #[serde(default)]
    pub mode: u8,
    /// 工具模式：standard | minimal | custom（PLAN-TOOL-MODES.md）。
    /// 空串 = standard（旧 session 零迁移兼容）；custom 时 `custom_tools` 生效。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_mode: String,
    /// 创造模式的自定义工具白名单（仅 tool_mode == "custom" 时生效）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_tools: Vec<String>,
    /// 归档标记：标签 × 归档后置 true（会话停止、不出现在标签条，左侧
    /// 列表归档组可见可恢复）。旧 meta.json 缺失该字段 = 未归档。
    #[serde(default)]
    pub archived: bool,
    /// 临时会话标记（子代理）：`index=false` 写入时置 true。会话关闭时
    /// 整个目录被删除（用完即走，磁盘零残留）；正规会话恒为 false。
    /// 旧 meta.json 缺失该字段 = 非临时会话。
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub skills: SkillSessionStateV2,
    /// Provider-confirmed usage accumulated across model requests in this session.
    #[serde(default)]
    pub usage_totals: crate::UsageInfo,
    /// Last provider-confirmed request usage, used to restore the live Info panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<crate::UsageInfo>,
    /// Number of model requests included in `usage_totals`.
    #[serde(default)]
    pub usage_requests: u32,
    /// Number of requests whose provider explicitly returned cache usage.
    #[serde(default)]
    pub cache_reported_requests: u32,

    // ── Runtime fields (not persisted) ──
    /// If set, this seed is passed as a CLI argument to the agent subprocess for auto-restore on startup.
    #[serde(skip)]
    pub resume_seed: Option<String>,
    /// Cumulative tokens consumed across all turns.
    #[serde(skip)]
    pub tokens: u64,
    /// 会话标题（**首轮后生成一次即冻结**，对齐主流 AI 工具行为；persisted）。
    /// 生成链路：worker 首 turn 完成后异步 LLM 总结用户需求（失败降级为
    /// 首条用户消息截断）→ 写盘 → daemon 广播 `SessionMetaChanged` → 前端刷新。
    pub title: Option<String>,
    /// 会话创建时的工作目录（canonical path，persisted）。Workspace 归属判定
    /// 基础：新会话 cwd 位于某 workspace path 内自动 attach；旧 meta.json 缺省
    /// None = 未分组（零迁移兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 上下文统计快照（可再生缓存：compact/dashboard 时重算）。原独立文件
    /// `sessions/{seed}/context_stats.json` 已退役，并入 meta.json。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_stats: Option<serde_json::Value>,
    /// True if session was restored from disk — system prompt preserved.
    #[serde(skip)]
    pub from_resume: bool,
}
impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            seed: String::new(),
            created_at: 0,
            updated_at: 0,
            model: String::new(),
            effort: None,
            message_count: 0,
            turn_count: 0,
            last_summary: String::new(),
            compact_skip: 0,
            mode: 0,
            tool_mode: String::new(),
            custom_tools: Vec::new(),
            archived: false,
            ephemeral: false,
            skills: SkillSessionStateV2::default(),
            usage_totals: crate::UsageInfo::default(),
            last_usage: None,
            usage_requests: 0,
            cache_reported_requests: 0,
            resume_seed: None,
            tokens: 0,
            title: None,
            cwd: None,
            context_stats: None,
            from_resume: false,
        }
    }
}

impl SessionMeta {
    pub fn effective_cache_reported_requests(&self) -> u32 {
        if self.cache_reported_requests == 0
            && self.usage_requests > 0
            && self
                .usage_totals
                .prompt_cache_hit_tokens
                .saturating_add(self.usage_totals.prompt_cache_miss_tokens)
                > 0
        {
            self.usage_requests
        } else {
            self.cache_reported_requests
        }
    }

    pub fn record_usage(&mut self, usage: &crate::UsageInfo) {
        self.cache_reported_requests = self.effective_cache_reported_requests();
        if self.cache_reported_requests > 0 {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_totals.prompt_tokens = self
            .usage_totals
            .prompt_tokens
            .saturating_add(usage.prompt_tokens);
        self.usage_totals.completion_tokens = self
            .usage_totals
            .completion_tokens
            .saturating_add(usage.completion_tokens);
        self.usage_totals.total_tokens = self
            .usage_totals
            .total_tokens
            .saturating_add(usage.total_tokens);
        self.usage_totals.prompt_cache_hit_tokens = self
            .usage_totals
            .prompt_cache_hit_tokens
            .saturating_add(usage.prompt_cache_hit_tokens);
        self.usage_totals.prompt_cache_miss_tokens = self
            .usage_totals
            .prompt_cache_miss_tokens
            .saturating_add(usage.prompt_cache_miss_tokens);
        self.usage_totals.reasoning_tokens = self
            .usage_totals
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens);
        if usage.cache_usage_reported == Some(true) {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_requests = self.usage_requests.saturating_add(1);
        if usage.cache_usage_reported == Some(true) {
            self.cache_reported_requests = self.cache_reported_requests.saturating_add(1);
        }
        self.last_usage = Some(usage.clone());
        self.tokens = self.usage_totals.total_tokens.into();
    }

    pub fn reset_usage(&mut self) {
        self.tokens = 0;
        self.usage_totals = crate::UsageInfo::default();
        self.last_usage = None;
        self.usage_requests = 0;
        self.cache_reported_requests = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_session_metadata_defaults_to_empty_skill_state_v2() {
        let meta: SessionMeta = serde_json::from_str(
            r#"{
            "seed":"s","created_at":0,"updated_at":0,"model":"m",
            "message_count":0,"turn_count":0,"last_summary":"","compact_skip":0,"mode":0
        }"#,
        )
        .unwrap();
        assert_eq!(meta.skills.version, 2);
        assert!(meta.skills.entries.is_empty());
        assert_eq!(meta.cache_reported_requests, 0);
    }

    #[test]
    fn legacy_session_metadata_defaults_tool_mode_to_standard() {
        // 旧 meta.json 无 tool_mode/custom_tools → 零迁移兼容（standard）。
        let meta: SessionMeta = serde_json::from_str(
            r#"{
            "seed":"s","created_at":0,"updated_at":0,"model":"m",
            "message_count":0,"turn_count":0,"last_summary":"","compact_skip":0,"mode":1
        }"#,
        )
        .unwrap();
        assert_eq!(meta.tool_mode, "");
        assert!(meta.custom_tools.is_empty());
    }

    #[test]
    fn tool_mode_round_trips_through_json() {
        let mut meta = SessionMeta::default();
        meta.tool_mode = "custom".to_string();
        meta.custom_tools = vec!["bash".to_string(), "edit".to_string()];
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_mode, "custom");
        assert_eq!(back.custom_tools, vec!["bash", "edit"]);
        // standard 时空 custom_tools 不落盘（skip_serializing_if）
        let mut meta2 = SessionMeta::default();
        meta2.tool_mode = "minimal".to_string();
        let json2 = serde_json::to_string(&meta2).unwrap();
        assert!(!json2.contains("custom_tools"));
    }

    #[test]
    fn usage_tracks_cache_reporting_separately_from_hit_rate() {
        let mut meta = SessionMeta::default();
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 100,
            prompt_cache_miss_tokens: 100,
            cache_usage_reported: Some(true),
            ..Default::default()
        });
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 50,
            ..Default::default()
        });

        assert_eq!(meta.usage_requests, 2);
        assert_eq!(meta.cache_reported_requests, 1);
        assert_eq!(meta.usage_totals.cache_usage_reported, Some(true));
        assert_eq!(meta.usage_totals.prompt_cache_hit_tokens, 0);
        assert_eq!(meta.usage_totals.prompt_cache_miss_tokens, 100);
    }

    #[test]
    fn legacy_cache_totals_infer_full_request_coverage() {
        let meta = SessionMeta {
            usage_requests: 3,
            usage_totals: crate::UsageInfo {
                prompt_cache_hit_tokens: 60,
                prompt_cache_miss_tokens: 40,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(meta.effective_cache_reported_requests(), 3);
    }
}
