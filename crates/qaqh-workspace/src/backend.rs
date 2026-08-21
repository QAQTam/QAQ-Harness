//! Tool execution backend routing.
//!
//! Authorization, admission, inflight tracking, finalization, and auditing stay
//! in the host Agent Worker. Backends only own the phase that invokes an
//! already-prepared tool call.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use crate::{ToolCallCtx, ToolResult};

/// Where a registered tool must execute.
///
/// Existing tools default to [`HostOnly`](Self::HostOnly). Workspace tools are
/// opted in explicitly as their remote execution contract is implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPlacement {
    #[default]
    HostOnly,
    Workspace,
}

/// Local handler function retained by a prepared call.
pub type ToolHandlerFn = fn(ToolCallCtx) -> ToolResult;

/// An authorized and prepared call passed to an execution backend.
pub struct BackendRequest {
    pub session_id: String,
    pub host_workspace: PathBuf,
    pub authorized_resources: Vec<PathBuf>,
    pub local_handler: ToolHandlerFn,
    pub ctx: ToolCallCtx,
}

/// Executes the data-plane phase of an already-authorized tool call.
pub trait ToolExecutionBackend: Send + Sync {
    fn execute(&self, request: BackendRequest) -> ToolResult;
}

/// Default backend that preserves the current in-process handler behavior.
#[derive(Debug, Default)]
pub struct LocalToolExecutionBackend;

impl ToolExecutionBackend for LocalToolExecutionBackend {
    fn execute(&self, request: BackendRequest) -> ToolResult {
        (request.local_handler)(request.ctx)
    }
}

/// HTTP backend that forwards prepared calls to a `qaqh-workspace serve`
/// instance (local process or WSL). Fallback policy:
/// - 连接失败（serve 未启动/不可达）→ 回退进程内 handler（渐进式兼容）；
/// - 执行中的失败（读超时、断连、响应解析失败）→ **不**本地重跑——
///   serve 端工具可能已执行/仍在执行，重跑会重复副作用；
/// - HTTP 读超时按工具的模型参数 `args.timeout_secs` 放大（+30s 余量），
///   避免代理层比 exec 等长任务更早放弃。
#[derive(Debug, Clone)]
pub struct HttpToolExecutionBackend {
    pub endpoint: String,
    pub token: String,
}

impl HttpToolExecutionBackend {
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
        }
    }
}

#[derive(serde::Serialize)]
struct HttpExecuteRequest<'a> {
    session_id: &'a str,
    workspace: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "String::is_empty")]
    action: String,
    args: &'a serde_json::Value,
    call_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

#[derive(serde::Deserialize)]
struct HttpExecuteResponse {
    #[serde(flatten)]
    result: ToolResult,
    /// serve 端执行产生的账本变更增量，回写 daemon 本地账本
    /// （Environment 的 `<file_state>` 注入依赖它）。
    #[serde(default)]
    state_delta: Vec<crate::file_state::StateEntry>,
}

/// 工具侧真实超时（模型参数优先，回退框架注入的 ctx.timeout_secs）。
/// exec 等工具的 timeout_secs 参数可达 3600，框架层 ctx.timeout_secs 只是
/// 注册的 default_timeout（如 exec=30s）——HTTP 读超时若按后者放大，
/// exec 跑超过 60s 就会被代理层误判超时并触发本地重跑（副作用重复！）。
fn tool_timeout_from_args(ctx: &ToolCallCtx) -> u64 {
    ctx.args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or_else(|| ctx.timeout_secs.unwrap_or(30).clamp(1, 3600))
}

impl ToolExecutionBackend for HttpToolExecutionBackend {
    fn execute(&self, request: BackendRequest) -> ToolResult {
        let ctx = &request.ctx;
        let payload = HttpExecuteRequest {
            session_id: &request.session_id,
            workspace: &request.host_workspace.to_string_lossy(),
            name: &ctx.name,
            action: ctx.action.clone(),
            args: &ctx.args,
            call_id: &ctx.id,
            timeout_secs: ctx.timeout_secs,
        };
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(error) => {
                log::warn!(
                    "[workspace-backend] serialize execute request: {error}; fallback local"
                );
                return (request.local_handler)(request.ctx);
            }
        };

