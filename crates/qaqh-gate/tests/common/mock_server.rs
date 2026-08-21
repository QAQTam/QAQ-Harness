//! Mock OpenAI-compatible server for integration testing the gate.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::json;
use tiny_http::{Header, Response, Server, StatusCode};

// ── SseChunk ──────────────────────────────────────────────────────────

/// One SSE data event, a delay, or an HTTP error.
#[derive(Clone)]
pub enum SseChunk {
    Data(serde_json::Value),
    Raw(String),
    #[allow(dead_code)]
    Delay(Duration),
    HttpError(u16, serde_json::Value),
}

impl SseChunk {
    pub fn text(text: &str) -> Self {
        Self::delta(json!({"content": text}))
    }

    pub fn reasoning(text: &str) -> Self {
        Self::delta(json!({"reasoning_content": text}))
    }

    /// A tool-call delta (OpenAI native format). `args` is the full
    /// arguments JSON string for this chunk.
    pub fn tool_call(index: u32, id: &str, name: &str, args: &str) -> Self {
        Self::delta(json!({
            "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            }]
        }))
    }

    pub fn delta(fields: serde_json::Value) -> Self {
        SseChunk::Data(json!({
            "choices": [{ "index": 0, "delta": fields }]
        }))
    }

    /// Finish chunk with an optional usage object.
    pub fn finish(reason: &str, usage: Option<serde_json::Value>) -> Self {
        let mut obj = json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }]
        });
        if let Some(u) = usage {
            obj["usage"] = u;
        }
        SseChunk::Data(obj)
    }

    /// `data: [DONE]\n\n`
    pub fn done() -> Self {
        SseChunk::Raw("[DONE]".into())
    }

    pub fn delay_ms(ms: u64) -> Self {
        SseChunk::Delay(Duration::from_millis(ms))
    }

    pub fn error(status: u16, message: &str) -> Self {
        SseChunk::HttpError(status, json!({"error": {"message": message}}))
    }
}

/// Standard usage info JSON.
pub fn usage(prompt: u32, completion: u32) -> serde_json::Value {
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    })
}

pub fn usage_with_cache(
    prompt: u32,
    completion: u32,
    cache_hit: u32,
    cache_miss: u32,
) -> serde_json::Value {
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_cache_hit_tokens": cache_hit,
        "prompt_cache_miss_tokens": cache_miss,
        "completion_tokens_details": { "reasoning_tokens": 7 },
    })
}

// ── MockServer ───────────────────────────────────────────────────────

enum ScenarioSource {
    /// Same scenario for every request.
    Fixed(Vec<SseChunk>),
    /// Rotating scenarios, one per request.
    Sequential(Vec<Vec<SseChunk>>),
}

/// A tiny HTTP server that responds to POST `/chat/completions` with
/// predefined SSE scenarios.
pub struct MockServer {
    pub port: u16,
    handle: Option<thread::JoinHandle<()>>,
    stop: Arc<Mutex<bool>>,
    /// Number of requests handled.
    pub request_count: Arc<AtomicUsize>,
    /// Last request body (for inspection).
    pub last_request_body: Arc<Mutex<Option<String>>>,
}

fn serve_scenario(req: tiny_http::Request, scenario: &[SseChunk]) {
    let mut sse = String::new();
    let mut error_response: Option<(u16, String)> = None;

    for chunk in scenario {
        match chunk {
            SseChunk::Data(val) => {
                sse.push_str(&format!("data: {}\n\n", val.to_string()));
            }
            SseChunk::Raw(raw) => {
                if raw == "[DONE]" {
                    sse.push_str("data: [DONE]\n\n");
                } else {
                    sse.push_str(raw);
                    sse.push('\n');
                }
            }
            SseChunk::Delay(delay) => thread::sleep(*delay),
            SseChunk::HttpError(status, body_val) => {
                error_response = Some((*status, body_val.to_string()));
                break;
            }
        }
    }

    if let Some((status, body)) = error_response {
        let status_code = StatusCode(status);
        let resp = Response::from_string(body).with_status_code(status_code);
        let _ = req.respond(resp);
    } else if !sse.is_empty() {
        let resp = Response::from_string(sse)
            .with_header("Content-Type: text/event-stream".parse::<Header>().unwrap());
        let _ = req.respond(resp);
    }
}

fn run_server(
    server: Server,
    source: Arc<Mutex<ScenarioSource>>,
    stop: Arc<Mutex<bool>>,
    request_count: Arc<AtomicUsize>,
    last_body: Arc<Mutex<Option<String>>>,
) {
    let mut seq_index: usize = 0;
    loop {
        if *stop.lock().unwrap() {
            break;
        }
        let mut req = match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("[mock] recv error: {e}");
                break;
            }
        };

        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_ok() {
            // body read successfully
        }
        *last_body.lock().unwrap() = Some(body);
        request_count.fetch_add(1, Ordering::SeqCst);

        // Get the scenario for this request
        let scenario = {
            let mut src = source.lock().unwrap();
            match &mut *src {
                ScenarioSource::Fixed(s) => s.clone(),
                ScenarioSource::Sequential(list) => {
                    let idx = seq_index % list.len();
                    seq_index += 1;
                    list[idx].clone()
                }
            }
        };

        serve_scenario(req, &scenario);
    }
}

impl MockServer {
    /// Serve the same scenario for every request.
    pub fn new(scenario: Vec<SseChunk>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("failed to bind mock server");
        let port = server.server_addr().to_ip().unwrap().port();
        let stop = Arc::new(Mutex::new(false));
        let request_count = Arc::new(AtomicUsize::new(0));
        let last_body = Arc::new(Mutex::new(None));
        let source = Arc::new(Mutex::new(ScenarioSource::Fixed(scenario)));

        let handle = {
            let stop = stop.clone();
            let rc = request_count.clone();
            let lb = last_body.clone();
            let src = source.clone();
            thread::spawn(|| run_server(server, src, stop, rc, lb))
        };

        MockServer {
            port,
            handle: Some(handle),
            stop,
            request_count,
            last_request_body: last_body,
        }
    }

    /// Serve scenarios in rotation, one per request.
    pub fn new_sequential(scenarios: Vec<Vec<SseChunk>>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("failed to bind mock server");
        let port = server.server_addr().to_ip().unwrap().port();
        let stop = Arc::new(Mutex::new(false));
        let request_count = Arc::new(AtomicUsize::new(0));
        let last_body = Arc::new(Mutex::new(None));
        let source = Arc::new(Mutex::new(ScenarioSource::Sequential(scenarios)));

        let handle = {
            let stop = stop.clone();
            let rc = request_count.clone();
            let lb = last_body.clone();
            let src = source.clone();
            thread::spawn(|| run_server(server, src, stop, rc, lb))
        };

        MockServer {
            port,
            handle: Some(handle),
            stop,
            request_count,
            last_request_body: last_body,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn last_request_json(&self) -> Option<serde_json::Value> {
        let guard = self.last_request_body.lock().unwrap();
        guard.as_ref().and_then(|s| serde_json::from_str(s).ok())
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
