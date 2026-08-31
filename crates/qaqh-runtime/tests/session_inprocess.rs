//! Knife-1 step-2 regression: normal session agents must also run as in-process
//! daemon actors, not as `qaqh agent --seed` child processes.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// SessionManager 是全局单例：同测试进程只 init 一次，两个用例共享。
static INIT: Once = Once::new();

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
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
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

#[test]
fn cross_session_cancel_does_not_leak() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    // 前序用例的 shutdown_all 会置进程级全局取消 flag，先复位本进程状态。
    qaqh_workspace::clear_cancel();
    let root = std::env::temp_dir().join(format!(
        "qaqh-cross-cancel-test-{}-{}",
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
    INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    let hub = Arc::new(RingingHub::new("cross-cancel-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    let seed_a = format!("cross-cancel-a-{}", std::process::id());
    let seed_b = format!("cross-cancel-b-{}", std::process::id());
    registry.spawn_new(&seed_a).expect("spawn session a");
    registry.spawn_new(&seed_b).expect("spawn session b");

    // 会话 A 经 registry 真实 interrupt 路径取消：per-session CancelToken +
    // 会话键控取消表（PR-3-4：不再置进程级全局 flag）。
    let interrupt = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        "cross-cancel-test-1",
        "cross-cancel-client",
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
        ),
    );
    registry.send_ringing(&seed_a, &interrupt).expect("interrupt a");

    // 无会话线程视角：进程级全局 flag 不被会话级 interrupt 触碰。
    assert!(
        !qaqh_workspace::is_cancel(),
        "session-scoped interrupt must not set the process-wide cancel flag"
    );

    // 会话 B 的工具执行视角（execute 路径绑定的 runtime ctx = worker 语义）：
    // cancel 检查在 admit 之前——未注册工具报 Unknown tool 而非 Cancelled，
    // 即证明 A 的取消没有泄漏进 B 的执行上下文。
    let ctx_b = qaqh_workspace::runtime::ToolCtx::admitted(&seed_b);
    let result = qaqh_workspace::execution::execute_with_context(
        "read",
        "",
        "{}",
        "cross-cancel-b-tool",
        None,
        &ctx_b,
    );
    assert!(
        !result.content.contains("CANCELLED"),
        "session B tool execution must not be cancelled by session A's interrupt: {}",
        result.content
    );
    assert!(
        result.content.contains("path is required"),
        "expected the read tool's args validation (cancel check passed first): {}",
        result.content
    );

    registry.shutdown_all();
}
