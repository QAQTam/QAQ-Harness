//! QAQ-Harness Gate Test UI — interactive web-based test harness.
//!
//! Starts a mock OpenAI API server (random port) and a web UI server
//! on `http://127.0.0.1:3000`.  Type a message and watch the gate's
//! streaming events in real time.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use qaqh_gate::{ProviderConfig, StreamEvent};
use qaqh_types::{Message, ToolDef, ToolFunction};
use serde_json::json;

// ══════════════════════════════════════════════════════════════════════
//  Mock Server
// ══════════════════════════════════════════════════════════════════════

/// A single SSE data event.
#[derive(Clone)]
enum SseEvent {
    Data(serde_json::Value),
    Raw(String),
    HttpError(u16, serde_json::Value),
}

/// Build the DSML tool-call delta manually (avoids json! macro issues).
fn make_dsml_delta() -> serde_json::Value {
    let content = concat!(
        "I'll read that file for you.\n\n",
        "<|DSML|tool_calls>\n",
        "<|DSML|invoke name=\"read\">\n",
        "<|DSML|parameter name=\"path\" string=\"true\">/tmp/data.txt\n",
        "</|DSML|parameter>\n",
        "</|DSML|invoke>\n",
        "</|DSML|tool_calls>"
    );
    serde_json::json!({"choices":[{"index":0,"delta":{"content":content}}]})
}

/// Scenario definitions keyed by name.
fn scenarios() -> HashMap<&'static str, Vec<SseEvent>> {
    let mut m: HashMap<&'static str, Vec<SseEvent>> = HashMap::new();

    m.insert(
        "Plain text",
        vec![
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"content":"Hello from mock! "}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"content":"This is a streaming response."}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":10,"total_tokens":22}})),
            SseEvent::Raw("[DONE]".into()),
        ],
    );

    m.insert(
        "With reasoning",
        vec![
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"reasoning_content":"Let me analyze this step by step..."}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"reasoning_content":"First, I need to understand the problem."}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"content":"The answer is 42."}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":28,"total_tokens":36}})),
            SseEvent::Raw("[DONE]".into()),
        ],
    );

    m.insert(
        "Tool call",
        vec![
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"content":"Let me search for that file."}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"test.txt\"}"}}]}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":15,"completion_tokens":25,"total_tokens":40}})),
            SseEvent::Raw("[DONE]".into()),
        ],
    );

    m.insert(
        "Tool call + DSML",
        vec![
            SseEvent::Data(make_dsml_delta()),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":30,"total_tokens":40}})),
            SseEvent::Raw("[DONE]".into()),
        ],
    );

    m.insert(
        "HTTP 401 error",
        vec![SseEvent::HttpError(
            401,
            json!({"error":{"message":"Invalid API key"}}),
        )],
    );

    m.insert(
        "Retry then success",
        vec![
            SseEvent::HttpError(429, json!({"error":{"message":"Rate limit exceeded"}})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{"content":"Retry succeeded!"}}]})),
            SseEvent::Data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}})),
            SseEvent::Raw("[DONE]".into()),
        ],
    );

    m
}

fn make_sse_string(events: &[SseEvent]) -> Vec<u8> {
    let mut buf = Vec::new();
    for ev in events {
        match ev {
            SseEvent::Data(val) => {
                let _ = writeln!(buf, "data: {}", val.to_string());
                buf.extend_from_slice(b"\n");
            }
            SseEvent::Raw(raw) => {
                if raw == "[DONE]" {
                    buf.extend_from_slice(b"data: [DONE]\n\n");
                } else {
                    buf.extend_from_slice(raw.as_bytes());
                    buf.push(b'\n');
                }
            }
            SseEvent::HttpError(_, _) => {} // handled separately
        }
    }
    buf
}

