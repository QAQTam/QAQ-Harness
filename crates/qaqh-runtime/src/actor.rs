//! In-process agent actor runner (Knife-1).
//!
//! Both main session loops and subagent loops run on daemon threads using the
//! typed `WorkerCommand` / `WriterEvent` channels. Each actor owns its thread
//! and its per-actor workspace state (`qaqh-workspace` thread-locals), so
//! session and subagent actors can run concurrently without a process-wide
//! serialization lock.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::{RingingHub, SessionActivityTracker};

#[derive(Clone)]
pub(crate) struct SubagentSpawnSpec {
    pub(crate) tools: Vec<String>,
    pub(crate) model: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) ephemeral: bool,
}

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

/// Apply subagent config defaults (`cfg.subagent.*`) and explicit overrides.
///
/// Shared by the legacy process worker and the in-process actor so the two
/// paths cannot drift. Explicit overrides win over settings defaults.
pub(crate) fn apply_subagent_config(
    agent: &mut qaqh_msgloop::state::agent::AgentState,
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

fn short_seed(seed: &str) -> String {
    seed.chars().take(8).collect()
}

/// Channel-side Ringing event consumer for an in-process actor.
pub(crate) fn run_inprocess_event_reader(
    event_rx: Receiver<qaqh_msgloop::ringing_v1::types::WriterEvent>,
    seed: String,
    generation: u64,
    activity: SessionActivityTracker,
    hub: Option<Arc<RingingHub>>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for event in event_rx {
            publish_worker_event(hub.as_deref(), &activity, &seed, generation, event);
        }
    }));
    if let Err(panic) = result {
        log::error!(
            "[AGENT:{}] in-process event reader panicked: {:?}",
            short_seed(&seed),
            panic
        );
    }
    if let Some(update) = activity.disconnect(&seed, generation) {
        crate::activity::publish_activity(hub.as_deref(), &update);
    }
}

fn publish_worker_event(
    hub: Option<&RingingHub>,
    activity: &SessionActivityTracker,
    seed: &str,
    generation: u64,
    event: qaqh_msgloop::ringing_v1::types::WriterEvent,
) {
    let Some(hub) = hub else {
        return;
    };
    match event {
        qaqh_msgloop::ringing_v1::types::WriterEvent::Timeline(env) => {
            if let Err(error) = hub.publish_timeline(&env.seed, env.intent) {
                log::error!("[timeline] rejected intent for {}: {error}", env.seed);
            }
        }
        qaqh_msgloop::ringing_v1::types::WriterEvent::Ringing(env) => {
            let domain: qaqh_domain::DomainEvent = env.event.into();
            let domain = crate::registry::externalize_large_content(hub, &env.seed, domain);
            let _ =
                hub.publish_with_causation(&env.seed, domain.clone(), env.causation_id.as_deref());
            if let Some(observe) = crate::activity::domain_activity_observe(&domain)
                && let Some(activity) = activity.observe(seed, generation, &observe)
            {
                crate::activity::publish_activity(Some(hub), &activity);
            }
        }
    }
}

/// Shared actor body for main session loops and subagent loops.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_actor(
    seed: String,
    kind: ActorKind,
    cmd_rx: Receiver<qaqh_msgloop::ringing_v1::types::WorkerCommand>,
    event_tx: SyncSender<qaqh_msgloop::ringing_v1::types::WriterEvent>,
    cancel: qaqh_msgloop::ringing_v1::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    workspace_mode: String,
    workspace_env: Option<(String, String)>,
) {
    let is_subagent = matches!(&kind, ActorKind::Subagent(_));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Per-actor state is thread-local in qaqh-workspace, so actors no
        // longer need a process-wide serialization lock to run concurrently.
        qaqh_workspace::set_actor_context("", &seed);

        if workspace_mode.eq_ignore_ascii_case("wsl") {
            if let Some((endpoint, token)) = workspace_env.as_ref()
                && !endpoint.is_empty()
                && !token.is_empty()
            {
                qaqh_workspace::install_workspace_backend(Arc::new(
                    qaqh_workspace::HttpToolExecutionBackend::new(endpoint.clone(), token.clone()),
                ));
            }
        } else {
            qaqh_workspace::use_local_workspace_backend();
        }

        if is_subagent {
            qaqh_workspace::authorization::set_subagent_sandbox(true);
        }

        let mut agent = qaqh_msgloop::state::agent::AgentState::new(
            qaqh_config::Config::load().unwrap_or_default(),
        );

        // Both session actors and subagent actors use an actor-private
        // ToolManager so daemon-side `skills.list_tools` stays stable while a
        // loop is running.
        let registrars = qaqh_msgloop::state::agent::agent_tool_registrars();
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
        manager.apply_init(allowed_tools, &seed);
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
                agent.session.seed = seed.clone();
                agent.session.created_at = qaqh_session::SessionManager::now_epoch();
                log::info!(
                    "[SUBAGENT-ACTOR] starting in-process subagent seed={} tools={:?} ephemeral={}",
                    seed,
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
                    "[SESSION-ACTOR] starting in-process session seed={} resume={:?} new={:?}",
                    seed,
                    resume_seed,
                    new_seed
                );
            }
        }

        let mut loop_ = qaqh_msgloop::ringing_v1::loop_core::Loop::from_channels(
            agent,
            cmd_rx,
            event_tx,
            cancel,
            writer_dead,
        );
        loop_.run();

        qaqh_workspace::clear_actor_context();
        cleanup_actor_state(is_subagent);
        log::info!("[ACTOR] in-process agent {seed} exited");
    }));

    if let Err(panic) = result {
        // Failure must not leak actor tooling/sandbox state to the daemon.
        qaqh_workspace::clear_actor_context();
        cleanup_actor_state(is_subagent);
        log::error!("[ACTOR] in-process agent {} panicked: {:?}", seed, panic);
    }
}

/// Knife-1 step-1 subagent actor wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_subagent_actor(
    seed: String,
    spec: SubagentSpawnSpec,
    cmd_rx: Receiver<qaqh_msgloop::ringing_v1::types::WorkerCommand>,
    event_tx: SyncSender<qaqh_msgloop::ringing_v1::types::WriterEvent>,
    cancel: qaqh_msgloop::ringing_v1::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    workspace_mode: String,
    workspace_env: Option<(String, String)>,
) {
    run_actor(
        seed,
        ActorKind::Subagent(spec),
        cmd_rx,
        event_tx,
        cancel,
        writer_dead,
        workspace_mode,
        workspace_env,
    );
}

/// Knife-1 step-2a session actor wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_session_actor(
    seed: String,
    resume_seed: Option<String>,
    new_seed: Option<String>,
    timeline_turn_count: u64,
    cmd_rx: Receiver<qaqh_msgloop::ringing_v1::types::WorkerCommand>,
    event_tx: SyncSender<qaqh_msgloop::ringing_v1::types::WriterEvent>,
    cancel: qaqh_msgloop::ringing_v1::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    workspace_mode: String,
    workspace_env: Option<(String, String)>,
) {
    run_actor(
        seed,
        ActorKind::Session {
            resume_seed,
            new_seed,
            timeline_turn_count,
        },
        cmd_rx,
        event_tx,
        cancel,
        writer_dead,
        workspace_mode,
        workspace_env,
    );
}

fn cleanup_actor_state(is_subagent: bool) {
    qaqh_workspace::runtime::clear_actor_tool_manager();
    if is_subagent {
        qaqh_workspace::authorization::set_subagent_sandbox(false);
    }
    qaqh_workspace::set_cancel(false);
}
