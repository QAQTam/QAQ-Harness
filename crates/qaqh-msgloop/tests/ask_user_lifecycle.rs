//! End-to-end ask_user lifecycle tests driven through Ringing V1 commands and
//! a mock OpenAI-compatible SSE endpoint. These tests exercise the production
//! Ring export (dispatch_ringing_one) end to end: ConversationSendMessage →
//! gate → InteractionRequested → InteractionAskRespond/Dismiss → terminal.
//!
//! M3：输入/输出全部走 Ringing worker 线格式（`wire: "Ringing_domain_v1"`），
//! legacy `Ui2Agent`/`Agent2Ui` 已完全拆除。

mod common;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use qaqh_domain::{
    AskAnswer, AskMode, AskQuestion, AskResolution, ControlCommand, ControlEvent,
    ConversationCommand, ConversationEvent, SessionState, ToolCommand, ToolEvent,
};
use qaqh_msgloop::state::agent::AgentState;
use qaqh_ringing::{RingingCommand, RingingEvent, RingingWorkerCommandEnvelope};
use serde_json::{Value, json};
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
    fn sequential_with_delay(scenarios: Vec<Vec<String>>, response_delay: Duration) -> Self {
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
                if !response_delay.is_zero() {
                    thread::sleep(response_delay);
                }
                let _ = request.respond(response);
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

// ═══════════════════════════════════════════════════════
// Ringing 命令发送 helper（M3：legacy Ui2Agent 输入已拆除）
// ═══════════════════════════════════════════════════════

fn send_cmd(writer: &mut os_pipe::PipeWriter, seed: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(seed, format!("c{}", next_command_id()), command);
    writeln!(writer, "{}", serde_json::to_string(&env).unwrap()).unwrap();
    writer.flush().unwrap();
}

fn next_command_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn cmd_user_input(text: &str) -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: text.into(),
        images: vec![],
        attachments: None,
        as_system: false,
    })
}

fn cmd_ask_respond(ask_id: &str, answers: &[(&str, &str)]) -> RingingCommand {
    RingingCommand::Control(ControlCommand::InteractionAskRespond {
        interaction_id: ask_id.into(),
        answers: answers
            .iter()
            .map(|(question_id, answer)| AskAnswer {
                question_id: (*question_id).into(),
                answer: (*answer).into(),
            })
            .collect(),
    })
}

fn cmd_ask_dismiss(ask_id: &str) -> RingingCommand {
    RingingCommand::Control(ControlCommand::InteractionAskDismiss {
        interaction_id: ask_id.into(),
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

fn cmd_session_resume(seed: &str) -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionResume { seed: seed.into() })
}

fn cmd_session_shutdown() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionShutdown)
}

fn cmd_cancel() -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationCancel { turn_id: None })
}

fn cmd_undo(turn_id: &str) -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationUndoTurn {
        turn_id: turn_id.into(),
    })
}

fn cmd_permission_respond(call_id: &str, approved: bool) -> RingingCommand {
    RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
        tool_call_id: call_id.into(),
        approved,
        trust_folder: false,
    })
}

// ═══════════════════════════════════════════════════════
// Ringing 事件断言 helper
// ═══════════════════════════════════════════════════════

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

fn expect_operation_failed(receiver: &std::sync::mpsc::Receiver<RingingEvent>, code: &str) {
    expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Control(ControlEvent::OperationFailed { error, .. })
                if error.code == code
        )
    });
}

fn expect_interaction_requested(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
    ask_id: &str,
) -> (String, AskMode, Vec<AskQuestion>) {
    let event = expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Control(ControlEvent::InteractionRequested {
                interaction_id,
                ..
            }) if interaction_id == ask_id
        )
    });
    match event {
        RingingEvent::Control(ControlEvent::InteractionRequested {
            turn_id,
            mode,
            questions,
            ..
        }) => {
            assert!(!turn_id.is_empty());
            assert!(!questions.is_empty());
            (turn_id, mode, questions)
        }
        other => panic!("expected InteractionRequested, got {other:?}"),
    }
}

