//! `spawn_agent` — in-process agent 构造唯一入口（PR-2-3）。
//!
//! 装配知识（权威配置读、`AgentState` 构造、图片能力快照、actor 私有
//! ToolManager）收敛在本模块：actor.rs 只保留进程级事项（thread-locals、
//! panic 隔离、清理），registry.rs 只持实例簿记与 channel 两端。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::agent::loop_core::Loop;
use crate::agent::state::agent::{AgentState, agent_tool_registrars};
use crate::agent::types::{CancelToken, WorkerCommand, WriterEvent};

#[derive(Clone)]
pub(crate) enum ActorKind {
    /// A normal session worker resumed with an existing seed or created with a
    /// preset seed.
    Session {
        resume_seed: Option<String>,
        new_seed: Option<String>,
        timeline_turn_count: u64,
    },
    Subagent(SubagentSpawnSpec),
}

#[derive(Clone)]
pub(crate) struct SubagentSpawnSpec {
    pub(crate) tools: Vec<String>,
    pub(crate) model: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) ephemeral: bool,
}

/// Apply subagent config defaults (`cfg.subagent.*`) and explicit overrides.
/// Explicit overrides win over settings defaults.
fn apply_subagent_config(
    agent: &mut AgentState,
    model: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
) {
    let sub = agent.config.subagent.clone();
    if !sub.model.is_empty() && model.is_none() {
        agent.config.model = sub.model;
    }
    if !sub.base_url.is_empty() && base_url.is_none() {
        agent.config.base_url = sub.base_url;
    }
    if !sub.api_key.is_empty() {
        agent.config.api_key = sub.api_key;
    }
    if sub.max_tokens > 0 && max_tokens.is_none() {
        agent.config.max_tokens = sub.max_tokens;
    }
    if let Some(model) = model {
        agent.config.model = model.to_string();
    }
    if let Some(base_url) = base_url {
        agent.config.base_url = base_url.to_string();
    }
    if let Some(max_tokens) = max_tokens {
        agent.config.max_tokens = max_tokens;
    }
}

/// Assemble and run one in-process agent until its command channel closes:
/// 权威配置读 → `AgentState` → 图片能力快照 → actor 私有 ToolManager →
/// kind 覆写（subagent 配置 / session seed）→ [`Loop`]。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_agent(
    seed: &str,
    kind: ActorKind,
    cmd_rx: Receiver<WorkerCommand>,
    event_tx: SyncSender<WriterEvent>,
    cancel: CancelToken,
    writer_dead: Arc<AtomicBool>,
) {
    // 权威配置读收敛 config 单入口（PR-1-8 同向）；图片能力快照
    // 就地注入（PR-1-10：actor 进程的工具调用路径零磁盘读）。
    let agent_config = qaqh_config::watch::authoritative().unwrap_or_default();
    let mut agent = AgentState::new(agent_config);
    agent.refresh_image_capability();

    // Both session actors and subagent actors use an actor-private
    // ToolManager so daemon-side `skills.list_tools` stays stable while a
    // loop is running.
    let registrars = agent_tool_registrars();
    let mut manager = qaqh_workspace::registration::build_tool_manager(&registrars);
    let allowed_tools: Vec<String> = match &kind {
        ActorKind::Subagent(spec) => {
            let mut tools = spec.tools.clone();
            if !tools.iter().any(|tool| tool == "skills") {
                tools.push("skills".to_string());
            }
            tools
        }
        // Empty allowlist means all tools for normal sessions.
        ActorKind::Session { .. } => Vec::new(),
    };
    manager.apply_init(allowed_tools, seed);
    qaqh_workspace::runtime::install_actor_tool_manager(manager);
    agent.tool_defs = qaqh_workspace::runtime::all_tools();

    match kind {
        ActorKind::Subagent(spec) => {
            agent.ephemeral = spec.ephemeral;
            apply_subagent_config(
                &mut agent,
                spec.model.as_deref(),
                spec.base_url.as_deref(),
                spec.max_tokens,
            );
            agent.session.seed = seed.to_string();
            agent.session.created_at = qaqh_session::SessionManager::now_epoch();
            log::info!(
                "[SUBAGENT-ACTOR] starting in-process subagent seed={seed} tools={:?} ephemeral={}",
                spec.tools,
                spec.ephemeral
            );
        }
        ActorKind::Session {
            resume_seed,
            new_seed,
            timeline_turn_count,
        } => {
            if let Some(ref resume) = resume_seed {
                agent.session.resume_seed = Some(resume.clone());
            }
            if let Some(ref new) = new_seed {
                agent.session.seed = new.clone();
                agent.session.created_at = qaqh_session::SessionManager::now_epoch();
            }
            // Carry the timeline turn-count floor per-agent instead of via
            // a process env var, so concurrent actors each see their own.
            agent.timeline_turn_count = timeline_turn_count;
            log::info!(
                "[SESSION-ACTOR] starting in-process session seed={seed} resume={resume_seed:?} new={new_seed:?}"
            );
        }
    }

    let mut loop_ = Loop::from_channels(agent, cmd_rx, event_tx, cancel, writer_dead);
    loop_.run();
}
