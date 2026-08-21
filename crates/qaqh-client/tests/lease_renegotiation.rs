//! End-to-end lease re-negotiation test.
//!
//! Spawns a *private* daemon instance in an isolated data root (temporary
//! home + `QAQH_DATA_DIR`), with a shortened lease TTL
//! (`QAQH_TEST_LEASE_TTL_MS` = 3s, shorter than the client's 5s renewal
//! interval) so renewals are guaranteed to fail and the lease to expire.
//! Regression coverage for the reconnect-death loop: the client must re-open
//! (new lease) and the SSE streams must recover to `Open` instead of pinning
//! a stale session that the daemon's keepalive gate keeps closing.
//!
//! Isolation: discovery file, single-instance lock and data root all live in
//! the temporary directory — the test never touches (and never kills) any
//! daemon the user may be running.
//!
//! Run with:  cargo test -p qaqh-client --test lease_renegotiation -- --ignored
//! Requires a compiled daemon binary: cargo build -p qaqh-daemon

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qaqh_client::{Channel, ChannelStatus, Client, ClientHandlers, ClientOptions};

// ── isolated test home ───────────────────────────────────────────────────

/// Create a private `home\.deepx` data root (Windows-validated layout:
/// parent == home, dir name == ".deepx") for this test's daemon + client.
fn make_isolated_home() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "qaqh-lease-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let data = base.join(".deepx");
    std::fs::create_dir_all(&data).expect("create isolated data root");
    base
}

fn cleanup_isolated_home(base: &PathBuf) {
    // Best effort: daemon may still hold handles right after kill.
    let _ = std::fs::remove_dir_all(base);
}

fn find_daemon_binary() -> PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    let test_dir = test_exe.parent().unwrap();

    // target/debug/deps/ → target/debug/  (or release)
    let rel = test_dir.join("../qaqh-daemon.exe");
    if rel.exists() {
        return rel.canonicalize().unwrap_or(rel);
    }

    if let Ok(dir) = std::env::var("CARGO_BUILD_TARGET_DIR") {
        for profile in &["debug", "release"] {
            let c = std::path::PathBuf::from(&dir)
                .join(profile)
                .join("qaqh-daemon.exe");
            if c.exists() {
                return c;
            }
        }
    }

    panic!(
        "daemon binary not found at {:?}; build with: cargo build -p qaqh-daemon",
        rel
    );
}

/// Spawn a private daemon with `QAQH_TEST_LEASE_TTL_MS=3000` (lease expires
/// in 3s, faster than the client's renewal cadence — forces the
/// re-negotiation path) inside the isolated data root. Never touches the
/// user's own daemon (separate discovery/lock/data dir).
fn spawn_isolated_daemon(home: &PathBuf) -> Child {
    let daemon = find_daemon_binary();
    let data = home.join(".deepx");

    let child = Command::new(&daemon)
        .arg("run")
        .env("QAQH_DATA_DIR", &data)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("QAQH_TEST_LEASE_TTL_MS", "3000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let discovery_path = data.join("daemon.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(&discovery_path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(ep) = parsed.get("endpoint").and_then(|v| v.as_str()) {
                    if !ep.is_empty() {
                        return child;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut child = child;
    let _ = child.kill();
    panic!("isolated daemon discovery file did not appear within 15s");
}

/// Drop guard: kill the spawned daemon and remove the isolated home.
struct TestDaemon {
    child: Option<Child>,
    home: PathBuf,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        cleanup_isolated_home(&self.home);
    }
}

// ── test ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires compiled daemon binary; run with -- --ignored"]
async fn lease_expiry_triggers_renegotiation_and_streams_recover() {
    let home = make_isolated_home();
    let data = home.join(".deepx");

    // Point this process's client at the same isolated data root (must be set
    // before any discovery read; the daemon child gets it via env too).
    // SAFETY: single-threaded test setup, no other env reads in flight.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }

    let daemon = TestDaemon {
        child: Some(spawn_isolated_daemon(&home)),
        home: home.clone(),
    };

    // Per-channel counters: open vs reconnecting transitions.
    let open_counts: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0, 0, 0]));
    let reconnect_counts: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0, 0, 0]));

    let handlers = ClientHandlers {
        on_batch: Arc::new(|_| {}),
        on_status: {
            let open_counts = open_counts.clone();
            let reconnect_counts = reconnect_counts.clone();
            Arc::new(move |channel: Channel, status: ChannelStatus| {
                let idx = match channel {
                    Channel::Control => 0,
                    Channel::Conversation => 1,
                    Channel::Tool => 2,
                };
                match &status {
                    ChannelStatus::Open { .. } => {
                        open_counts.lock().unwrap()[idx] += 1;
                    }
                    ChannelStatus::Reconnecting { .. } => {
                        reconnect_counts.lock().unwrap()[idx] += 1;
                    }
                    _ => {}
                }
            })
        },
        on_reset: None,
        on_timeline_entry: Arc::new(|_, _| {}),
        on_timeline_status: Arc::new(|_| {}),
        on_timeline_snapshot: Arc::new(|_| {}),
    };

    let client = Client::connect_async(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        daemon_path: None,
        start_timeout: Duration::from_secs(8),
        remote: None,
    })
    .await
    .expect("client connect to isolated daemon");

    // Phase 1: wait for the initial Open on all three channels.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let all_open = open_counts.lock().unwrap().iter().all(|&c| c >= 1);
        if all_open || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let initial = open_counts.lock().unwrap().clone();
    assert!(
        initial.iter().all(|&c| c >= 1),
        "channels did not reach Open initially: {initial:?}"
    );

    // Phase 2: with TTL (3s) < renewal interval (5s), the lease keeps
    // expiring. Renewal fails -> the client must re-open and re-broadcast the
    // session; streams must come back to Open instead of pinning the dead
    // session (the reconnect-death loop this test guards against).
    tokio::time::sleep(Duration::from_secs(45)).await;

    let open = open_counts.lock().unwrap().clone();
    let reconnects = reconnect_counts.lock().unwrap().clone();

    // The lease must have actually expired (reconnects observed)…
    assert!(
        reconnects.iter().any(|&c| c >= 1),
        "lease never expired (TTL override not active?): reconnects={reconnects:?}"
    );
    // …and the streams must have recovered at least once after the initial
    // Open (re-negotiation worked; no permanent disconnect).
    assert!(
        open.iter().any(|&c| c >= 2),
        "streams never recovered after lease expiry: open={open:?} reconnects={reconnects:?}"
    );

    // Phase 3: sanity — after another window the streams are still cycling
    // Open (self-healing continues; not permanently stuck in Reconnecting).
    tokio::time::sleep(Duration::from_secs(20)).await;
    let open = open_counts.lock().unwrap().clone();
    assert!(
        open.iter().any(|&c| c >= 3),
        "self-healing stopped after initial recovery: open={open:?}"
    );

    client.close();
    drop(daemon);
}
