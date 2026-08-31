//! Shared test harness: drive a `Loop` through OS pipes, replacing the retired
//! `Loop::new_ipc`. Production uses `Loop::from_channels`; this harness owns
//! the reader (input → command channel) and writer (event channel → output)
//! threads so tests keep the same pipe semantics without the process boundary.

use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

use qaqh_msgloop::ringing_v1::loop_core::{Loop, LoopChannels, ringing_command_is_interrupt};
use qaqh_msgloop::ringing_v1::types::{WorkerCommand, WriterEvent};
use qaqh_msgloop::ringing_v1::wire::read_worker_command_frame;
use qaqh_msgloop::state::agent::AgentState;

/// First-class pipe transport for tests: spawns the reader (input → command
/// channel) and writer (event channel → output) threads that `Loop::new_ipc`
/// used to own, then builds the loop via [`Loop::from_channels`].
///
/// Returns the loop; callers still own the pipe's `input`/`output` ends and
/// must feed command frames / read event frames exactly as before.
pub fn spawn_pipe_loop(
    agent: AgentState,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> Loop {
    let channels = LoopChannels::new();
    let cancel_for_reader = channels.cancel.clone();
    let cmd_tx = channels.cmd_tx.clone();
    let writer_dead_for_thread = channels.writer_dead.clone();

    // Reader: input JSON-LP → cmd_tx (sets cancel on interrupt commands).
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        loop {
            match read_worker_command_frame(&mut reader) {
                Ok(Some(env)) => {
                    let causation = env.command_id.clone();
                    if ringing_command_is_interrupt(&env) {
                        cancel_for_reader.set();
                        qaqh_workspace::set_cancel(true);
                    }
                    if cmd_tx
                        .send(WorkerCommand {
                            frame: env,
                            causation: Some(causation),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    // Writer: event_rx → output (JSON-LP lines).
    std::thread::spawn(move || {
        let mut writer = output;
        loop {
            let written = match channels.event_rx.recv() {
                Ok(WriterEvent::Ringing(env)) => write_event_env(&mut writer, &env),
                Ok(WriterEvent::Timeline(env)) => write_timeline_env(&mut writer, &env),
                Err(_) => break,
            };
            if written.is_err() {
                break;
            }
        }
        writer_dead_for_thread.store(true, Ordering::SeqCst);
    });

    Loop::from_channels(
        agent,
        channels.cmd_rx,
        channels.event_tx,
        channels.cancel,
        channels.writer_dead,
    )
}

fn write_event_env<W: Write>(
    w: &mut W,
    env: &qaqh_ringing::RingingWorkerEventEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}

fn write_timeline_env<W: Write>(
    w: &mut W,
    env: &qaqh_ringing::RingingTimelineIntentEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}
