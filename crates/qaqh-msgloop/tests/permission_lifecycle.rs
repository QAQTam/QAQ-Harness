//! End-to-end permission lifecycle tests driven through Ringing V1 commands
//! and a mock OpenAI-compatible SSE endpoint. These tests exercise LLM-generated
//! tool calls and the ToolPermissionRequested / ToolPermissionRespond lifecycle.
//!
//! M3：输入/输出全部走 Ringing worker 线格式（`wire: "Ringing_domain_v1"`）。

mod common;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use qaqh_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationEvent, SessionState,
    ToolCommand, ToolEvent,
};
use qaqh_msgloop::state::agent::AgentState;
use qaqh_ringing::{
    RingingCommand, RingingEvent, RingingWorkerCommandEnvelope, RingingWorkerEventEnvelope,
};
use serde_json::json;
use tiny_http::{Header, Response, Server};

static SESSION_INIT: Once = Once::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct MockServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<Mutex<bool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn sequential(scenarios: Vec<Vec<String>>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind mock server");
        let port = server.server_addr().to_ip().expect("mock address").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));
        let scenarios = Arc::new(Mutex::new(VecDeque::from(scenarios)));
        let request_counter = requests.clone();
        let request_bodies = bodies.clone();
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            loop {
                if *stop_flag.lock().expect("stop lock") {
                    break;
                }
                let mut request = match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(request)) => request,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                request_bodies.lock().expect("body lock").push(body);
                request_counter.fetch_add(1, Ordering::SeqCst);
                let scenario = scenarios
                    .lock()
                    .expect("scenario lock")
                    .pop_front()
                    .expect("unexpected extra gate request");
                let mut sse = String::new();
                for data in scenario {
                    sse.push_str("data: ");
                    sse.push_str(&data);
                    sse.push_str("\n\n");
                }
                let response = Response::from_string(sse).with_header(
                    "Content-Type: text/event-stream"
                        .parse::<Header>()
                        .expect("content-type header"),
                );
                request.respond(response).expect("mock response");
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            bodies,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().expect("stop lock") = true;
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock server thread");
        }
    }
}

fn tool_round(calls: &[(&str, &str, serde_json::Value)]) -> Vec<String> {
    let mut events = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": args.to_string()}
                        }]
                    }
                }]
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    events.push(
        json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
        .to_string(),
    );
    events.push("[DONE]".into());
    events
}

fn final_round(text: &str) -> Vec<String> {
    vec![
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}).to_string(),
        json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
        })
        .to_string(),
        "[DONE]".into(),
    ]
}

fn marker_exec_args(path: &std::path::Path) -> serde_json::Value {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().replace('\'', "''");
        json!({
            "argv": [
                "powershell",
                "-NoProfile",
                "-Command",
                format!("Set-Content -LiteralPath '{path}' -Value done"),
            ]
        })
    }
    #[cfg(not(windows))]
    {
        json!({
            "argv": ["sh", "-c", "printf done > \"$1\"", "sh", path.to_string_lossy()]
        })
    }
}

// ═══════════════════════════════════════════════════════
// Ringing 命令 / 事件 helper
// ═══════════════════════════════════════════════════════

fn next_command_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn send_cmd(writer: &mut os_pipe::PipeWriter, seed: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(seed, format!("c{}", next_command_id()), command);
    writeln!(writer, "{}", serde_json::to_string(&env).expect("serialize envelope")).expect("write frame");
    writer.flush().expect("flush pipe");
}

fn cmd_user_input(text: &str) -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: text.into(),
        images: vec![],
        attachments: None,
        as_system: false,
    })
}

fn cmd_session_create(close_current: bool) -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionCreate {
        close_current,
        cwd: None,
        tool_mode: None,
        custom_tools: Vec::new(),
    })
}

fn cmd_session_shutdown() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionShutdown)
}

fn cmd_permission_respond(call_id: &str, approved: bool) -> RingingCommand {
    RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
        tool_call_id: call_id.into(),
        approved,
        trust_folder: false,
    })
}

