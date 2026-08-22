//! End-to-end daemon test: start daemon → WS connect → create session →
//! send message → verify response events.
//!
//! Run with:  cargo test -p qaqh-daemon --test daemon_ws -- --ignored
//! Requires a compiled daemon binary in target/debug/ or target/release/.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// ── helpers ────────────────────────────────────────────────────────────

fn kill_leftover_daemon() {
    let _ = Command::new("taskkill")
        .args(["/f", "/im", "qaqh-daemon.exe"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    thread::sleep(Duration::from_millis(500));
}

fn find_daemon_binary() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe in test");
    let test_dir = test_exe.parent().expect("exe parent dir");

    // target/debug/deps/ → target/debug/  (or release)
    let rel = test_dir.join("../qaqh-daemon.exe");
    if rel.exists() {
        return rel.canonicalize().unwrap_or(rel);
    }

    // fallback: try CARGO_BUILD_TARGET_DIR env var
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

fn spawn_daemon() -> (Child, String) {
    kill_leftover_daemon();
    let daemon = find_daemon_binary();

    let mut child = Command::new(&daemon)
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    let discovery_path = std::path::PathBuf::from(home)
        .join(".deepx")
        .join("daemon.json");

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(&discovery_path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                if let Some(ep) = parsed.get("endpoint").and_then(|v| v.as_str()) {
                    return (child, ep.to_string());
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    panic!("daemon discovery file did not appear within 15s");
}

fn read_token() -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    let discovery: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::PathBuf::from(home)
                .join(".deepx")
                .join("daemon.json"),
        )
        .expect("read daemon.json"),
    )
    .expect("parse daemon.json");
    discovery["token"]
        .as_str()
        .expect("token in daemon.json")
        .to_string()
}

// ── minimal WebSocket framing (no external deps) ──────────────────────

fn send_ws(writer: &mut TcpStream, text: &str) {
    let data = text.as_bytes();
    let len = data.len();
    let mut frame = vec![0x81u8]; // FIN + text opcode
    if len < 126 {
        frame.push(len as u8);
    } else if len <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(data);
    writer.write_all(&frame).expect("write ws frame");
    writer.flush().expect("flush ws frame");
}

fn recv_ws(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let mut header = [0u8; 2];
    reader.read_exact(&mut header).ok()?;
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7F) as u64;

    if len == 126 {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).ok()?;
        len = u16::from_be_bytes(buf) as u64;
    } else if len == 127 {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).ok()?;
        len = u64::from_be_bytes(buf);
    }

    let mut mask_key = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask_key).ok()?;
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).ok()?;

    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask_key[i % 4];
        }
    }

    match opcode {
        0x1 => String::from_utf8(payload)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok()),
        0x8 => None,
        0x9 => {
            // ping → pong
            let mut pong = header;
            pong[0] = 0x8A; // FIN + pong
            let _ = reader.get_mut().write_all(&pong);
            recv_ws(reader)
        }
        _ => recv_ws(reader), // skip unknown
    }
}

fn expect_ws_event(
    reader: &mut BufReader<TcpStream>,
    event_type: &str,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(msg) = recv_ws(reader) {
            if msg["type"] == "event" && msg["event"]["type"] == event_type {
                return msg;
            }
            if msg["type"] == "event" && msg["event"]["type"] == "error" {
                panic!("agent error: {}", msg["event"]["message"]);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timeout waiting for {event_type} event");
}

// ── test ──────────────────────────────────────────────────────────────

#[test]
#[ignore = "requires compiled daemon binary — run with cargo test -- --ignored"]
fn daemon_full_session_lifecycle() {
    let (mut child, endpoint) = spawn_daemon();
    let token = read_token();

    // Parse host:port from ws:// endpoint, stripping the path (e.g. /control/v1).
    // The daemon discovery endpoint is "ws://127.0.0.1:PORT/control/v1"; passing
    // the full string to TcpStream::connect fails with "invalid port value".
    let host_port = endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();

    // --- WS connect + upgrade ---
    let stream = TcpStream::connect(host_port).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;

    let ws_key = "dGhlIHNhbXBsZSBub25jZQ==";
    write!(
        writer,
        "GET /control/v1 HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Bearer {token}\r\n\
         \r\n",
        host = host_port,
        key = ws_key,
        token = token,
    )
    .unwrap();
    writer.flush().unwrap();

    // Read upgrade response.
    let mut response = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        response.push_str(&line);
        if line == "\r\n" || line.trim().is_empty() {
            break;
        }
    }
    assert!(response.contains("101"), "WS upgrade failed:\n{response}");

    // --- ClientHello ---
    send_ws(
        &mut writer,
        &json!({
            "type": "client_hello",
            "protocol_version": 1,
            "client_version": "test",
            "client_kind": "tui",
            "client_instance_id": "test-instance",
        })
        .to_string(),
    );
    let hello = recv_ws(&mut reader).expect("server_hello");
    assert_eq!(
        hello["type"], "server_hello",
        "expected server_hello, got {hello}"
    );

    // --- session.new ---
    send_ws(
        &mut writer,
        &json!({
            "type": "request", "request_id": "r1",
            "method": "session.new", "params": {}
        })
        .to_string(),
    );
    let resp = recv_ws(&mut reader).expect("session.new response");
    let seed = resp["result"].as_str().expect("seed").to_string();
    assert!(!seed.is_empty(), "session.new returned empty seed");

    // --- session attach ---
    send_ws(
        &mut writer,
        &json!({
            "type": "session_attach", "request_id": "r2", "seed": seed
        })
        .to_string(),
    );
    let _attach = recv_ws(&mut reader).expect("attach response");

    // --- session.resume ---
    send_ws(
        &mut writer,
        &json!({
            "type": "request", "request_id": "r3",
            "method": "session.resume", "params": {"seed": seed}
        })
        .to_string(),
    );
    let _resume = recv_ws(&mut reader).expect("resume response");

    // Wait for SessionCreated (may arrive as event or in snapshot).
    expect_ws_event(&mut reader, "session_created", Duration::from_secs(15));

    // --- session.send_message ---
    send_ws(
        &mut writer,
        &json!({
            "type": "request", "request_id": "r4",
            "method": "session.send_message",
            "params": {"seed": seed, "text": "Hello from daemon test"}
        })
        .to_string(),
    );
    let _send_resp = recv_ws(&mut reader).expect("send_message response");

    // Wait for TurnStart (LLM may take time).
    expect_ws_event(&mut reader, "turn_start", Duration::from_secs(60));

    // Cleanup.
    let _ = child.kill();
}
