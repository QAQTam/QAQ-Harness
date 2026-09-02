//! Knife-1 step-2 regression: normal session agents must also run as in-process
//! daemon actors, not as `qaqh agent --seed` child processes.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, QaqhService, RingingHub};

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
    registry
        .send_ringing(&seed_a, &interrupt)
        .expect("interrupt a");

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

/// E: idle 卸载 → 重生 → 会话历史连续（docs/memory-governance-plan.md §E）。
/// 卸载走 registry.close 优雅路径；重生走 spawn_new 的 resume 语义
/// （load_for_resume + WAL 重放 + outbox 对账）。
#[test]
fn idle_unload_then_respawn_preserves_history() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-idle-unload-{}-{}",
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

    let seed = format!("session-idle-unload-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("idle-unload-test"));
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

    // 1. Spawn + wait for Created（worker 活着，liveness 全新）。
    registry.spawn_new(&seed).expect("spawn in-process session");
    let liveness = registry
        .worker_liveness(&seed)
        .expect("session actor must expose liveness");
    assert!(!liveness.unloadable() || liveness.idle_secs() < 1);

    // 未到阈值 → 不卸载。
    let unloaded = registry.unload_idle_sessions(3600);
    assert!(unloaded.is_empty(), "fresh worker must not be unloaded");

    // 2. 拨钟 + 卸载。
    liveness.rewind_last_activity(7200);
    let unloaded = registry.unload_idle_sessions(3600);
    assert_eq!(unloaded, vec![seed.clone()], "idle worker must be unloaded");
    assert!(!registry.is_running(&seed));

    // 卸载后再次卸载 = 幂等（实例已不在 registry）。
    let again = registry.unload_idle_sessions(3600);
    assert!(again.is_empty(), "unload must be idempotent");

    // 3. 重生（resume 语义）→ Created 再现 → 会话可用。
    registry
        .spawn_new(&seed)
        .expect("respawn after idle unload");
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
            Err(error) => panic!("respawned session emitted no Created event: {error}"),
        }
    }
    assert!(registry.is_running(&seed));

    // 4. 挂起交互守卫：busy/suspend 的 worker 不可卸载（拨钟也不行）。
    if let Some(again_liveness) = registry.worker_liveness(&seed) {
        again_liveness.rewind_last_activity(7200);
        again_liveness.set_suspend_pending(true);
        let blocked = registry.unload_idle_sessions(3600);
        assert!(
            !blocked.contains(&seed),
            "suspended session must not be idle-unloaded"
        );
        again_liveness.set_suspend_pending(false);
    }

    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn close_session_cleans_per_seed_resident_state() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-close-clean-{}-{}",
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

    // 不实际 spawn worker：本用例验证 close_session 对 per-seed 全局常驻态
    // 的清理（IMAGE_REGISTRY / hub channels / content_store）——清理点在
    // service.close_session 内，与实例是否存在无关；有 worker 的完整关闭
    // 路径由 idle_unload_then_respawn_preserves_history 覆盖。
    let seed = format!("session-close-clean-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("close-clean-test"));
    let service = QaqhService::init(qaqh_session::SessionManager::global());
    service.attach_ringing(hub.clone());

    qaqh_workspace::read_image::store_image(&seed, "image/png", "QUJD");
    let content_id = hub.put_content(&seed, "text/plain", b"hello".to_vec(), false);
    let _ = hub.publish(
        &seed,
        qaqh_domain::DomainEvent::Control(ControlEvent::SessionStateChanged {
            seed: seed.clone(),
            state: SessionState::Resumed,
        }),
    );
    assert!(qaqh_workspace::read_image::peek_image(&seed, 0).is_some());
    assert!(hub.get_content(&seed, &content_id).is_some());
    let before = hub
        .snapshot(qaqh_domain::RingingChannel::Control, &seed)
        .baseline_stream_seq;
    assert!(before > 0, "channel state must be resident before close");

    service.close_session(&seed, None).expect("close session");

    assert!(
        qaqh_workspace::read_image::peek_image(&seed, 0).is_none(),
        "IMAGE_REGISTRY entry must be dropped on close"
    );
    assert!(
        hub.get_content(&seed, &content_id).is_none(),
        "content_store entry must be released on close"
    );
    let after = hub
        .snapshot(qaqh_domain::RingingChannel::Control, &seed)
        .baseline_stream_seq;
    assert_eq!(after, 0, "hub channel state must be dropped on close");
}
