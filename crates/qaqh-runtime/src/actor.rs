//! In-process agent actor runner (Knife-1).
//!
//! Both main session loops and subagent loops run on daemon threads using the
//! typed `WorkerCommand` / `WriterEvent` channels. Each actor owns its thread
//! and its per-actor workspace state (`qaqh-workspace` thread-locals), so
//! session and subagent actors can run concurrently without a process-wide
//! serialization lock.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::agent::{ActorKind, SubagentSpawnSpec};
use crate::{RingingHub, SessionActivityTracker};

fn short_seed(seed: &str) -> String {
    seed.chars().take(8).collect()
}

/// Channel-side Ringing event consumer for an in-process actor.
pub(crate) fn run_inprocess_event_reader(
    event_rx: Receiver<crate::agent::types::WriterEvent>,
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
    event: crate::agent::types::WriterEvent,
) {
    let Some(hub) = hub else {
        return;
    };
    match event {
        crate::agent::types::WriterEvent::Timeline(env) => {
            if let Err(error) = hub.publish_timeline(&env.seed, env.intent) {
                log::error!("[timeline] rejected intent for {}: {error}", env.seed);
            }
        }
        crate::agent::types::WriterEvent::Ringing(env) => {
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
///
/// Process-level concerns only: per-actor workspace thread-locals, backend
/// selection, panic isolation and cleanup. Agent construction and the loop
/// itself live behind [`crate::agent::spawn_agent`] (PR-2-3).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_actor(
    seed: String,
    kind: ActorKind,
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
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

        crate::agent::spawn_agent(&seed, kind, cmd_rx, event_tx, cancel, writer_dead);

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
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
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
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
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