        // HTTP 读超时按工具真实超时放大（+30s 网络余量），避免代理层比
        // 服务端更早放弃；连接/写超时保持有界（服务不可达快速回退）。
        let timeout = tool_timeout_from_args(ctx).saturating_add(30).min(3600);
        let url = format!("{}/execute", self.endpoint.trim_end_matches('/'));
        let result = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(5)))
            .timeout_per_call(Some(std::time::Duration::from_secs(timeout)))
            .build()
            .new_agent()
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body);

        match result {
            Ok(mut response) => {
                let parsed = response
                    .body_mut()
                    .read_json::<HttpExecuteResponse>()
                    .map_err(|e| e.to_string());
                match parsed {
                    Ok(resp) => {
                        // 回写 serve 端产生的账本变更（文件状态注入依赖）
                        if !resp.state_delta.is_empty() {
                            let n = resp.state_delta.len();
                            crate::file_state::apply_pending(resp.state_delta);
                            log::debug!(
                                "[workspace-backend] applied {n} file-state entries from serve"
                            );
                        }
                        resp.result
                    }
                    Err(error) => {
                        // 响应已发出但解析失败：serve 端可能已执行（副作用已发生），
                        // 绝不本地重跑。
                        log::warn!(
                            "[workspace-backend] invalid execute response: {error}; NOT retrying locally"
                        );
                        workspace_exec_failed(&format!("invalid execute response: {error}"))
                    }
                }
            }
            Err(error) => {
                // 仅"服务不可达"回退本地：请求从未到达 serve，无副作用风险。
                // ureq 3 把 Connection refused 包装为 Error::Io（From<io::Error>），
                // Error::ConnectionFailed 仅自定义 connector 才产生——必须同时
                // 匹配 Io(ConnectionRefused|NotConnected)，否则回退永不触发，
                // serve 一死 L1 工具（read/write/edit/exec/grep）全部不可用。
                // 执行中的失败（ConnectionReset 断连、TimedOut 读超时、HTTP
                // 错误）绝不本地重跑——serve 端工具可能已执行/仍在执行。
                if is_connect_failure(&error) {
                    log::warn!(
                        "[workspace-backend] {} unreachable ({error}); fallback local",
                        self.endpoint
                    );
                    return (request.local_handler)(request.ctx);
                }
                log::warn!(
                    "[workspace-backend] {} execute failed mid-flight ({error}); NOT retrying locally",
                    self.endpoint
                );
                workspace_exec_failed(&error.to_string())
            }
        }
    }
}

/// 执行中失败的统一错误响应（不触发本地重跑）。
/// 判定"请求从未到达 serve"的连接阶段失败：这些情况下本地回退是安全的
/// （serve 端不可能已产生副作用）。
///
/// ureq 3 的 `Error::ConnectionFailed` 仅由自定义 connector 产生；默认
/// connector 的 refused/not-connected 经 `From<io::Error>` 包装为
/// `Error::Io`——两者都必须识别。`ConnectionReset`（断连）与 `TimedOut`
/// （读超时）可能是执行中失败，绝不回退。
fn is_connect_failure(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}
fn workspace_exec_failed(detail: &str) -> ToolResult {
    ToolResult::error(
        serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "WORKSPACE_EXEC_FAILED",
            "message": format!("workspace serve execute failed: {detail}"),
            "hint": "The workspace service did not respond in time — it may still be executing the tool (e.g. a long-running exec). The call was NOT retried locally to avoid duplicate side effects. If the previous call is still running, wait for it, then resend if needed.",
        })
        .to_string(),
    )
}

fn backend_slot() -> &'static RwLock<Arc<dyn ToolExecutionBackend>> {
    static WORKSPACE_BACKEND: OnceLock<RwLock<Arc<dyn ToolExecutionBackend>>> = OnceLock::new();
    WORKSPACE_BACKEND.get_or_init(|| RwLock::new(Arc::new(LocalToolExecutionBackend)))
}

fn active_workspace_backend() -> Arc<dyn ToolExecutionBackend> {
    backend_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn swap_workspace_backend(backend: Arc<dyn ToolExecutionBackend>) -> Arc<dyn ToolExecutionBackend> {
    let mut current = backend_slot()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::replace(&mut *current, backend)
}

/// Install the backend used by tools registered with [`ToolPlacement::Workspace`].
///
/// Calls already in flight retain the backend they acquired before the swap.
pub fn install_workspace_backend(backend: Arc<dyn ToolExecutionBackend>) {
    drop(swap_workspace_backend(backend));
}

/// Restore in-process execution for workspace tools.
pub fn use_local_workspace_backend() {
    install_workspace_backend(Arc::new(LocalToolExecutionBackend));
}

pub(crate) fn execute(
    placement: ToolPlacement,
    session_id: String,
    host_workspace: PathBuf,
    authorized_resources: Vec<PathBuf>,
    local_handler: ToolHandlerFn,
    ctx: ToolCallCtx,
) -> ToolResult {
    let request = BackendRequest {
        session_id,
        host_workspace,
        authorized_resources,
        local_handler,
        ctx,
    };
    match placement {
        ToolPlacement::HostOnly => LocalToolExecutionBackend.execute(request),
        ToolPlacement::Workspace => active_workspace_backend().execute(request),
    }
}

#[cfg(test)]
pub(crate) struct WorkspaceBackendTestGuard {
    previous: Option<Arc<dyn ToolExecutionBackend>>,
}

#[cfg(test)]
impl Drop for WorkspaceBackendTestGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            drop(swap_workspace_backend(previous));
        }
    }
}

