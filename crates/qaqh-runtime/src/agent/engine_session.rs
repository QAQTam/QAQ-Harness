//! SessionEngine: session lifecycle management.
//!
//! Handles: create, resume, reload_config.
//! Delegates to: lifecycle.rs for core session operations.

use super::types::*;
use crate::agent::state::lifecycle;

/// Number of recent turns sent on session restore.
const INITIAL_LOAD_COUNT: usize = 20;

pub struct SessionEngine;

impl SessionEngine {
    pub fn new() -> Self {
        Self
    }

    /// Create a new session with a fresh seed.
    pub fn create(&self, agent: &mut crate::agent::state::agent::AgentState, _cancel: &CancelToken) {
        lifecycle::create_session(agent);
    }

    /// Create a new session with a pre-set seed (from CLI --seed).
    pub fn create_with_seed(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        lifecycle::create_session_with_seed(agent);
    }

    /// Resume an existing session. Returns false if the session doesn't exist.
    pub fn resume(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        seed: &str,
        _cancel: &CancelToken,
    ) -> bool {
        log::info!("[SESSION] resume seed={seed}");
        if lifecycle::init_session(agent, Some(seed)) {
            // Restore persisted agent mode（0=Code 也重置：避免进程内已切
            // plan/code 后恢复默认会话仍停留在旧模式——前后端显示/拦截一致）。
            let saved_mode = agent.session.mode;
            qaqh_workspace::runtime::set_mode(saved_mode);

            // SessionRestored is emitted by the caller (Loop::dispatch)
            // since it needs access to the emitter.
            let loaded = INITIAL_LOAD_COUNT.min(agent.msg.turn_count() as usize);
            log::info!(
                "[SESSION] restored, {} turns (has_more={})",
                loaded,
                agent.msg.turn_count() as usize > INITIAL_LOAD_COUNT
            );
            true
        } else {
            log::info!("[SESSION] init_session returned false for {seed}");
            false
        }
    }

    /// Reload config from disk and apply to agent.
    pub fn reload_config(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        // P2-D1：磁盘为权威源；磁盘读失败时回退单写口广播的最新镜像。
        // 权威读收敛到 config crate 单入口（PR-1-8）。
        if let Some(cfg) = qaqh_config::watch::authoritative() {
            Self::apply_config(cfg, agent);
            qaqh_workspace::workspace::load_session_workspace(&agent.session.seed);
        }
    }

    /// 把磁盘配置的热同步字段整体拷入运行中 agent（[`Self::reload_config`] 的
    /// 唯一实现，取所有权避免逐字段 clone）。
    ///
    /// 2026-08-25 复盘（docs/config-revamp-plan.md §1 R1）：此前手工逐字段
    /// 拷贝漏掉 `auto_compact_threshold`，压缩 gate（engine_turn.rs:910）读的
    /// 又是 `agent.config.auto_compact_threshold` → 活会话永远按旧阈值提前
    /// 压缩，而 UI/磁盘均已是新值（三方不一致，极难排查）。
    /// 单测 `applies_all_hot_fields` 锁定字段清单：**新增热同步字段时必须
    /// 同步补本测试断言**，否则该字段在运行中会话上静默不生效。
    /// （P2-D1 落地 watch + `From<&Config>` 后本函数将被整体快照赋值取代。）
    pub(crate) fn apply_config(
        cfg: qaqh_config::Config,
        agent: &mut crate::agent::state::agent::AgentState,
    ) {
        agent.config.api_key = cfg.api_key;
        agent.config.model = cfg.model;
        agent.config.base_url = cfg.base_url;
        agent.config.endpoint = cfg.endpoint;
        agent.config.provider_id = cfg.provider_id;
        agent.config.reasoning_effort = cfg.reasoning_effort;
        agent.config.max_tokens = cfg.max_tokens;
        agent.config.context_limit = cfg.context_limit;
        agent.config.auto_compact_threshold = cfg.auto_compact_threshold;
        agent.config.permission_level = cfg.permission_level;
        // (provider, endpoint) 解析随配置刷新（PR-1-9：engines 只读字段）。
        agent.refresh_endpoint_spec();
        // 图片能力快照随配置刷新（PR-1-10：工具调用路径零磁盘读）。
        agent.refresh_image_capability();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁定 reload 热同步字段清单（R1 回归守卫：曾漏 auto_compact_threshold，
    /// 导致活会话永远用旧阈值提前压缩）。新增同步字段 → 同步补断言。
    #[test]
    fn applies_all_hot_fields() {
        let new_cfg = qaqh_config::Config {
            api_key: "sk-new".into(),
            model: "model-new".into(),
            base_url: "https://new.example/v1".into(),
            endpoint: "openai".into(),
            provider_id: "prov-new".into(),
            reasoning_effort: "max".into(),
            max_tokens: 123_456,
            context_limit: 2_000_000,
            auto_compact_threshold: 0.95,
            permission_level: 3,
            ..Default::default()
        };
        let mut agent = crate::agent::state::agent::AgentState::new(qaqh_config::Config {
            api_key: "sk-old".into(),
            model: "model-old".into(),
            base_url: "https://old.example/v1".into(),
            provider_id: "prov-old".into(),
            reasoning_effort: "low".into(),
            max_tokens: 4096,
            context_limit: 10_000,
            auto_compact_threshold: 0.3,
            permission_level: 1,
            ..Default::default()
        });

        SessionEngine::apply_config(new_cfg, &mut agent);

        assert_eq!(agent.config.api_key, "sk-new");
        assert_eq!(agent.config.model, "model-new");
        assert_eq!(agent.config.base_url, "https://new.example/v1");
        assert_eq!(agent.config.endpoint, "openai");
        assert_eq!(agent.config.provider_id, "prov-new");
        assert_eq!(agent.config.reasoning_effort, "max");
        assert_eq!(agent.config.max_tokens, 123_456);
        assert_eq!(agent.config.context_limit, 2_000_000);
        // R1 主角：阈值必须随 reload 同步到运行中会话。
        assert!((agent.config.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert_eq!(agent.config.permission_level, 3);
    }
}