fn expect_event(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
    timeout: Duration,
    predicate: impl Fn(&RingingEvent) -> bool,
) -> RingingEvent {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => return event,
            Ok(event) => seen.push(format!("{event:?}")),
            Err(error) => panic!("event timeout/disconnect: {error}; seen={seen:#?}"),
        }
    }
}

fn permission_id(receiver: &std::sync::mpsc::Receiver<RingingEvent>) -> String {
    match expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Tool(ToolEvent::ToolPermissionRequested { .. })
        )
    }) {
        RingingEvent::Tool(ToolEvent::ToolPermissionRequested { tool_call_id, .. }) => tool_call_id,
        other => panic!("expected ToolPermissionRequested, got {other:?}"),
    }
}

fn assert_no_round_completion(receiver: &std::sync::mpsc::Receiver<RingingEvent>) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(event)
                if matches!(
                    event,
                    RingingEvent::Conversation(
                        ConversationEvent::TurnCompleted { .. }
                            | ConversationEvent::TurnFailed { .. }
                    ) | RingingEvent::Tool(ToolEvent::ToolFinished { .. })
                ) =>
            {
                panic!("suspended LLM turn completed prematurely: {event:?}")
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("event channel disconnected")
            }
        }
    }
}

fn collect_through_terminal(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
) -> Vec<RingingEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = receiver
            .recv_timeout(remaining)
            .expect("resumed turn did not complete");
        let terminal = matches!(
            event,
            RingingEvent::Conversation(
                ConversationEvent::TurnCompleted { .. }
                    | ConversationEvent::TurnFailed { .. }
                    | ConversationEvent::ConversationCancelled { .. }
            )
        );
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn assert_single_completion(events: &[RingingEvent], expected_results: usize) {
    let finished = events
        .iter()
        .filter_map(|event| match event {
            RingingEvent::Tool(ToolEvent::ToolFinished { tool_call_id, .. }) => {
                Some(tool_call_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished.len(),
        expected_results,
        "tool results must be emitted once"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
            ))
            .count(),
        1,
        "TurnCompleted must be emitted once",
    );
}

fn finished_result<'a>(events: &'a [RingingEvent]) -> Option<&'a qaqh_types::ToolResult> {
    events.iter().find_map(|event| match event {
        RingingEvent::Tool(ToolEvent::ToolFinished { result, .. }) => Some(result),
        _ => None,
    })
}

fn run_case(
    permission_level: u8,
    workspace: &std::path::Path,
    scenarios: Vec<Vec<String>>,
    expected_requests: usize,
    test: impl FnOnce(&mut os_pipe::PipeWriter, &std::sync::mpsc::Receiver<RingingEvent>)
    + Send
    + 'static,
) -> Vec<String> {
    SESSION_INIT.call_once(|| {
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    });
    let mock = MockServer::sequential(scenarios);
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());

    let mut agent = AgentState::init("permission-lifecycle-test");
    agent.ephemeral = true;
    agent.config.permission_level = permission_level;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (input_reader, mut input_writer) = os_pipe::pipe().expect("os pipe");
    let (output_reader, output_writer) = os_pipe::pipe().expect("os pipe");
    let mut agent_loop =
        common::spawn_pipe_loop(agent, BufReader::new(input_reader), output_writer);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output_reader).lines().map_while(Result::ok) {
            if let Ok(env) = serde_json::from_str::<RingingWorkerEventEnvelope>(&line) {
                if event_tx.send(env.event).is_err() {
                    break;
                }
            }
        }
    });

    let workspace = workspace.to_path_buf();
    let driver = thread::spawn(move || {
        send_cmd(&mut input_writer, "", cmd_session_create(false));
        let seed = match expect_event(&event_rx, Duration::from_secs(5), |event| {
            matches!(
                event,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { seed, .. }) => seed,
            other => panic!("expected SessionStateChanged(Created), got {other:?}"),
        };
        // Session creation restores a persisted workspace. Tests need the
        // explicit workspace selection to occur after that lifecycle step.
        qaqh_workspace::set_workspace(&workspace.to_string_lossy());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test(&mut input_writer, &event_rx)
        }));
        send_cmd(&mut input_writer, &seed, cmd_session_shutdown());
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    });

    agent_loop.run();
    driver.join().expect("test driver");
    assert_eq!(mock.requests.load(Ordering::SeqCst), expected_requests);
    mock.bodies.lock().expect("body lock").clone()
}