fn start_mock_server(
    scenario_name: Arc<Mutex<String>>,
    scenarios: Arc<HashMap<&'static str, Vec<SseEvent>>>,
) -> (u16, Arc<AtomicBool>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("failed to bind mock server");
    let port = server.server_addr().to_ip().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    thread::spawn(move || {
        loop {
            if stop_clone.load(Ordering::SeqCst) {
                break;
            }
            let req = match server.recv_timeout(Duration::from_millis(200)) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => break,
            };

            let url = req.url().to_string();

            // Balance endpoint passthrough
            if url.contains("/balance") {
                let resp = tiny_http::Response::from_string(
                    r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"100","granted_balance":"50","topped_up_balance":"50"}]}"#,
                )
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
                let _ = req.respond(resp);
                continue;
            }

            // Get current scenario
            let name = scenario_name.lock().unwrap().clone();
            let events = scenarios.get(name.as_str()).cloned().unwrap_or_default();

            // Check for HTTP error response (first event)
            if let Some(SseEvent::HttpError(status, body)) = events.first() {
                let resp = tiny_http::Response::from_string(body.to_string())
                    .with_status_code(tiny_http::StatusCode(*status));
                let _ = req.respond(resp);
                continue;
            }

            let sse = make_sse_string(&events);
            let resp = tiny_http::Response::from_string(String::from_utf8_lossy(&sse).to_string())
                .with_header(
                    "Content-Type: text/event-stream"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
            let _ = req.respond(resp);
        }
    });

    (port, stop)
}

// ══════════════════════════════════════════════════════════════════════
//  HTTP helpers (minimal, no dependencies)
// ══════════════════════════════════════════════════════════════════════

