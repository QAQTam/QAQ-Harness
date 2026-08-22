//! `qaqh-workspace serve` — HTTP tool service.
//!
//! Runs the tool registry as a loopback HTTP service so the daemon (or any
//! host) can execute tools out-of-process without embedding the tool runtime.
//!
//! Protocol v1 (transport-neutral, Bearer-authenticated):
//!
//! ```text
//! GET  /health          -> 200 {"ok":true,"tools":N,"version":"..."}
//! GET  /tools           -> 200 [{"name","description","parameters"}, ...]
//! POST /execute         -> 200 {"status":"ok|error|...", ...}
//!                         400 invalid body / 401 bad token / 404 unknown tool
//!                         500 execution failure
//!   body: {"session_id","workspace","name","args",
//!          "action"?, "call_id"?, "timeout_secs"?}
//! auth:  Authorization: Bearer <token> on every endpoint
//! ```
//!
//! Execution model:
//! - The tool runtime keeps **process-global** session/workspace state
//!   (`runtime::set_context`, `workspace::set_process_workspace`), so tool
//!   calls are serialized on one dedicated worker thread via a bounded queue.
//! - Control endpoints (`/health`, `/tools`) stay responsive on the HTTP
//!   thread pool and never block behind a running tool.
//! - The service trusts its caller (daemon) for authorization: the host is
//!   responsible for permission decisions before dispatch. `permission_level`
//!   is fixed at the CLI level (4) inside this process.
//! - Tool output is bounded by the tool handlers themselves (exec truncation,
//!   edit_file summaries); the response never streams unbounded body text.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bounded queue capacity for pending executions (backpressure beyond this
/// rejects with 429 instead of unbounded buffering).
const EXECUTE_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, Serialize)]
struct HealthResponse {
    ok: bool,
    tools: usize,
    version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ToolInfo {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ExecuteRequest {
    session_id: String,
    workspace: String,
    name: String,
    #[serde(default)]
    action: String,
    #[serde(default = "default_args")]
    args: serde_json::Value,
    #[serde(default)]
    call_id: String,
    #[serde(default, rename = "timeout_secs")]
    timeout_secs: Option<u64>,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

/// worker 执行结果 + 账本变更增量（随 HTTP 响应回传，daemon 侧同步本地账本）。
struct ExecuteOutcome {
    result: crate::ToolResult,
    state_delta: Vec<crate::file_state::StateEntry>,
}

/// Job handed to the serial execution worker.
struct ExecuteJob {
    request: ExecuteRequest,
    respond: mpsc::Sender<ExecuteOutcome>,
}

fn authorized(request: &tiny_http::Request, token: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|candidate| candidate == token)
}

fn json_response(
    status: tiny_http::StatusCode,
    payload: &impl Serialize,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let len = body.len();
    let headers = vec![
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("static header is valid"),
    ];
    tiny_http::Response::new(status, headers, std::io::Cursor::new(body), Some(len), None)
}

fn text_response(
    status: tiny_http::StatusCode,
    text: &str,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = text.as_bytes().to_vec();
    let len = body.len();
    let headers = vec![
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
            .expect("static header is valid"),
    ];
    tiny_http::Response::new(status, headers, std::io::Cursor::new(body), Some(len), None)
}

/// 执行单个工具调用（catch_unwind 防护），返回结果 + 账本增量。
/// 串行 worker 与 process 内联分支共用。
fn run_tool(request: &ExecuteRequest) -> ExecuteOutcome {
    let name = request.name.clone();
    let args = serde_json::to_string(&request.args).unwrap_or_else(|_| "{}".into());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::execution::execute_with_context(
            &request.name,
            &request.action,
            &args,
            &request.call_id,
            None,
        )
    }))
    .unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        crate::execution::ToolExecResult {
            content: format!("[ERROR] tool '{name}' panicked: {message}"),
            success: false,
            result: crate::ToolResult::error(format!("tool '{name}' panicked: {message}")),
            meta: crate::ToolExecMeta {
                name: name.clone(),
                elapsed_ms: 0,
                output_size: 0,
                success: false,
                args_summary: String::new(),
            },
            code_delta: None,
            skill_effects: Vec::new(),
        }
    });
    ExecuteOutcome {
        result: result.result,
        state_delta: crate::file_state::take_pending(),
    }
}