fn expect_interaction_resolved(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
    ask_id: &str,
    resolution: AskResolution,
) {
    expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Control(ControlEvent::InteractionResolved {
                interaction_id,
                resolution: actual,
            }) if interaction_id == ask_id && *actual == resolution
        )
    });
}

fn expect_turn_started(receiver: &std::sync::mpsc::Receiver<RingingEvent>) {
    expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Conversation(ConversationEvent::TurnStarted { .. })
        )
    });
}

fn expect_permission_requested(receiver: &std::sync::mpsc::Receiver<RingingEvent>, call_id: &str) {
    expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Tool(ToolEvent::ToolPermissionRequested {
                tool_call_id,
                ..
            }) if tool_call_id == call_id
        )
    });
}

/// 收集事件直到出现回合终态（TurnCompleted / TurnFailed / ConversationCancelled）。
fn collect_through_terminal(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
) -> Vec<RingingEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = receiver
            .recv_timeout(remaining)
            .expect("turn did not reach a terminal event");
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

fn tool_finished_ids(events: &[RingingEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RingingEvent::Tool(ToolEvent::ToolFinished { tool_call_id, .. }) => {
                Some(tool_call_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn assert_no_turn_advance(receiver: &std::sync::mpsc::Receiver<RingingEvent>, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(event)
                if matches!(
                    event,
                    RingingEvent::Control(ControlEvent::InteractionRequested { .. })
                        | RingingEvent::Conversation(
                            ConversationEvent::TurnCompleted { .. }
                                | ConversationEvent::TurnFailed { .. }
                        )
                ) =>
            {
                panic!("turn advanced before all permissions resolved: {event:?}")
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn find_tool_result_content(value: &Value, call_id: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            if object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == call_id)
                && object.get("role").and_then(Value::as_str) == Some("tool")
            {
                return object
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            object
                .values()
                .find_map(|child| find_tool_result_content(child, call_id))
        }
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_tool_result_content(child, call_id)),
        _ => None,
    }
}

/// ToolResult 结构化后的工具输出：业务 JSON 在 `model.text`（或平铺 `text`）内嵌；
/// ask 等内建交互工具直接存业务 JSON（无包装），三种形状都兼容。
fn inner_tool_result(content: &str) -> Value {
    let value: Value = serde_json::from_str(content).expect("tool result json");
    let text = value
        .get("model")
        .and_then(|model| model.get("text"))
        .or_else(|| value.get("text"))
        .and_then(Value::as_str);
    match text {
        Some(text) => serde_json::from_str(text).unwrap_or(value),
        None => value,
    }
}

fn run_case(
    scenarios: Vec<Vec<String>>,
    expected_requests: usize,
    test: impl FnOnce(
        &mut os_pipe::PipeWriter,
        &std::sync::mpsc::Receiver<RingingEvent>,
        Arc<AtomicUsize>,
        String,
    ) + Send
    + 'static,
) -> Vec<String> {
    run_case_with_delay(scenarios, Duration::ZERO, expected_requests, test)
}

fn run_case_with_delay(
    scenarios: Vec<Vec<String>>,
    response_delay: Duration,
    expected_requests: usize,
    test: impl FnOnce(
        &mut os_pipe::PipeWriter,
        &std::sync::mpsc::Receiver<RingingEvent>,
        Arc<AtomicUsize>,
        String,
    ) + Send
    + 'static,
) -> Vec<String> {
    SESSION_INIT.call_once(|| {
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    });
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("input.txt"),
        "hello from permission test\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("input2.txt"), "second permission input\n").unwrap();
    let mock = MockServer::sequential_with_delay(scenarios, response_delay);
    let request_count = mock.requests.clone();
    qaqh_workspace::set_workspace(&temp.path().to_string_lossy());

    let mut agent = AgentState::init("ask-lifecycle-test");
    agent.ephemeral = true;
    agent.config.permission_level = 1;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (input_reader, mut input_writer) = os_pipe::pipe().unwrap();
    let (output_reader, output_writer) = os_pipe::pipe().unwrap();
    let mut agent_loop = common::spawn_pipe_loop(agent, BufReader::new(input_reader), output_writer);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output_reader).lines().map_while(Result::ok) {
            if let Ok(env) = serde_json::from_str::<RingingWorkerCommandEnvelope>(&line) {
                let _ = env; // 命令方向帧不应出现在 stdout；忽略
                continue;
            }
            if let Ok(env) = serde_json::from_str::<qaqh_ringing::RingingWorkerEventEnvelope>(&line)
            {
                if event_tx.send(env.event).is_err() {
                    break;
                }
            }
        }
    });

    let workspace = temp.path().to_path_buf();
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
        qaqh_workspace::set_workspace(&workspace.to_string_lossy());
        let seed_for_shutdown = seed.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test(&mut input_writer, &event_rx, request_count, seed)
        }));
        send_cmd(
            &mut input_writer,
            &seed_for_shutdown,
            cmd_session_shutdown(),
        );
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    });

    agent_loop.run();
    driver.join().expect("test driver");
    assert_eq!(mock.requests.load(Ordering::SeqCst), expected_requests);
    mock.bodies.lock().expect("body lock").clone()
}

