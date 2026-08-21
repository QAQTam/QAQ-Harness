//! In-process Ringing V1 loop transport test (knife 1 step 1).
//!
//! `Loop::new_ipc` is the OS-pipe boundary. This test drives the exact same
//! loop through `LoopChannels` / `Loop::from_channels`, proving the loop is
//! transport-agnostic before daemon-side subagent actors use the channel form.

use std::sync::mpsc;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlCommand, ControlEvent, SessionState};
use qaqh_msgloop::ringing_v1::loop_core::{Loop, LoopChannels};
use qaqh_msgloop::ringing_v1::types::{WorkerCommand, WriterEvent};
use qaqh_msgloop::state::agent::AgentState;
use qaqh_ringing::{RingingCommand, RingingEvent, RingingWorkerCommandEnvelope};

static SESSION_INIT: Once = Once::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn send_cmd(cmd_tx: &mpsc::SyncSender<WorkerCommand>, seed: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(seed, "inproc-cmd", command);
    cmd_tx
        .send(WorkerCommand {
            frame: env,
            causation: Some("inproc-cmd".into()),
        })
        .expect("test channel/thread must not fail");
}

fn expect(
    rx: &mpsc::Receiver<RingingEvent>,
    timeout: Duration,
    pred: impl Fn(&RingingEvent) -> bool,
) -> RingingEvent {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) if pred(&event) => return event,
            Ok(event) => eprintln!("skipped event: {event:?}"),
            Err(error) => panic!("timeout/disconnect while waiting for event: {error}"),
        }
    }
}

#[test]
fn inprocess_channels_run_the_same_session_lifecycle_as_pipes() {
    let _test_lock = TEST_LOCK.lock().expect("test channel/thread must not fail");
    let tmp = tempfile::tempdir().expect("test channel/thread must not fail");
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).expect("test channel/thread must not fail");
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    SESSION_INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));

    let mut agent = AgentState::init("test");
    agent.ephemeral = true;

    let channels = LoopChannels::new();
    let cmd_tx = channels.cmd_tx.clone();
    let writer_dead = channels.writer_dead.clone();
    let (event_tx, event_rx) = mpsc::channel::<RingingEvent>();

    let reader = std::thread::spawn(move || {
        for event in channels.event_rx {
            match event {
                WriterEvent::Ringing(env) => {
                    if event_tx.send(env.event).is_err() {
                        break;
                    }
                }
                WriterEvent::Timeline(_) => {}
            }
        }
    });

    let driver = std::thread::spawn(move || {
        send_cmd(
            &cmd_tx,
            "",
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
        );
        let seed = match expect(&event_rx, Duration::from_secs(10), |event| {
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
        assert!(!seed.is_empty());

        // Interrupt frames still set the shared cancel token before the command
        // enters the queue; SessionShutdown is the loop's normal exit signal.
        assert!(matches!(
            qaqh_msgloop::ringing_v1::loop_core::ringing_command_is_interrupt(
                &RingingWorkerCommandEnvelope::new(
                    &seed,
                    "inproc-shutdown",
                    RingingCommand::Control(ControlCommand::SessionShutdown)
                )
            ),
            true
        ));
        send_cmd(
            &cmd_tx,
            &seed,
            RingingCommand::Control(ControlCommand::SessionShutdown),
        );
    });

    let mut lp = Loop::from_channels(
        agent,
        channels.cmd_rx,
        channels.event_tx,
        channels.cancel,
        channels.writer_dead,
    );
    lp.run();

    driver.join().expect("test channel/thread must not fail");
    reader.join().expect("test channel/thread must not fail");
    // No writer thread owns this channel side; the flag must stay clean.
    assert!(!writer_dead.load(std::sync::atomic::Ordering::SeqCst));
}