/// 扁平协议响应：ToolResult 序列化为顶层字段 + state_delta 附加
/// （daemon 端 HttpExecuteResponse 用 #[serde(flatten)] 解析）。
fn respond_outcome(request: tiny_http::Request, outcome: ExecuteOutcome) {
    let mut payload = serde_json::to_value(&outcome.result).unwrap_or_else(
        |_| serde_json::json!({ "status": "error", "message": "result serialization failed" }),
    );
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "state_delta".to_string(),
            serde_json::to_value(&outcome.state_delta)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new())),
        );
    }
    let _ = request.respond(json_response(tiny_http::StatusCode(200), &payload));
}

/// 工具真实超时：模型参数 `args.timeout_secs` 优先（与 daemon 侧
/// `HttpToolExecutionBackend::tool_timeout_from_args` 同源），回退请求级
/// `timeout_secs` 字段（= 注册 default_timeout），再回退 30s。serve 用它给
/// `recv_timeout` 定上限，保证代理层不会比 exec 等长任务更早放弃。
fn request_timeout(req: &ExecuteRequest) -> u64 {
    req.args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0 && n <= 3600)
        .or_else(|| req.timeout_secs.filter(|&n| n > 0 && n <= 3600))
        .unwrap_or(30)
}
fn handle_execute(
    mut request: tiny_http::Request,
    _token: &str,
    tx: &mpsc::SyncSender<ExecuteJob>,
    next_id: &AtomicU64,
) {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(400),
            "read body failed",
        ));
        return;
    }
    let mut parsed: ExecuteRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(error) => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(400),
                &format!("invalid execute body: {error}"),
            ));
            return;
        }
    };
    if parsed.session_id.is_empty() || parsed.workspace.is_empty() || parsed.name.is_empty() {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(400),
            "session_id, workspace, and name are required",
        ));
        return;
    }
    if crate::runtime::all_tools()
        .iter()
        .all(|def| def.function.name != parsed.name)
    {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(404),
            &format!("unknown tool: {}", parsed.name),
        ));
        return;
    }
    if parsed.call_id.is_empty() {
        parsed.call_id = format!("serve_{}", next_id.fetch_add(1, Ordering::SeqCst));
    }

    // process 工具（check/wait/kill）内联执行：进程注册表是内存状态且线程
    // 安全，无需串行 worker——长任务（如长 exec 跑数分钟）执行期间必须能
    // 即时 check/kill 抢占，否则 kill 请求会排队到任务结束，无法中断。
    // 不 set workspace：进程注册表与 workspace 无关，避免干扰 worker 状态。
    if parsed.name == "process" {
        crate::runtime::set_context(&parsed.session_id, 4);
        let outcome = run_tool(&parsed);
        respond_outcome(request, outcome);
        return;
    }

    let wait_secs = request_timeout(&parsed).saturating_add(30);
    let (respond, received) = mpsc::channel();
    match tx.send(ExecuteJob {
        request: parsed,
        respond,
    }) {
        Ok(()) => {
            // 等待串行 worker，但带代理层超时（工具真实超时 + 30s 余量）：
            // 防止 HTTP 线程无限挂起堆积（事故根因之一）。超时 ≠ 取消：
            // worker 继续执行，仅释放 HTTP 线程；daemon 侧收到 error 结果
            // 后按"执行中失败"处理，不会本地重跑（副作用安全）。
            let wait = Duration::from_secs(wait_secs);
            match received.recv_timeout(wait) {
                Ok(outcome) => respond_outcome(request, outcome),
                Err(_) => {
                    let outcome = ExecuteOutcome {
                        result: crate::ToolResult::error(format!(
                            "execution still running after proxy timeout ({}s); \
                             check with the `process` tool if the tool supports backgrounding",
                            wait.as_secs()
                        )),
                        state_delta: Vec::new(),
                    };
                    respond_outcome(request, outcome);
                }
            }
        }
        Err(_) => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(429),
                "execution queue full; retry later",
            ));
        }
    }
}

