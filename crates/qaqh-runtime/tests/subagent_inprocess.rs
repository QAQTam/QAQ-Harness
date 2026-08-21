//! Knife-1 step-1 regression: `AgentRegistry::spawn_subagent` must create a
//! daemon-thread Ringing Loop, not a `qaqh agent --seed` child process.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn spawn_subagent_runs_inprocess_loops_and_shutdown_signals_all() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-subagent-inprocess-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        // QAQH_DATA_DIR is process-wide; this integration-test binary contains
        // a single test and holds TEST_LOCK around all global state mutations.
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    static INIT: Once = Once::new();
    INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    // Daemon process manager snapshot: must stay stable while the actor
    // installs its private ToolManager.
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);
    let process_tools = qaqh_workspace::runtime::process_all_tool_names();

    let seed = format!("sub-inproc-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-inprocess-test"));
    let mut control_rx = hub.subscribe(qaqh_domain::RingingChannel::Control);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(envelope) = control_rx.blocking_recv() {
            if event_tx.send(envelope).is_err() {
                break;
            }
        }
    });
    let mut registry = AgentRegistry::new();
    registry.attach_ringing(hub);

    registry
        .spawn_subagent(&seed, &[], None, None, None)
        .expect("spawn in-process subagent");
    assert!(registry.is_running(&seed), "registry must track the actor");

    // The actor emits SessionStateChanged(Created) through the same hub path as
    // a process worker's stdout reader. Its private ToolManager is visible to
    // actor tool calls but must not replace the daemon process snapshot.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match event_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => break,
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("subagent actor emitted no Created event: {error}"),
        }
    }

    // The process-level snapshot must stay stable and must NOT pick up the
    // actor's private manager (which is now thread-local per actor): the daemon
    // `skills.list_tools` view reflects only the daemon's own registrar set, so
    // a running subagent actor cannot leak its tool set into the daemon.
    assert_eq!(
        qaqh_workspace::runtime::process_all_tool_names(),
        process_tools,
        "daemon process tool snapshot must be stable while an actor is running"
    );
    assert!(
        !qaqh_workspace::runtime::process_all_tool_names()
            .iter()
            .any(|name| name == "spawn_subagent"),
        "actor-private tools (spawn_subagent) must not leak into the daemon snapshot"
    );

    // Knife-1 step 2: a second subagent must run concurrently (per-actor
    // thread-local state), not queue behind a process-wide serialization lock.
    // shutdown_all must signal every instance before joining any of them, or
    // this test hangs.
    let queued_seed = format!("sub-concurrent-{}", std::process::id());
    registry
        .spawn_subagent(&queued_seed, &[], None, None, None)
        .expect("spawn concurrent in-process subagent");
    assert!(registry.is_running(&queued_seed));
    // Both actors alive at once proves concurrency (previously the second
    // actor blocked on SUBAGENT_ACTOR_SERIAL until the first exited).
    assert!(
        registry.is_running(&seed) && registry.is_running(&queued_seed),
        "concurrent subagents must both be running"
    );

    // The flush-stop race the old serialized path needed is gone: a subagent
    // spawned while another is mid-turn must still emit its own Created event
    // promptly. Under `ACTOR_SERIAL` this recv would time out because the
    // second actor's thread blocked behind the first actor's loop.
    let second_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_second_created = false;
    while Instant::now() < second_deadline {
        match event_rx.recv_timeout(second_deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == queued_seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => {
                    saw_second_created = true;
                    break;
                }
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("second actor emitted no Created event: {error}"),
        }
    }
    assert!(
        saw_second_created,
        "second subagent must reach Created while the first is still running (concurrency)"
    );

    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
    assert!(!registry.is_running(&queued_seed));
}

#[test]
fn spawn_subagent_does_not_go_through_process_spawn() {
    let root = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{root}/src/registry.rs"))
        .expect("read qaqh-runtime registry.rs");
    let body = source
        .lines()
        .skip_while(|line| !line.contains("pub fn spawn_subagent("))
        .skip(1)
        .take_while(|line| !line.contains("fn spawn_subagent_inprocess("))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !body.contains("Command::new"),
        "spawn_subagent must not construct a child process: {body}"
    );
    assert!(
        !body.contains("spawn_with("),
        "spawn_subagent must not route through the process worker path: {body}"
    );
    assert!(
        body.contains("spawn_subagent_inprocess("),
        "spawn_subagent must delegate to the in-process actor path: {body}"
    );
}