struct HttpRequest {
    method: String,
    path: String,
    #[allow(dead_code)]
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start_line = String::new();
    reader.read_line(&mut start_line).ok()?;
    let parts: Vec<&str> = start_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut headers = HashMap::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(pos) = trimmed.find(':') {
            let key = trimmed[..pos].trim().to_lowercase();
            let value = trimmed[pos + 1..].trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }

    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_status(stream: &mut TcpStream, status: &str) {
    let _ = write!(stream, "HTTP/1.1 {}\r\n", status);
}

fn write_header(stream: &mut TcpStream, key: &str, value: &str) {
    let _ = write!(stream, "{}: {}\r\n", key, value);
}

fn write_body(stream: &mut TcpStream, body: &[u8]) {
    let _ = write!(stream, "Content-Length: {}\r\n\r\n", body.len());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn write_sse_event(stream: &mut TcpStream, data: &str) {
    let _ = write!(stream, "data: {}\n\n", data);
    let _ = stream.flush();
}

// ══════════════════════════════════════════════════════════════════════
//  HTML page (embedded)
// ══════════════════════════════════════════════════════════════════════

const HTML_PAGE: &str = r###"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>QAQ-Harness Gate Test Harness</title>
<style>
* { box-sizing: border-box; margin: 0; padding: 0; }
body { font-family: system-ui, -apple-system, sans-serif; background: #1a1a2e; color: #e0e0e0; height: 100vh; display: flex; flex-direction: column; }
header { background: #16213e; padding: 12px 20px; border-bottom: 1px solid #0f3460; display: flex; align-items: center; gap: 16px; flex-shrink: 0; }
header h1 { font-size: 18px; color: #e94560; }
header select { background: #0f3460; color: #e0e0e0; border: 1px solid #1a1a2e; padding: 6px 10px; border-radius: 4px; font-size: 13px; }
header label { font-size: 13px; color: #a0a0b0; }
.container { display: flex; flex: 1; overflow: hidden; }
.panel { flex: 1; display: flex; flex-direction: column; overflow: hidden; border-right: 1px solid #0f3460; }
.panel:last-child { border-right: none; }
.panel-title { background: #16213e; padding: 8px 14px; font-size: 12px; font-weight: 600; color: #a0a0b0; text-transform: uppercase; letter-spacing: 1px; border-bottom: 1px solid #0f3460; flex-shrink: 0; }
.panel-content { flex: 1; overflow-y: auto; padding: 10px 14px; font-family: 'Cascadia Code', 'Fira Code', 'Consolas', monospace; font-size: 12px; line-height: 1.6; white-space: pre-wrap; word-break: break-all; }
.event-line { margin: 2px 0; padding: 2px 6px; border-radius: 3px; }
.event-content { color: #4fc3f7; }
.event-reasoning { color: #ffb74d; }
.event-tool { color: #81c784; }
.event-done { color: #aed581; }
.event-error { color: #ef5350; background: rgba(239,83,80,0.1); }
.event-retry { color: #ffa726; }
.event-request { color: #ce93d8; }
.event-balance { color: #4dd0e1; }
#input-area { display: flex; gap: 8px; padding: 12px 20px; background: #16213e; border-top: 1px solid #0f3460; flex-shrink: 0; }
#input-area input { flex: 1; background: #0f3460; border: 1px solid #1a1a2e; padding: 10px 14px; border-radius: 6px; color: #e0e0e0; font-size: 14px; outline: none; }
#input-area input:focus { border-color: #e94560; }
#input-area button { background: #e94560; color: #fff; border: none; padding: 10px 24px; border-radius: 6px; font-size: 14px; font-weight: 600; cursor: pointer; }
#input-area button:hover { background: #c73652; }
#input-area button:disabled { opacity: 0.5; cursor: not-allowed; }
.meta { color: #666; font-size: 11px; }
</style>
</head>
<body>
<header>
  <h1>Gate Test Harness</h1>
  <label>Scenario:</label>
  <select id="scenario-select">
    <option>Plain text</option>
    <option>With reasoning</option>
    <option>Tool call</option>
    <option selected>Tool call + DSML</option>
    <option>HTTP 401 error</option>
    <option>Retry then success</option>
  </select>
  <span id="mock-port" class="meta"></span>
</header>
<div class="container">
  <div class="panel">
    <div class="panel-title">Request JSON</div>
    <div id="request-panel" class="panel-content">Waiting for input...</div>
  </div>
  <div class="panel">
    <div class="panel-title">Stream Events</div>
    <div id="events-panel" class="panel-content">Waiting for input...</div>
  </div>
  <div class="panel">
    <div class="panel-title">Final Message</div>
    <div id="result-panel" class="panel-content">Waiting for input...</div>
  </div>
</div>
<div id="input-area">
  <input type="text" id="user-input" placeholder="Type a message..." autofocus>
  <button id="send-btn" onclick="sendMessage()">Send</button>
</div>
<script>
let currentAbort = null;

async function sendMessage() {
  const input = document.getElementById('user-input');
  const text = input.value.trim();
  if (!text) return;

  if (currentAbort) {
    currentAbort.abort();
    currentAbort = null;
  }

  const scenario = document.getElementById('scenario-select').value;
  document.getElementById('request-panel').textContent = 'Sending...';
  document.getElementById('events-panel').textContent = '';
  document.getElementById('result-panel').textContent = '';

  const ac = new AbortController();
  currentAbort = ac;

  try {
    const response = await fetch('/api/chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: text, scenario: scenario }),
      signal: ac.signal,
    });

    document.getElementById('request-panel').textContent =
      `POST /api/chat\nContent-Type: application/json\n\n${JSON.stringify({ text, scenario }, null, 2)}`;

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let eventsHtml = '';
    let fullContent = '';
    let fullReasoning = '';
    let toolCalls = [];

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const parts = buffer.split('\n\n');
      buffer = parts.pop() || '';

      for (const part of parts) {
        const lines = part.split('\n');
        let dataLine = '';
        for (const line of lines) {
          if (line.startsWith('data: ')) {
            dataLine = line.slice(6);
          }
        }
        if (!dataLine) continue;
        if (dataLine === '[DONE]') {
          eventsHtml += '<div class="event-line event-done">[DONE]</div>';
          break;
        }

        let ev;
        try { ev = JSON.parse(dataLine); } catch { continue; }

        const choices = ev.choices && ev.choices[0];
        const delta = choices && choices.delta;

        if (ev.usage) {
          const u = ev.usage;
          eventsHtml += `<div class="event-line event-done">Done — usage: ${u.prompt_tokens}↑ ${u.completion_tokens}↓</div>`;
        }

        if (delta) {
          if (delta.content) {
            eventsHtml += `<div class="event-line event-content">Content: ${escapeHtml(delta.content)}</div>`;
            fullContent += delta.content;
          }
          if (delta.reasoning_content) {
            eventsHtml += `<div class="event-line event-reasoning">Reasoning: ${escapeHtml(delta.reasoning_content)}</div>`;
            fullReasoning += delta.reasoning_content;
          }
          if (delta.tool_calls) {
            for (const tc of delta.tool_calls) {
              const info = tc.function ? `${tc.function.name}(${tc.function.arguments || ''})` : '(partial)';
              eventsHtml += `<div class="event-line event-tool">ToolCall #${tc.index}: ${escapeHtml(info)}</div>`;
              if (tc.id) toolCalls[tc.index] = { id: tc.id, name: tc.function?.name || '', args: '' };
              if (tc.function?.arguments) {
                if (toolCalls[tc.index]) toolCalls[tc.index].args += tc.function.arguments;
              }
            }
          }
          if (choices.finish_reason) {
            eventsHtml += `<div class="event-line event-done">Finish: ${choices.finish_reason}</div>`;
          }
        }
        document.getElementById('events-panel').innerHTML = eventsHtml;
        document.getElementById('events-panel').scrollTop = document.getElementById('events-panel').scrollHeight;
      }
    }

    // Show final result
    let resultText = '';
    if (fullReasoning) resultText += `[Reasoning]\n${fullReasoning}\n\n`;
    if (fullContent) resultText += `[Content]\n${fullContent}\n\n`;
    if (toolCalls.length > 0) {
      resultText += `[Tool Calls]\n`;
      for (const tc of toolCalls) {
        if (tc) {
          let args;
          try { args = JSON.parse(tc.args); } catch { args = tc.args; }
          resultText += `  ${tc.name}(${JSON.stringify(args)})\n`;
        }
      }
    }
    document.getElementById('result-panel').textContent = resultText || '(empty)';

  } catch (err) {
    if (err.name === 'AbortError') return;
    document.getElementById('events-panel').innerHTML = `<div class="event-line event-error">Error: ${escapeHtml(err.message)}</div>`;
  } finally {
    currentAbort = null;
  }
}

function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

document.getElementById('user-input').addEventListener('keydown', e => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendMessage(); }
});
</script>
</body>
</html>"###;

// ══════════════════════════════════════════════════════════════════════
//  Main
// ══════════════════════════════════════════════════════════════════════

fn main() {
    let scenarios_map: HashMap<&'static str, Vec<SseEvent>> = scenarios();
    let scenario_name: Arc<Mutex<String>> = Arc::new(Mutex::new("Plain text".into()));

    // Start mock server
    let (mock_port, _mock_stop) = start_mock_server(scenario_name.clone(), Arc::new(scenarios_map));

    // Start web UI
    let listener = TcpListener::bind("127.0.0.1:3000").expect("failed to bind :3000");
    let mock_port_for_html = mock_port;

    println!("╔══════════════════════════════════════════════╗");
    println!("║  QAQ-Harness · Gate Test Harness                  ║");
    println!("╠══════════════════════════════════════════════╣");
    println!(
        "║  Mock API : http://127.0.0.1:{:<4}             ║",
        mock_port
    );
    println!("║  Test UI  : http://127.0.0.1:3000           ║");
    println!("╚══════════════════════════════════════════════╝");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mock_port = mock_port_for_html;

        // Set read timeout so we don't hang
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

        let req = match read_http_request(&mut stream) {
            Some(r) => r,
            None => continue,
        };

        if req.method == "GET" && req.path == "/" {
            write_status(&mut stream, "200 OK");
            write_header(&mut stream, "Content-Type", "text/html; charset=utf-8");
            write_body(&mut stream, HTML_PAGE.as_bytes());
        } else if req.method == "POST" && req.path == "/api/chat" {
            handle_chat(stream, &req, mock_port, &scenario_name);
        } else {
            write_status(&mut stream, "404 Not Found");
            write_body(&mut stream, b"Not found");
        }
    }
}

fn handle_chat(
    mut stream: TcpStream,
    req: &HttpRequest,
    mock_port: u16,
    scenario_name: &Arc<Mutex<String>>,
) {
    // Parse request body
    let body_str = String::from_utf8_lossy(&req.body);
    let json: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(_) => {
            write_status(&mut stream, "400 Bad Request");
            write_body(&mut stream, b"Invalid JSON");
            return;
        }
    };

    let user_text = json["text"].as_str().unwrap_or("hello");
    let scenario = json["scenario"].as_str().unwrap_or("Plain text");

    // Update mock scenario
    *scenario_name.lock().unwrap() = scenario.to_string();

    // Build provider config pointing to mock server
    let base_url = format!("http://127.0.0.1:{}", mock_port);
    let provider = ProviderConfig::openai(
        &base_url,
        "sk-test-key",
        "test-model",
        None,
        None,
        Default::default(),
        Default::default(),
        false,
        None,
    );

    let messages = vec![
        Message::system("You are a helpful assistant."),
        Message::user(user_text),
    ];

    let tools = vec![ToolDef {
        call_type: "function".into(),
        function: ToolFunction {
            name: "read".into(),
            description: "Read the contents of a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"}
                },
                "required": ["path"]
            }),
        },
    }];

    // Write SSE response headers
    write_status(&mut stream, "200 OK");
    write_header(&mut stream, "Content-Type", "text/event-stream");
    write_header(&mut stream, "Cache-Control", "no-cache");
    write_header(&mut stream, "Connection", "keep-alive");
    write_header(&mut stream, "Access-Control-Allow-Origin", "*");
    let _ = write!(stream, "\r\n");
    let _ = stream.flush();

    let cancel = Arc::new(AtomicBool::new(false));

    // Run the gate in a separate thread so we can stream via channel
    let (tx, rx) = mpsc::channel::<String>();
    let tx_for_gate = tx.clone();

    thread::spawn(move || {
        let _ = qaqh_gate::chat_stream(
            &provider,
            messages,
            Some(tools),
            4096,
            Some("high".into()),
            None,
            Some(&cancel),
            &mut |ev| {
                let line = match ev {
                    StreamEvent::ContentDelta(t) => {
                        json!({"type":"delta","content":t}).to_string()
                    }
                    StreamEvent::ReasoningDelta(r) => {
                        json!({"type":"reasoning","content":r}).to_string()
                    }
                    StreamEvent::ToolCallProgress { index, id, name, args_so_far } => {
                        json!({"type":"tool_call","index":index,"id":id,"name":name,"args":args_so_far}).to_string()
                    }
                    StreamEvent::WebSearchStatus(status) => {
                        json!({"type":"web_search","status":status}).to_string()
                    }
                    StreamEvent::Done { raw_message, usage, stop_reason } => {
                        let msg_json: serde_json::Value = serde_json::json!({
                            "role": raw_message.role,
                            "content": raw_message.content.iter().map(|b| match b {
                                qaqh_types::ContentBlock::Text { text } => json!({"type":"text","text":text}),
                                qaqh_types::ContentBlock::Reasoning { reasoning } => json!({"type":"reasoning","reasoning":reasoning}),
                                qaqh_types::ContentBlock::ToolUse { id, name, input } => json!({"type":"tool_use","id":id,"name":name,"input":input}),
                                _ => json!(null),
                            }).collect::<Vec<_>>(),
                        });
                        json!({"type":"done","message":msg_json,"usage":usage,"stop_reason":stop_reason}).to_string()
                    }
                    StreamEvent::UsageUpdate(u) => { json!({"type":"usage","usage":u}).to_string() }
                    StreamEvent::Error(e) => {
                        json!({"type":"error","message":e}).to_string()
                    }
                    StreamEvent::Retrying { attempt, max_retries, delay_secs, error } => {
                        json!({"type":"retrying","attempt":attempt,"max_retries":max_retries,"delay_secs":delay_secs,"error":error}).to_string()
                    }
                };
                let _ = tx_for_gate.send(line);
            },
        );
        let _ = tx_for_gate.send("__DONE__".into());
    });

    // Forward channel messages to SSE stream
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => {
                if msg == "__DONE__" {
                    write_sse_event(&mut stream, "[DONE]");
                    break;
                }
                write_sse_event(&mut stream, &msg);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep alive: send a comment line
                let _ = write!(stream, ": keepalive\n\n");
                let _ = stream.flush();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}