#[test]
fn skill_activation_reaches_followup_round_and_next_user_turn() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let skill_dir = temp.path().join(".agents/skills/sticky-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: sticky-skill\ndescription: Use for the lifecycle test.\n---\nSTICKY_SKILL_INSTRUCTION\n",
    )
    .unwrap();

    let bodies = run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[(
                "activate-skill",
                "skills",
                json!({"action": "activate", "name": "sticky-skill"}),
            )]),
            final_round("first turn finished"),
            final_round("second turn finished"),
        ],
        3,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("use the matching skill"));
            assert_eq!(permission_id(receiver), "activate-skill");
            assert_no_round_completion(receiver);
            send_cmd(writer, "", cmd_permission_respond("activate-skill", true));
            let first = collect_through_terminal(receiver);
            assert_single_completion(&first, 1);
            let result = finished_result(&first);
            assert!(
                result.is_some_and(|result| result.is_success()),
                "skill tool must execute successfully: {result:?}"
            );

            send_cmd(writer, "", cmd_user_input("continue using it"));
            let second = collect_through_terminal(receiver);
            assert_eq!(
                second
                    .iter()
                    .filter(|event| matches!(
                        event,
                        RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
                    ))
                    .count(),
                1
            );
        },
    );

    // Activation-set injection (Codex world-state mode, agent.rs
    // build_context): once the active set changes, the envelope carrying the
    // activated body is persisted as a system message in the MessageStore, so
    // later request bodies DO carry STICKY_SKILL_INSTRUCTION (in the system
    // prefix region). The lifecycle itself (activation survives follow-up
    // rounds and user turns) is owned by SkillContextManager; here we pin the
    // injection contract so a regression to per-round tail injection must
    // update this test too.
    let injected = bodies
        .iter()
        .filter(|body| body.contains("STICKY_SKILL_INSTRUCTION"))
        .count();
    assert!(
        injected >= 1,
        "activated skill body must reach later requests via the system envelope"
    );
}

#[test]
fn llm_approval_resumes_original_turn_once() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("input.txt");
    std::fs::write(&path, "hello\n").unwrap();
    let call_path = path.clone();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[("llm-read", "read", json!({"path": path}))]),
            final_round("finished"),
        ],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("read it"));
            match expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    RingingEvent::Tool(ToolEvent::ToolPermissionRequested { .. })
                )
            }) {
                RingingEvent::Tool(ToolEvent::ToolPermissionRequested {
                    tool_call_id,
                    risk,
                    consequence,
                    ..
                }) => {
                    assert_eq!(tool_call_id, "llm-read");
                    assert_eq!(risk, qaqh_domain::PermissionRisk::Low);
                    assert!(!consequence.is_empty());
                }
                other => panic!("expected ToolPermissionRequested, got {other:?}"),
            }
            assert_no_round_completion(receiver);
            send_cmd(writer, "", cmd_permission_respond("llm-read", true));
            let events = collect_through_terminal(receiver);
            assert_single_completion(&events, 1);
            let result = finished_result(&events);
            assert!(result.is_some_and(|result| result.is_success()));
            assert!(call_path.exists());
        },
    );
}

#[test]
fn llm_rejection_resumes_with_original_failure() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("input.txt");
    std::fs::write(&path, "hello\n").unwrap();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[("llm-denied", "read", json!({"path": path}))]),
            final_round("handled denial"),
        ],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("read it"));
            assert_eq!(permission_id(receiver), "llm-denied");
            send_cmd(writer, "", cmd_permission_respond("llm-denied", false));
            let events = collect_through_terminal(receiver);
            assert_single_completion(&events, 1);
            let result = finished_result(&events);
            assert!(result.is_some_and(|result| {
                !result.is_success()
                    && result
                        .error
                        .as_ref()
                        .is_some_and(|error| error.message.contains("[DENIED]"))
            }));
        },
    );
}