#[cfg(test)]
pub(crate) fn replace_workspace_backend_for_test(
    backend: Arc<dyn ToolExecutionBackend>,
) -> WorkspaceBackendTestGuard {
    WorkspaceBackendTestGuard {
        previous: Some(swap_workspace_backend(backend)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    #[test]
    fn connect_failure_classification() {
        use std::io;

        // 请求从未到达 serve：安全回退本地。
        assert!(is_connect_failure(&ureq::Error::ConnectionFailed));
        assert!(is_connect_failure(&ureq::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Connection refused"
        ))));
        assert!(is_connect_failure(&ureq::Error::Io(io::Error::new(
            io::ErrorKind::NotConnected,
            "socket not connected"
        ))));

        // 执行中失败：绝不回退（副作用可能已发生）。
        assert!(!is_connect_failure(&ureq::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "remote closed"
        ))));
        assert!(!is_connect_failure(&ureq::Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "read timeout"
        ))));
        assert!(!is_connect_failure(&ureq::Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer disconnected"
        ))));
        assert!(!is_connect_failure(&ureq::Error::StatusCode(500)));
    }
    fn serve_bin_path() -> std::path::PathBuf {
        // 测试运行于 target/<profile>/deps/qaqh_workspace-<hash>.exe；
        // serve 二进制在 target/<profile>/qaqh-workspace(.exe)。
        let mut exe = std::env::current_exe().expect("test exe path");
        exe.pop(); // deps/
        exe.pop(); // <profile>/
        exe.push(if cfg!(windows) {
            "qaqh-workspace.exe"
        } else {
            "qaqh-workspace"
        });
        exe
    }

    /// RAII 守护 serve 子进程：测试任何路径（断言失败、panic、unreachable
    /// fallback）退出时都 kill + wait 回收，否则孤儿孙进程握着 stdout 管道，
    /// cargo test 结束后不退出，exec 拿不到回传。
    struct ServeGuard {
        child: Option<Child>,
        endpoint: String,
        token: String,
    }

    impl Drop for ServeGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn spawn_serve() -> ServeGuard {
        let exe = serve_bin_path();
        assert!(exe.exists(), "serve binary missing at {exe:?}");
        let token = "backend-test-token";
        let mut child = Command::new(exe)
            .args(["serve", "--port", "0", "--token", token])
            // serve_main 中 QAQH_WORKSPACE_TOKEN 环境变量优先于 --token；
            // 测试进程可能继承 daemon 注入的变量，必须清除以保证 token 匹配。
            .env_remove("QAQH_WORKSPACE_TOKEN")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn qaqh-workspace serve");
        let stdout = child.stdout.take().expect("serve stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        for _ in 0..100 {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if let Some(bound) = line.strip_prefix("QAQH_WORKSPACE_READY ") {
                return ServeGuard {
                    child: Some(child),
                    endpoint: format!("http://{}", bound.trim()),
                    token: token.into(),
                };
            }
        }
        // 启动失败：主动回收再 panic
        drop(child);
        panic!("serve did not publish READY; last line: {line:?}");
    }

    fn make_ctx(name: &str, args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            id: "backend-test".into(),
            name: name.into(),
            action: String::new(),
            args,
            tx_progress: None,
            timeout_secs: Some(30),
            cancel: Arc::new(AtomicBool::new(false)),
            skill_effects: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn tool_timeout_prefers_model_arg_over_framework_default() {
        // exec 的模型参数 timeout_secs=600 → HTTP 读超时必须按它放大，
        // 否则 60s 就被代理层放弃（触发本地重跑 = 命令副作用重复）。
        let ctx = make_ctx("exec", serde_json::json!({ "timeout_secs": 600 }));
        assert_eq!(tool_timeout_from_args(&ctx), 600);

        // 未传模型参数 → 回退框架注入的 ctx.timeout_secs（exec default 30）
        let ctx = make_ctx("exec", serde_json::json!({ "argv": ["cmd", "/c", "dir"] }));
        assert_eq!(tool_timeout_from_args(&ctx), 30);

        // 非法值（0 / 超大）→ 回退框架值
        let ctx = make_ctx("exec", serde_json::json!({ "timeout_secs": 0 }));
        assert_eq!(tool_timeout_from_args(&ctx), 30);
        let ctx = make_ctx("exec", serde_json::json!({ "timeout_secs": 99999 }));
        assert_eq!(tool_timeout_from_args(&ctx), 30);

        // HTTP 层放大（+30s 网络余量，封顶 3600）
        let ctx = make_ctx("exec", serde_json::json!({ "timeout_secs": 600 }));
        let http_timeout = tool_timeout_from_args(&ctx).saturating_add(30).min(3600);
        assert_eq!(http_timeout, 630);
    }

    fn unreachable_local(_: ToolCallCtx) -> ToolResult {
        unreachable!("remote execution must succeed")
    }

    #[test]
    #[cfg(windows)] // Windows 盘符/cmd 语义；Linux 无对应环境
    fn process_kill_preempts_long_running_task_in_serve() {
        // 用户场景：长任务（如 cargo test 跑数分钟）期间必须能 kill 抢占。
        // 修复前：process kill 走串行 worker，排队到长任务结束（数分钟）。
        // 修复后：process 内联执行，kill 立即生效。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().to_str().unwrap().to_string();
        crate::set_workspace(&ws);

        let guard = spawn_serve();
        let backend = HttpToolExecutionBackend::new(guard.endpoint.clone(), guard.token.clone());
        let req = |ctx: ToolCallCtx| BackendRequest {
            session_id: "backend-test-session".into(),
            host_workspace: std::path::PathBuf::from(&ws),
            authorized_resources: vec![],
            local_handler: unreachable_local,
            ctx,
        };

        // 1) 长任务：ping 60 秒，background_after_secs=1 → 1 秒后移交后台
        let exec_result = backend.execute(req(make_ctx(
            "exec",
            serde_json::json!({
                "argv": ["cmd", "/C", "ping -n 60 127.0.0.1 >NUL"],
                "timeout_secs": 120,
                "background_after_secs": 1,
            }),
        )));
        let exec_text = exec_result.model_text();
        let parsed: serde_json::Value =
            serde_json::from_str(&exec_text).unwrap_or_else(|_| serde_json::json!({}));
        assert_eq!(
            parsed["status"], "backgrounded",
            "exec 应转后台: {exec_text}"
        );
        let pid = parsed["process_id"].as_u64().expect("process_id in result");

        // 2) process kill 必须立即生效（不被串行队列阻塞到 60s）
        let started = std::time::Instant::now();
        let kill_result = backend.execute(req(make_ctx(
            "process",
            serde_json::json!({ "action": "kill", "id": pid }),
        )));
        let kill_elapsed = started.elapsed().as_secs_f64();
        assert!(
            kill_elapsed < 10.0,
            "kill 必须抢占（<10s），实际 {kill_elapsed:.1}s——若走串行队列会阻塞到长任务结束"
        );
        let kill_text = kill_result.model_text();
        assert!(kill_text.contains("killed"), "kill 应成功: {kill_text}");

        // 3) 抢占后 serve 仍健康（串行 worker 未被 kill 阻塞）
        let probe = backend.execute(req(make_ctx(
            "todo",
            serde_json::json!({ "action": "list" }),
        )));
        assert!(
            probe.error.is_none(),
            "serve 应保持可用: {}",
            probe.model_text()
        );
    }
    #[test]
    fn http_backend_executes_write_in_serve_and_syncs_ledger() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().to_str().unwrap().to_string();
        crate::set_workspace(&ws);

        let guard = spawn_serve();
        let backend = HttpToolExecutionBackend::new(guard.endpoint.clone(), guard.token.clone());

        let request = BackendRequest {
            session_id: "backend-test-session".into(),
            host_workspace: std::path::PathBuf::from(&ws),
            authorized_resources: vec![],
            local_handler: unreachable_local,
            ctx: make_ctx(
                "write",
                serde_json::json!({ "path": "a.txt", "content": "hello\nworld\n" }),
            ),
        };
        let result = backend.execute(request);
        assert!(
            result.error.is_none(),
            "write via serve failed: {}",
            result.model_text()
        );
        assert!(
            result.model_text().contains("a.txt"),
            "summary should mention the written file: {}",
            result.model_text()
        );

        // 1) 文件由 serve 进程真实写入
        let content = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
        assert_eq!(content.replace("\r\n", "\n"), "hello\nworld\n");

        // 2) state_delta 已回传：daemon 侧账本持有 serve 端记录的 hash
        let abs = tmp.path().join("a.txt").to_string_lossy().to_string();
        let h = crate::file_state::last_hash(&abs).expect("ledger synced from serve");
        assert_eq!(
            h,
            crate::file_shared::content_hash(
                &crate::file_shared::normalize_newlines("hello\nworld\n").0
            )
        );

        // 3) 执行链验证：全程未走 local_handler（unreachable 未触发）；
        //    guard Drop 时回收 serve 子进程
    }
}
