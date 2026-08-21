//! SessionEngine: session lifecycle management.
//!
//! Handles: create, resume, reload_config.
//! Delegates to: lifecycle.rs for core session operations.

use super::types::*;
use crate::state::lifecycle;

/// Number of recent turns sent on session restore.
const INITIAL_LOAD_COUNT: usize = 20;

pub struct SessionEngine;

impl SessionEngine {
    pub fn new() -> Self {
        Self
    }

    /// Create a new session with a fresh seed.
    pub fn create(&self, agent: &mut crate::state::agent::AgentState, _cancel: &CancelToken) {
        lifecycle::create_session(agent);
        agent.rebind_store();
        qaqh_workspace::runtime::set_context(&agent.session.seed, agent.config.permission_level);
    }

    /// Create a new session with a pre-set seed (from CLI --seed).
    pub fn create_with_seed(
        &self,
        agent: &mut crate::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        lifecycle::create_session_with_seed(agent);
        agent.rebind_store();
        qaqh_workspace::runtime::set_context(&agent.session.seed, agent.config.permission_level);
    }

    /// Resume an existing session. Returns false if the session doesn't exist.
    pub fn resume(
        &self,
        agent: &mut crate::state::agent::AgentState,
        seed: &str,
        _cancel: &CancelToken,
    ) -> bool {
        log::info!("[SESSION] resume seed={seed}");
        if lifecycle::init_session(agent, Some(seed)) {
            agent.rebind_store();
            qaqh_workspace::runtime::set_context(
                &agent.session.seed,
                agent.config.permission_level,
            );

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
        agent: &mut crate::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        if let Ok(cfg) = qaqh_config::Config::load() {
            agent.config.api_key = cfg.api_key;
            agent.config.model = cfg.model;
            agent.config.base_url = cfg.base_url;
            agent.config.endpoint = cfg.endpoint;
            agent.config.provider_id = cfg.provider_id;
            agent.config.reasoning_effort = cfg.reasoning_effort;
            agent.config.max_tokens = cfg.max_tokens;
            agent.config.context_limit = cfg.context_limit;
            agent.config.permission_level = cfg.permission_level;
            agent.config.permission_level = cfg.permission_level;
            qaqh_workspace::workspace::load_session_workspace(&agent.session.seed);
        }
    }
}