// ═══════════════════════════════════════════════════════
// Mock SSE 场景构造（与 legacy 版相同）
// ═══════════════════════════════════════════════════════

fn tool_round(calls: &[(&str, &str, Value)]) -> Vec<String> {
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

// ═══════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════

#[test]
fn batch_ask_waits_for_every_answer_and_writes_one_exact_result() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[(
                "ask-batch",
                "ask",
                json!({
                    "questions": [
                        {"id":"q1", "question":"First?", "options":["A","B"], "allow_custom":false},
                        {"id":"q2", "question":"Second?", "options":["C","D"], "allow_custom":false}
                    ]
                }),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("ask me"));
            let (_, mode, requested) = expect_interaction_requested(receiver, "ask-batch");
            assert_eq!(mode, AskMode::Batch);
            assert_eq!(requested.len(), 2);

            // 部分答案 → 拒绝（batch 需要全部）
            send_cmd(writer, &seed, cmd_ask_respond("ask-batch", &[("q1", "A")]));
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            // 完整答案 → resolved
            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("ask-batch", &[("q1", "A"), ("q2", "D")]),
            );
            expect_interaction_resolved(receiver, "ask-batch", AskResolution::Answered);
            let events = collect_through_terminal(receiver);
            let finished = tool_finished_ids(&events);
            assert_eq!(finished, vec!["ask-batch"]);
            assert_eq!(
                events
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

    let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
    let content = find_tool_result_content(&second_request, "ask-batch")
        .expect("second request must include ask result");
    assert_eq!(
        inner_tool_result(&content),
        json!({
            "status": "answered",
            "answers": [
                {"question_id":"q1", "answer":"A"},
                {"question_id":"q2", "answer":"D"}
            ]
        })
    );
}

#[test]
fn multiple_ask_calls_are_presented_sequentially_before_one_resume() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[
                (
                    "ask-1",
                    "ask",
                    json!({"question":"First?", "options":["A"], "allow_custom":false}),
                ),
                (
                    "ask-2",
                    "ask",
                    json!({"question":"Second?", "options":["B"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("ask twice"));
            expect_interaction_requested(receiver, "ask-1");
            send_cmd(writer, &seed, cmd_ask_respond("ask-1", &[("q1", "A")]));
            expect_interaction_resolved(receiver, "ask-1", AskResolution::Answered);
            expect_interaction_requested(receiver, "ask-2");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(writer, &seed, cmd_ask_respond("ask-2", &[("q1", "B")]));
            expect_interaction_resolved(receiver, "ask-2", AskResolution::Answered);
            let events = collect_through_terminal(receiver);
            let finished = tool_finished_ids(&events);
            assert_eq!(finished, vec!["ask-1", "ask-2"]);
        },
    );

    for (call_id, expected) in [("ask-1", "A"), ("ask-2", "B")] {
        let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
        let content = find_tool_result_content(&second_request, call_id)
            .expect("second request must include each ask result");
        assert_eq!(
            inner_tool_result(&content)["answers"][0]["answer"],
            expected
        );
    }
}

#[test]
fn invalid_or_stale_responses_do_not_consume_the_active_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "active-ask",
                "ask",
                json!({"question":"Pick A", "options":["A"], "allow_custom":false}),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("validate identity"));
            expect_interaction_requested(receiver, "active-ask");

            let invalid: [(&str, &[(&str, &str)]); 4] = [
                ("stale-ask", &[("q1", "A")]),
                ("active-ask", &[("q1", "A"), ("q1", "A")]),
                ("active-ask", &[("unknown", "A")]),
                ("active-ask", &[("q1", "B")]),
            ];
            for (ask_id, answers) in invalid {
                send_cmd(writer, &seed, cmd_ask_respond(ask_id, answers));
                expect_operation_failed(receiver, "ask_rejected");
                assert_eq!(request_count.load(Ordering::SeqCst), 1);
            }

            send_cmd(writer, &seed, cmd_ask_respond("active-ask", &[("q1", "A")]));
            expect_interaction_resolved(receiver, "active-ask", AskResolution::Answered);
            collect_through_terminal(receiver);
        },
    );
}

