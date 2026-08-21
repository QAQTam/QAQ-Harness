//! Knife-1 step-2 收尾 regression：`QaqhService` 提供进程内 `SubagentHost`。
//!
//! `spawn_subagent` 工具此后经宿主句柄直达 daemon 进程内的
//! AgentRegistry + RingingHub，不再建立 daemon HTTP/SSE 回连。本测试用无模型
//! 的确定性路径验证宿主接口：spawn 返回 seed、send_ringing 命令可直达、
//! subscribe 能从 hub 过滤出该 seed 的事件批次、close 幂等；无 hub 时优雅降级。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_domain::{ControlEvent, DomainEvent, RingingChannel, SessionState};
use qaqh_runtime::{QaqhService, RingingHub};
use qaqh_subagent::SubagentHost;

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn qaqh_service_host_spawn_subscribe_send_close() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-host-direct-test-{}-{}",
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

    // ── 阶段 1：未 attach hub，宿主能力优雅降级（不 panic / 不阻塞）。──
    let service = QaqhService::init();
    let host: &dyn SubagentHost = &service;
    let nohub_rx = host.subscribe("no-hub-seed");
    assert!(
        nohub_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "subscribe without hub must not block forever"
    );

    // ── 阶段 2：attach hub 后走完整宿主直连路径。──
    let hub = Arc::new(RingingHub::new("qaqh-host-direct-test"));
    service.attach_ringing(hub.clone());
    let host: &dyn SubagentHost = &service;

    // 1. spawn：返回合法 seed；不经过 daemon HTTP/SSE。
    let seed = host
        .spawn_subagent(&[], None, None, None, None)
        .expect("host spawn_subagent must succeed");
    assert!(!seed.is_empty(), "host spawn must return a non-empty seed");

    // 2. subscribe：从 hub 过滤该 seed 的事件批次（工具 collect 线程消费）。
    let rx = host.subscribe(&seed);
    // 发布一条属于该 seed 的合成事件（等价 actor 事件进入 hub 的路径）。
    hub.publish_with_causation(
        &seed,
        DomainEvent::Control(ControlEvent::SessionStateChanged {
            seed: seed.clone(),
            state: SessionState::Created,
        }),
        None,
    );
    let batch = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("subscribe must deliver the seed's event batch");
    assert_eq!(batch.seed, seed, "batch must carry the sub seed");
    assert_eq!(batch.channel, RingingChannel::Control);
    assert!(
        batch.envelopes.iter().any(|env| env.seed == seed),
        "batch must contain the published envelope"
    );

    // 3. send_ringing：命令直达 actor 队列（SessionShutdown 触发 actor 优雅退出，
    //    无模型也可安全清理；不依赖 provider）。
    host.send_ringing(
        &seed,
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionShutdown),
    )
    .expect("send_ringing must address the in-process actor");

    // 4. close：幂等（已关闭/已退出均返回 Ok，不 panic）。
    host.close(&seed).expect("first close");
    let _ = host.close(&seed); // 第二次幂等
}