#[test]
fn llm_multiple_pending_waits_for_every_response() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let first = temp.path().join("first.txt");
    let second = temp.path().join("second.txt");
    std::fs::write(&first, "one\n").unwrap();
    std::fs::write(&second, "two\n").unwrap();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[
                ("llm-first", "read", json!({"path": first})),
                ("llm-second", "read", json!({"path": second})),
            ]),
            final_round("both finished"),
        ],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("read both"));
            let mut ids = vec![permission_id(receiver), permission_id(receiver)];
            ids.sort();
            assert_eq!(ids, vec!["llm-first", "llm-second"]);
            send_cmd(writer, "", cmd_permission_respond("llm-first", true));
            assert_no_round_completion(receiver);
            send_cmd(writer, "", cmd_permission_respond("llm-second", true));
            let events = collect_through_terminal(receiver);
            assert_single_completion(&events, 2);
        },
    );
}

#[test]
fn llm_four_pending_execs_defer_execution_until_all_resolved() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let markers = (1..=4)
        .map(|index| temp.path().join(format!("exec-{index}.txt")))
        .collect::<Vec<_>>();
    let calls = markers
        .iter()
        .enumerate()
        .map(|(index, path)| (format!("exec-{}", index + 1), marker_exec_args(path)))
        .collect::<Vec<_>>();
    let call_refs = calls
        .iter()
        .map(|(id, args)| (id.as_str(), "exec", args.clone()))
        .collect::<Vec<_>>();
    let expected_markers = markers.clone();

    run_case(
        1,
        temp.path(),
        vec![tool_round(&call_refs), final_round("all execs finished")],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("run four commands"));
            let mut ids = (0..4).map(|_| permission_id(receiver)).collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids, vec!["exec-1", "exec-2", "exec-3", "exec-4"]);

            for id in &ids[..3] {
                send_cmd(writer, "", cmd_permission_respond(id, true));
            }
            assert_no_round_completion(receiver);
            let execution_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < execution_deadline
                && expected_markers.iter().all(|path| !path.exists())
            {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                expected_markers.iter().all(|path| !path.exists()),
                "approved execs must remain deferred until every decision is recorded",
            );

            send_cmd(writer, "", cmd_permission_respond(&ids[3], true));
            let completion_deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < completion_deadline
                && expected_markers.iter().any(|path| !path.exists())
            {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                expected_markers.iter().all(|path| path.exists()),
                "approved exec batch did not finish: {:?}",
                expected_markers
                    .iter()
                    .map(|path| path.exists())
                    .collect::<Vec<_>>(),
            );
            let events = collect_through_terminal(receiver);
            assert_single_completion(&events, 4);
        },
    );
}

#[test]
fn llm_mixed_auto_and_pending_emits_one_unified_result() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("input.txt");
    let output = temp.path().join("output.txt");
    std::fs::write(&input, "hello\n").unwrap();
    let expected_output = output.clone();
    run_case(
        2,
        temp.path(),
        vec![
            tool_round(&[
                ("llm-auto", "read", json!({"path": input})),
                (
                    "llm-pending",
                    "write",
                    json!({"path": output, "content": "created"}),
                ),
            ]),
            final_round("mixed finished"),
        ],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("read and write"));
            assert_eq!(permission_id(receiver), "llm-pending");
            assert_no_round_completion(receiver);
            send_cmd(writer, "", cmd_permission_respond("llm-pending", true));
            let events = collect_through_terminal(receiver);
            assert_single_completion(&events, 2);
            assert_eq!(
                std::fs::read_to_string(&expected_output).unwrap(),
                "created"
            );
        },
    );
}