#[test]
fn dismiss_validates_identity_and_does_not_swallow_the_next_user_input() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "dismiss-ask",
                "ask",
                json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("fresh turn finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("start dismiss case"));
            expect_interaction_requested(receiver, "dismiss-ask");

            send_cmd(writer, &seed, cmd_ask_dismiss("stale-dismiss"));
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(writer, &seed, cmd_ask_dismiss("dismiss-ask"));
            expect_interaction_resolved(receiver, "dismiss-ask", AskResolution::Dismissed);
            let aborted = collect_through_terminal(receiver);
            assert!(aborted.iter().any(|event| matches!(
                event,
                RingingEvent::Conversation(
                    ConversationEvent::TurnCompleted { stop_reason, .. }
                ) if stop_reason.as_deref() == Some("cancelled")
            )));

            send_cmd(writer, &seed, cmd_user_input("fresh input"));
            let fresh = collect_through_terminal(receiver);
            assert!(fresh.iter().any(|event| matches!(
                event,
                RingingEvent::Conversation(ConversationEvent::RoundCompleted {
                    answer: Some(answer),
                    ..
                }) if answer == "fresh turn finished"
            )));
        },
    );
}

#[test]
fn permission_then_ask_resolves_the_same_tool_round_once() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[
                ("read-call", "read", json!({"path":"input.txt"})),
                (
                    "ask-after-read",
                    "ask",
                    json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("read then ask"));
            expect_permission_requested(receiver, "read-call");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(writer, &seed, cmd_permission_respond("read-call", true));
            expect_interaction_requested(receiver, "ask-after-read");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("ask-after-read", &[("q1", "yes")]),
            );
            let events = collect_through_terminal(receiver);
            let finished = tool_finished_ids(&events);
            assert_eq!(finished, vec!["read-call", "ask-after-read"]);
        },
    );

    let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
    assert!(find_tool_result_content(&second_request, "read-call").is_some());
    assert!(find_tool_result_content(&second_request, "ask-after-read").is_some());
}

#[test]
fn every_permission_resolves_before_the_queued_ask_is_presented() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[
                ("read-one", "read", json!({"path":"input.txt"})),
                ("read-two", "read", json!({"path":"input2.txt"})),
                (
                    "ask-after-two-reads",
                    "ask",
                    json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("approve both then ask"));
            for expected in ["read-one", "read-two"] {
                expect_permission_requested(receiver, expected);
            }

            send_cmd(writer, &seed, cmd_permission_respond("read-one", true));
            assert_no_turn_advance(receiver, Duration::from_millis(250));
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(writer, &seed, cmd_permission_respond("read-two", true));
            expect_interaction_requested(receiver, "ask-after-two-reads");
            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("ask-after-two-reads", &[("q1", "yes")]),
            );
            let events = collect_through_terminal(receiver);
            let finished = tool_finished_ids(&events);
            assert_eq!(finished.len(), 3);
        },
    );
}