/// ── 子代理状态端点 ──────────────────────────────────────────────
///
/// 子代理（spawn_subagent）的进程记录注册在**本进程**的 ProcessRegistry，
/// 使主代理的 `process check/wait/kill`（Workspace placement，同样在本进程
/// 执行）能查到子代理状态——修复此前“子代理记录注册在 worker 进程、process
/// 工具却在 serve 进程执行”的跨进程错位（查不到/误杀同号进程）。
///
/// 协议：POST /subagent，Bearer 鉴权；body:
///   {"action":"register","name":"subagent:xxx"}          -> {"ok":true,"id":N}
///   {"action":"update","id":N,"answer":"...","exit_code":0} -> {"ok":true}
///   {"action":"status","id":N}                            -> {"ok":true,"info":{...}}
///   {"action":"kill","id":N}                              -> {"ok":true|false}
/// 未知 action / 缺参数 -> 400。id 不存在时 update 仍返回 ok（幂等）。
#[derive(Debug, Deserialize)]
struct SubagentRequest {
    action: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    answer: String,
    #[serde(default)]
    exit_code: i32,
}

fn handle_subagent(mut request: tiny_http::Request) {
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() {
        let _ = request.respond(text_response(
            tiny_http::StatusCode(400),
            "read body failed",
        ));
        return;
    }
    let parsed: SubagentRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(error) => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(400),
                &format!("invalid subagent body: {error}"),
            ));
            return;
        }
    };
    let outcome: serde_json::Value = match parsed.action.as_str() {
        "register" => {
            if parsed.name.trim().is_empty() {
                let _ = request.respond(text_response(
                    tiny_http::StatusCode(400),
                    "register requires a non-empty name",
                ));
                return;
            }
            let id = crate::process_registry::ProcessRegistry::register(parsed.name.trim());
            log::info!(
                "[serve] subagent registered id={id} name={}",
                parsed.name.trim()
            );
            serde_json::json!({ "ok": true, "id": id })
        }
        "update" => {
            if !parsed.answer.is_empty() {
                crate::process_registry::ProcessRegistry::set_answer(parsed.id, parsed.answer);
            }
            crate::process_registry::ProcessRegistry::mark_exited(parsed.id, parsed.exit_code);
            serde_json::json!({ "ok": true })
        }
        "status" => {
            let _ = crate::process_registry::ProcessRegistry::try_wait(parsed.id);
            match crate::process_registry::ProcessRegistry::get_info(parsed.id) {
                Some(info) => serde_json::json!({ "ok": true, "info": info }),
                None => serde_json::json!({ "ok": false, "info": null }),
            }
        }
        "kill" => {
            let ok = crate::process_registry::ProcessRegistry::kill(parsed.id);
            log::info!("[serve] subagent kill id={} ok={ok}", parsed.id);
            serde_json::json!({ "ok": ok })
        }
        other => {
            let _ = request.respond(text_response(
                tiny_http::StatusCode(400),
                &format!("unknown subagent action: {other}"),
            ));
            return;
        }
    };
    let _ = request.respond(json_response(tiny_http::StatusCode(200), &outcome));
}

