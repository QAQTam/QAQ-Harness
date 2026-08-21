//! Stress test: 10 parallel file read tool calls on the same file.
//! Designed to trigger any deadlock, panic, or lock poisoning in the
//! multi-tool parallel execution path.
//!
//! M3：走 Ringing ToolInvoke 命令（legacy Ui2Agent::ToolCall 已拆除）。

mod common;

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::time::Duration;

use qaqh_domain::{ControlCommand, ControlEvent, ConversationEvent, SessionState, ToolCommand};
use qaqh_msgloop::state::agent::AgentState;
use qaqh_ringing::{
    RingingCommand, RingingEvent, RingingWorkerCommandEnvelope, RingingWorkerEventEnvelope,
};

#[test]
fn ten_parallel_reads_same_file() {
    // ── Setup workspace with a small test file ──
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("test.txt");
    std::fs::write(&file_path, "0123456789").unwrap();
    qaqh_workspace::set_workspace(&tmp.path().to_string_lossy());

    // ── Init agent ──
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    let mut agent = AgentState::init("test");
    // Make the session ephemeral to avoid disk I/O interference
    agent.ephemeral = true;

    // ── Create IPC loop with pipe channels ──
    let (event_tx_from_agent, event_rx_to_test) = mpsc::channel::<RingingEvent>();

    let (input_reader, mut input_writer) = os_pipe::pipe().unwrap();
    let (output_reader, output_writer) = os_pipe::pipe().unwrap();

    let mut loop_ = common::spawn_pipe_loop(agent, BufReader::new(input_reader), output_writer);

    // ── Spawn a thread that feeds commands and collects events ──
    let event_rx = event_rx_to_test;
    let event_tx = event_tx_from_agent;

    // Background thread: forward agent output pipe → event channel
    std::thread::spawn(move || {
        let reader = BufReader::new(output_reader);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if let Ok(env) = serde_json::from_str::<RingingWorkerEventEnvelope>(&line) {
                        if event_tx.send(env.event).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let handle = std::thread::spawn(move || {
        fn send_cmd(w: &mut os_pipe::PipeWriter, seed: &str, command: RingingCommand) {
            let env = RingingWorkerCommandEnvelope::new(seed, format!("c{}", rand_id()), command);
            writeln!(w, "{}", serde_json::to_string(&env).unwrap()).unwrap();
            w.flush().unwrap();
        }
        fn rand_id() -> u64 {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }

        // Feed SessionCreate first
        send_cmd(
            &mut input_writer,
            "",
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
        );

        // Wait for SessionStateChanged(Created)
        let mut seed = String::new();
        loop {
            match event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(RingingEvent::Control(ControlEvent::SessionStateChanged {
                    seed: s,
                    state: SessionState::Created,
                })) => {
                    seed = s;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(
            !seed.is_empty(),
            "SessionStateChanged(Created) not received"
        );

        // Send 10 ToolInvoke frames with incrementing IDs
        for i in 0..10 {
            send_cmd(
                &mut input_writer,
                &seed,
                RingingCommand::Tool(ToolCommand::ToolInvoke {
                    tool_call_id: format!("tc_{i}"),
                    name: "read".into(),
                    action: String::new(),
                    args: serde_json::json!({
                        "path": file_path.to_string_lossy(),
                    }),
                }),
            );
        }

        // Drain events and check for errors
        let mut error_count = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match event_rx.recv_timeout(remaining) {
                Ok(RingingEvent::Control(ControlEvent::OperationFailed { error, .. })) => {
                    eprintln!("Error event: {}", error.message);
                    error_count += 1;
                }
                Ok(RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })) => break,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        error_count
    });

    // Run the agent loop in this thread
    loop_.run();

    let error_count = handle.join().unwrap();
    assert_eq!(error_count, 0, "Agent emitted {} error events", error_count);
}