#[test]
fn cancel_aborts_one_suspended_turn_and_invalidates_its_ask_id() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![tool_round(&[(
            "cancel-ask",
            "ask",
            json!({"question":"Wait?", "options":["yes"], "allow_custom":false}),
        )])],
        1,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("start cancel case"));
            expect_interaction_requested(receiver, "cancel-ask");

            send_cmd(writer, &seed, cmd_cancel());
            let aborted = collect_through_terminal(receiver);
            assert!(aborted.iter().any(|event| match event {
                RingingEvent::Conversation(ConversationEvent::ConversationCancelled { .. }) => true,
                RingingEvent::Conversation(ConversationEvent::TurnCompleted {
                    stop_reason,
                    ..
                }) => {
                    stop_reason.as_deref() == Some("cancelled")
                }
                _ => false,
            }));

            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("cancel-ask", &[("q1", "yes")]),
            );
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn new_session_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "new-session-ask",
                "ask",
                json!({"question":"Switch?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("stale answer was consumed"),
        ],
        1,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("start new-session case"));
            expect_interaction_requested(receiver, "new-session-ask");
            send_cmd(writer, &seed, cmd_session_create(true));
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    RingingEvent::Control(ControlEvent::SessionStateChanged {
                        state: SessionState::Created,
                        ..
                    })
                )
            });
            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("new-session-ask", &[("q1", "yes")]),
            );
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn resume_session_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "resume-session-ask",
                "ask",
                json!({"question":"Resume?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("stale answer was consumed"),
        ],
        1,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("start resume-session case"));
            expect_interaction_requested(receiver, "resume-session-ask");
            send_cmd(writer, &seed, cmd_session_resume(&seed));
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    RingingEvent::Control(ControlEvent::SessionStateChanged {
                        state: SessionState::Resumed,
                        ..
                    })
                )
            });
            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("resume-session-ask", &[("q1", "yes")]),
            );
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn undo_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![tool_round(&[(
            "undo-ask",
            "ask",
            json!({"question":"Undo?", "options":["yes"], "allow_custom":false}),
        )])],
        1,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("start undo case"));
            let (turn_id, _, _) = expect_interaction_requested(receiver, "undo-ask");
            send_cmd(writer, &seed, cmd_undo(&turn_id));
            // undo 完成后不再有 legacy SessionRestored；Ringing 侧以
            // OperationCompleted 表达，前端随后自行重拉 bootstrap 快照。
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    RingingEvent::Control(ControlEvent::OperationCompleted { .. })
                )
            });
            send_cmd(writer, &seed, cmd_ask_respond("undo-ask", &[("q1", "yes")]));
            expect_operation_failed(receiver, "ask_rejected");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn cancel_during_gate_emits_one_complete_terminal_transaction() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case_with_delay(
        vec![final_round("too late")],
        Duration::from_millis(300),
        1,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("cancel during gate"));
            expect_turn_started(receiver);
            let deadline = Instant::now() + Duration::from_secs(5);
            while request_count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
            send_cmd(writer, &seed, cmd_cancel());
            let events = collect_through_terminal(receiver);
            assert!(events.iter().any(|event| matches!(
                event,
                RingingEvent::Conversation(ConversationEvent::ConversationCancelled { .. })
                    | RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
                    | RingingEvent::Conversation(ConversationEvent::TurnFailed { .. })
            )));
        },
    );
}

#[test]
fn stale_undo_does_not_consume_the_active_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "undo-identity-ask",
                "ask",
                json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, seed| {
            send_cmd(writer, &seed, cmd_user_input("validate undo identity"));
            expect_interaction_requested(receiver, "undo-identity-ask");
            send_cmd(writer, &seed, cmd_undo("stale-turn"));
            expect_operation_failed(receiver, "undo_conflict");
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send_cmd(
                writer,
                &seed,
                cmd_ask_respond("undo-identity-ask", &[("q1", "yes")]),
            );
            expect_interaction_resolved(receiver, "undo-identity-ask", AskResolution::Answered);
            collect_through_terminal(receiver);
        },
    );
}