/// Run the tool service until the process is terminated.
///
/// Binds `host:port` (port 0 picks an ephemeral port), prints the ready line
/// `QAQH_WORKSPACE_READY <host>:<port>` to stdout for the spawning daemon,
/// then serves requests.
pub fn serve(host: &str, port: u16, token: &str) -> Result<(), String> {
    let addr = format!("{host}:{port}");
    let server = tiny_http::Server::http(addr.as_str()).map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = server.server_addr().to_string();

    crate::runtime::init_tools("workspace-serve", &[], vec![]);
    crate::runtime::set_context("workspace-serve", 4);
    let default_workspace = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    crate::workspace::set_process_workspace(&default_workspace);

    // Serial execution worker: tool runtime state is process-global.
    let (tx, rx) = mpsc::sync_channel::<ExecuteJob>(EXECUTE_QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("workspace-executor".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                crate::runtime::set_context(&job.request.session_id, 4);
                crate::workspace::set_process_workspace(&job.request.workspace);
                let outcome = run_tool(&job.request);
                let _ = job.respond.send(outcome);
            }
            // channel 断开（唯一 sender 是 HTTP 线程持有的 Arc<tx>，只有
            // 在 serve 关闭路径才会全断）→ 异常退出，让 supervisor 重启。
            // 正常 shutdown 由外部 taskkill 完成，不会走到这里。
            log::error!(
                "[workspace] executor thread ended unexpectedly; exiting for supervisor restart"
            );
            std::process::exit(70);
        })
        .map_err(|e| format!("spawn executor thread: {e}"))?;

    // Ready line: the daemon reads this from stdout to learn the port.
    println!("QAQH_WORKSPACE_READY {bound}");
    let _ = std::io::stdout().flush();
    log::info!("qaqh-workspace serve listening on {bound}");

    let next_id = Arc::new(AtomicU64::new(1));
    let tx = Arc::new(tx);
    for request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let token = token.to_string();
        let tx = tx.clone();
        let next_id = next_id.clone();
        std::thread::Builder::new()
            .name("workspace-http".into())
            .spawn(move || {
                if !authorized(&request, &token) {
                    let _ =
                        request.respond(text_response(tiny_http::StatusCode(401), "unauthorized"));
                    return;
                }
                let path = url.split('?').next().unwrap_or("");
                match (method.as_str(), path) {
                    ("GET", "/health") => {
                        let tools = crate::runtime::all_tools().len();
                        let _ = request.respond(json_response(
                            tiny_http::StatusCode(200),
                            &HealthResponse {
                                ok: true,
                                tools,
                                version: env!("CARGO_PKG_VERSION"),
                            },
                        ));
                    }
                    ("GET", "/tools") => {
                        let tools: Vec<ToolInfo> = crate::runtime::all_tools()
                            .into_iter()
                            .map(|def| ToolInfo {
                                name: def.function.name.clone(),
                                description: def.function.description.clone(),
                                parameters: def.function.parameters.clone(),
                            })
                            .collect();
                        let _ = request.respond(json_response(tiny_http::StatusCode(200), &tools));
                    }
                    ("POST", "/execute") => {
                        handle_execute(request, &token, &tx, &next_id);
                    }
                    ("POST", "/subagent") => {
                        handle_subagent(request);
                    }
                    _ => {
                        let _ =
                            request.respond(text_response(tiny_http::StatusCode(404), "not found"));
                    }
                }
            })
            .map_err(|e| log::warn!("spawn http thread: {e}"))
            .ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(args: serde_json::Value) -> ExecuteRequest {
        ExecuteRequest {
            session_id: "s".into(),
            workspace: "w".into(),
            name: "exec".into(),
            action: String::new(),
            args,
            call_id: "c".into(),
            timeout_secs: None,
        }
    }

    #[test]
    fn request_timeout_prefers_args() {
        assert_eq!(
            request_timeout(&req(serde_json::json!({ "timeout_secs": 120 }))),
            120
        );
    }

    #[test]
    fn request_timeout_falls_back_to_field() {
        let mut r = req(serde_json::json!({}));
        r.timeout_secs = Some(45);
        assert_eq!(request_timeout(&r), 45);
    }

    #[test]
    fn request_timeout_defaults_and_clamps() {
        assert_eq!(request_timeout(&req(serde_json::json!({}))), 30);
        // 超范围参数被忽略，回退默认。
        assert_eq!(
            request_timeout(&req(serde_json::json!({ "timeout_secs": 0 }))),
            30
        );
        assert_eq!(
            request_timeout(&req(serde_json::json!({ "timeout_secs": 99999 }))),
            30
        );
        // 非数字参数被忽略。
        assert_eq!(
            request_timeout(&req(serde_json::json!({ "timeout_secs": "fast" }))),
            30
        );
    }
}
