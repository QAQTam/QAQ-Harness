//! Knife-1 step-2 regression: normal session agents must also run as in-process
//! daemon actors, not as `qaqh agent --seed` child processes.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn session_spawns_inprocess_and_receives_created_event() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-inprocess-test-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    static INIT: Once = Once::new();
    INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    let seed = format!("session-inproc-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("session-inprocess-test"));
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

    registry.spawn_new(&seed).expect("spawn in-process session");
    assert!(
        registry.is_running(&seed),
        "registry must track the session actor"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match event_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => {
                    break;
                }
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("session actor emitted no Created event: {error}"),
        }
    }
    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
}

#[test]
fn session_spawn_has_no_process_spawn_in_source() {
    let root = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{root}/src/registry.rs"))
        .expect("read qaqh-runtime registry.rs");
    let body = source
        .lines()
        .skip_while(|line| !line.contains("fn spawn_with("))
        .take_while(|line| !line.contains("/// 发送 Ringing worker 命令帧"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !body.contains("current_exe"),
        "session spawn must not use current_exe: {body}"
    );
    assert!(
        !body.contains("command.arg(\"agent\")"),
        "session spawn must not start the qaqh-daemon agent subcommand: {body}"
    );
}