#[test]
fn llm_session_switch_invalidates_suspended_turn() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("must-not-exist.txt");
    let stale_output = output.clone();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[(
                "llm-stale",
                "write",
                json!({"path": output, "content": "unsafe"}),
            )]),
            final_round("new session works"),
        ],
        2,
        move |writer, receiver| {
            send_cmd(writer, "", cmd_user_input("write it"));
            assert_eq!(permission_id(receiver), "llm-stale");
            send_cmd(writer, "", cmd_session_create(true));
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    RingingEvent::Control(ControlEvent::SessionStateChanged {
                        state: SessionState::Created,
                        ..
                    })
                )
            });
            send_cmd(writer, "", cmd_permission_respond("llm-stale", true));
            assert!(
                !stale_output.exists(),
                "stale approval executed after switch"
            );
            send_cmd(writer, "", cmd_user_input("continue in new session"));
            let events = collect_through_terminal(receiver);
            assert!(events.iter().any(|event| matches!(
                event,
                RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
            )));
            assert!(!stale_output.exists());
        },
    );
}

#[test]
fn llm_approval_forwards_exec_via_http_backend() {
    // 验证 WSL 模式（Http backend）下的审批链路：worker 进程内 admit 产生
    // ToolPermissionRequested → 放行 → 工具经 Http backend 发到真实 serve 执行。
    // serve 位置（本地/ WSL）不影响本断言（审批在 worker 进程内，Http backend 只转发）；
    // WSL 的路径桥接由 PLAN-WSL2-PATH-BRIDGE 的端到端单独验证。
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("http-exec.txt");
    let expected_marker = marker.clone();
    let calls = tool_round(&[("http-exec", "exec", marker_exec_args(&marker))]);

    // 起真实 serve 进程（本地，模拟 WSL 模式的 Http backend 执行路径）。
    // 平台适配：Windows 产物带 .exe；CARGO_TARGET_DIR 覆盖时跟随覆盖目录。
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let exe_name = if cfg!(windows) {
        "qaqh-workspace.exe"
    } else {
        "qaqh-workspace"
    };
    let serve_bin = target_dir.join("debug").join(exe_name);
    assert!(
        serve_bin.exists(),
        "serve binary missing: {}",
        serve_bin.display()
    );
    let mut serve = std::process::Command::new(&serve_bin)
        .args([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "18991",
            "--token",
            "testtoken",
        ])
        // serve 的 token 来源 env(QAQH_WORKSPACE_TOKEN) 优先于 --token；测试进程
        // 环境可能残留 daemon 注入的值，显式钉住 testtoken 保证与 Http backend 一致。
        .env("QAQH_WORKSPACE_TOKEN", "testtoken")
        .env_remove("QAQH_WORKSPACE_URL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn qaqh-workspace serve");

    // 等 ready 行。
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    if let Some(stdout) = serve.stdout.as_mut() {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("QAQH_WORKSPACE_READY") {
                ready = true;
                break;
            }
            if Instant::now() > ready_deadline {
                break;
            }
        }
    }
    assert!(ready, "serve did not become ready in time");

    // 装 Http backend 指向 serve（与 worker.rs 的 QAQH_WORKSPACE_MODE=wsl 分支等价）。
    qaqh_workspace::install_workspace_backend(std::sync::Arc::new(
        qaqh_workspace::HttpToolExecutionBackend::new("http://127.0.0.1:18991", "testtoken"),
    ));

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_case(
            1,
            temp.path(),
            vec![calls, final_round("done")],
            2,
            move |writer, receiver| {
                send_cmd(writer, "", cmd_user_input("run exec"));
                // ① 审批事件产生（worker 进程内 admit）
                let id = permission_id(receiver);
                assert_eq!(id, "http-exec");
                // ② 放行 → 工具经 Http backend 发到 serve 执行
                send_cmd(writer, "", cmd_permission_respond(&id, true));
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && !expected_marker.exists() {
                    thread::sleep(Duration::from_millis(25));
                }
                assert!(
                    expected_marker.exists(),
                    "approved exec did not run via Http backend / serve"
                );
                let events = collect_through_terminal(receiver);
                assert_single_completion(&events, 1);
            },
        )
    }));

    // 清理：恢复进程内 backend + 停 serve。
    qaqh_workspace::use_local_workspace_backend();
    let _ = serve.kill();
    let _ = serve.wait();
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}
