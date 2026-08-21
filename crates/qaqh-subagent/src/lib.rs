//! qaqh-subagent — spawn sub-agent tool for the QAQ-Harness agent (Ringing V1).
//!
//! The subagent is an **isolated Ringing session**, not a raw child process:
//!
//! 1. `spawn_subagent` runs the subagent as an in-process actor on a daemon
//!    thread. When the daemon installs an in-process [`SubagentHost`]
//!    (Knife-1 step-2: `QaqhService` via `qaqh_subagent::install_host`), the
//!    tool drives the actor directly through the host handle — no daemon
//!    HTTP/SSE loopback. Without a host (tests / non-daemon embedding) it
//!    falls back to the legacy `subagent.spawn` action over HTTP/SSE.
//! 2. The parent attaches (or directly addresses) the sub-seed and sends the
//!    task via the ordinary `ConversationSendMessage` Ringing command.
//! 3. A background collector thread watches the event stream for
//!    `TurnCompleted` / `TurnFailed` / `ConversationCancelled` and records the
//!    final answer into the shared [`ProcessRegistry`] — the existing
//!    `process check|wait|kill` tools then work unchanged.
//!
//! Supports model override (different model/provider per subagent), context
//! sharing, per-instance naming, and timeout/cancel semantics.
//!
//! ## Registration
//!
//! Call `qaqh_subagent::register(&mut tool_manager)` during agent
//! initialization (the subagent worker itself does this via
//! `AgentState::init_subagent`) to register the `spawn_subagent` tool.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qaqh_client::{
    ActionRequest, Client, ClientError, ClientHandlers, ClientOptions, CommandOptions,
    ControlEvent, ConversationCommand, ConversationEvent, RingingCommand, RingingCommandAck,
    RingingCommandAckStatus, RingingEvent,
};
// `ContentRef` / `EventBatch`（trait 签名 + transport 事件流）经下方
// `pub use host::{ContentRef, EventBatch, ..}` 引入。
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

mod host;
pub use host::{host, install_host, ContentRef, EventBatch, SubagentHost};

/// Run a future on the shared qaqh-client tokio runtime. Safe from any
/// non-tokio thread (tool handlers and collector threads are std threads).
fn rt_block_on<F: std::future::Future>(fut: F) -> F::Output {
    qaqh_client::runtime_handle().block_on(fut)
}

/// 子代理固定身份提示：注入到子代理任务文本的 `[SYSTEM]` 段。
/// 子代理的 base system prompt（`backend_prompt.md`）与主代理同源（同 config
/// 加载），前缀天然一致、可命中 provider 前缀缓存；本段补充子代理专属身份约束。
const SUBAGENT_IDENTITY_PROMPT: &str = "\
你现在是工作于QAQ-Harness的子代理工程师，你被要求严格执行主coding agents的一切要求，\
不得擅自违背未经允许的操作，并且忠实地把主代理的任务精准完成。";

pub fn register(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "spawn_subagent".to_string(),
        description: "Spawn a sub-agent to handle a focused task independently. \
            The subagent runs as an isolated Ringing session with its own context \
            and the parent's tool set (restricted by user settings). Returns a \
            process_id immediately for optional kill/check tracking; the final \
            answer is automatically injected into your conversation as a \
            [SUBAGENT ...] system message when the subagent finishes - do NOT \
            poll with `process wait`, just continue after the injection arrives. \
            Use for complex multi-step sub-tasks that benefit from isolation. \
            `agent_name` should be a verb+task phrase (e.g. 'explore_task').",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_description": {"type": "string", "description": "Short description of the task for the subagent."},
                "agent_name": {"type": "string", "description": "Name for this subagent, verb+task phrase (e.g. 'explore_task', 'review_code')."},
                "context": {"type": "string", "description": "Optional background context to hand to the subagent before the task."},
                "timeout_secs": {"type": "integer", "description": "Maximum time in seconds before the subagent is cancelled. Default 120."}
            },
            "required": ["task_description"],
            "additionalProperties": false
        }),
        handler: handle_spawn_subagent,
        risk: ToolRisk::Administrative,
        category: qaqh_workspace::permission::ToolCategory::Exec,
        default_timeout: std::time::Duration::from_secs(180),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subagent_schema_never_accepts_api_keys() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        assert!(!properties.contains_key("api_key"));
        assert!(!handler.input_schema.to_string().contains("--api-key"));
    }

    #[test]
    fn spawn_subagent_schema_exposes_only_the_llm_facing_params() {
        let mut manager = ToolManager::new();
        register(&mut manager);
        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        // 模型面只有 4 个参数；system_prompt / tools / model / base_url /
        // max_tokens 不再暴露（由设置页配置或内置身份提示提供）。
        let expected: Vec<&str> = vec!["task_description", "agent_name", "context", "timeout_secs"];
        assert_eq!(properties.len(), expected.len());
        for key in expected {
            assert!(properties.contains_key(key), "missing {key}");
        }
        for legacy in [
            "system_prompt",
            "tools",
            "model",
            "base_url",
            "max_tokens",
            "name",
            "task",
        ] {
            assert!(
                !properties.contains_key(legacy),
                "{legacy} must not be exposed"
            );
        }
        assert_eq!(
            handler.input_schema["required"][0], "task_description",
            "task_description must be the only required param"
        );
    }

    #[test]
    fn task_builder_injects_identity_and_wraps_context() {
        // 无 context：身份提示 + 任务，无 context 段。
        let bare = build_subagent_task("do the thing", "");
        assert!(bare.contains("[SYSTEM]"));
        assert!(bare.contains(SUBAGENT_IDENTITY_PROMPT));
        assert!(bare.contains("[TASK]\ndo the thing"));
        assert!(!bare.contains("[CONTEXT]"));

        // 有 context：必须包裹在 <main_subagent_message> 内，防止子代理把
        // 传入内容误判为自身 user 消息而直接修改项目。
        let with_ctx = build_subagent_task("review the diff", "repo at F:\\proj\nbranch main");
        assert!(with_ctx.contains(
            "<main_subagent_message>\nrepo at F:\\proj\nbranch main\n</main_subagent_message>"
        ));
        assert!(with_ctx.contains("[TASK]\nreview the diff"));
        // 身份提示在前，任务在后。
        assert!(with_ctx.find("[SYSTEM]").unwrap() < with_ctx.find("[TASK]").unwrap());
    }
}

/// 构造子代理任务文本：固定身份提示（`[SYSTEM]`）+ 显式包裹的上下文
/// （`<main_subagent_message>`，防止子代理把传入内容当作自己的 user 消息
/// 而直接动项目）+ 任务（`[TASK]`）。
fn build_subagent_task(task_description: &str, context: &str) -> String {
    let mut parts = vec![format!("[SYSTEM]\n{SUBAGENT_IDENTITY_PROMPT}")];
    if !context.trim().is_empty() {
        parts.push(format!(
            "[CONTEXT]\n<main_subagent_message>\n{}\n</main_subagent_message>",
            context.trim()
        ));
    }
    parts.push(format!("[TASK]\n{}", task_description.trim()));
    parts.join("\n\n")
}

// ── serve 端子代理注册表（ProcessRegistry 权威进程）─────────────────
//
// ProcessRegistry 是进程内单例：exec 后台进程与 process 工具（Workspace
// placement）在 workspace serve 进程执行，而 spawn_subagent（HostOnly）在
// 主代理 worker 进程执行——此前子代理记录注册在 worker 进程，主代理的
// `process check/wait/kill` 却在 serve 进程查询，永远 NOT_FOUND（甚至因
// 两个进程 ID 都从 1 编号而误查/误杀同号的 exec 后台进程）。
//
// 修复：子代理记录注册到 serve 进程（`POST /subagent`），与 process 工具
// 的执行进程一致；collect 线程轮询 serve 检测 kill、收尾时回写终态。
// serve 不可达时回退到本地注册表（退化到旧行为，仅影响进程可见性）。

/// 子代理进程记录的实际存放位置。
enum RegistryRef {
    /// serve 进程注册表（process 工具可见）。
    Remote {
        client: ServeRegistryClient,
        id: u32,
    },
    /// 本地（worker 进程）注册表——serve 不可达时的降级路径。
    Local { id: u32 },
}

impl RegistryRef {
    fn id(&self) -> u32 {
        match self {
            RegistryRef::Remote { id, .. } | RegistryRef::Local { id } => *id,
        }
    }

    /// 是否已被 `process kill` 标记为 killed。
    fn killed(&self) -> bool {
        match self {
            RegistryRef::Remote { client, id } => client
                .post("status", *id, "", "", 0)
                .and_then(|v| v.get("info").cloned())
                .and_then(|info| {
                    info.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "killed")
                })
                .unwrap_or(false),
            RegistryRef::Local { id } => {
                qaqh_workspace::process_registry::ProcessRegistry::get_info(*id)
                    .and_then(|info| {
                        info.get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "killed")
                    })
                    .unwrap_or(false)
            }
        }
    }

    /// 收尾：写入最终作答与退出码。
    fn finish(&self, answer: &str, exit_code: i32) {
        match self {
            RegistryRef::Remote { client, id } => {
                let _ = client.post("update", *id, "", answer, exit_code);
            }
            RegistryRef::Local { id } => {
                qaqh_workspace::process_registry::ProcessRegistry::set_answer(
                    *id,
                    answer.to_string(),
                );
                qaqh_workspace::process_registry::ProcessRegistry::mark_exited(*id, exit_code);
            }
        }
    }
}

/// workspace serve 的 `/subagent` 端点访问器（worker 环境变量注入端点）。
#[derive(Clone)]
struct ServeRegistryClient {
    endpoint: String,
    token: String,
}

impl ServeRegistryClient {
    /// 从 daemon 注入的 worker 环境发现 serve 端点。
    fn discover() -> Option<Self> {
        let endpoint = std::env::var("QAQH_WORKSPACE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())?;
        let token = std::env::var("QAQH_WORKSPACE_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())?;
        Some(Self { endpoint, token })
    }

    fn post(
        &self,
        action: &str,
        id: u32,
        name: &str,
        answer: &str,
        exit_code: i32,
    ) -> Option<serde_json::Value> {
        let url = format!("{}/subagent", self.endpoint.trim_end_matches('/'));
        let body = serde_json::json!({
            "action": action,
            "id": id,
            "name": name,
            "answer": answer,
            "exit_code": exit_code,
        });
        let result = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(5)))
            .timeout_per_call(Some(std::time::Duration::from_secs(10)))
            .build()
            .new_agent()
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send_json(body);
        match result {
            Ok(mut response) => response.body_mut().read_json::<serde_json::Value>().ok(),
            Err(error) => {
                log::warn!("[SUBAGENT] serve /subagent {action} failed: {error}");
                None
            }
        }
    }
}

/// 注册子代理进程记录：优先 serve 进程（process 工具可见），失败回退本地。
fn register_subagent_process(name: &str) -> RegistryRef {
    if let Some(client) = ServeRegistryClient::discover() {
        match client
            .post("register", 0, name, "", 0)
            .and_then(|v| v.get("id").and_then(|x| x.as_u64()))
        {
            Some(id) => {
                log::info!("[SUBAGENT] '{name}' registered in serve registry id={id}");
                return RegistryRef::Remote {
                    client,
                    id: id as u32,
                };
            }
            None => {
                log::warn!(
                    "[SUBAGENT] '{name}' serve register failed; falling back to local registry (process tools will NOT see it)"
                );
            }
        }
    }
    let id = qaqh_workspace::process_registry::ProcessRegistry::register(name);
    log::info!("[SUBAGENT] '{name}' registered in local registry id={id}");
    RegistryRef::Local { id }
}

/// 命令 ACK 是否被 daemon 接受（Accepted）。
fn ack_accepted(result: &Result<RingingCommandAck, ClientError>) -> bool {
    matches!(result, Ok(ack) if matches!(ack.status, RingingCommandAckStatus::Accepted))
}

/// 子代理命令/事件传输抽象：宿主直连与 HTTP/SSE 回连共用同一套 collect 流程。
///
/// - [`HostTransport`]：进程内直连（无 lease / 无 HTTP），由 daemon 装配宿主；
/// - [`HttpTransport`]：旧 daemon HTTP/SSE 回连（宿主不可用时的降级路径）。
trait SubagentTransport: Send {
    /// 向某 seed 发送命令。返回是否被 accepted。
    fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String>;
    /// 读取外置大内容。
    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String>;
    /// 建立 attachment / lease（HTTP 路径需要；宿主直连为 no-op）。
    fn attach(&self, seed: &str) -> Result<(), String>;
    /// 关闭客户端连接（宿主直连为 no-op）。
    fn close(&self);
    /// 该 seed 的实时事件批次流。
    fn events(&self) -> &mpsc::Receiver<EventBatch>;
}

/// 宿主直连传输：直接调用进程内宿主（ActorRegistry + RingingHub）。
struct HostTransport {
    host: Arc<dyn SubagentHost>,
    batch_rx: mpsc::Receiver<EventBatch>,
}

impl SubagentTransport for HostTransport {
    fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String> {
        // SessionClose 由 daemon registry 拦截处理（loop_core 会忽略该命令），
        // 进程内宿主直接执行 close（registry + 临时会话清理），语义一致。
        if matches!(
            &command,
            RingingCommand::Control(qaqh_client::ControlCommand::SessionClose { .. })
        ) {
            self.host.close(seed)?;
            return Ok(true);
        }
        self.host.send_ringing(seed, command)?;
        Ok(true)
    }

    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String> {
        self.host.download_content(seed, reference)
    }

    fn attach(&self, _seed: &str) -> Result<(), String> {
        Ok(())
    }

    fn close(&self) {}

    fn events(&self) -> &mpsc::Receiver<EventBatch> {
        &self.batch_rx
    }
}

/// HTTP/SSE 回连传输（降级路径）：包装 `qaqh_client::Client`。
struct HttpTransport {
    client: Client,
    batch_rx: mpsc::Receiver<EventBatch>,
}

impl SubagentTransport for HttpTransport {
    fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String> {
        let result = rt_block_on(self.client.send_command(
            Some(seed),
            command,
            CommandOptions::default(),
        ));
        Ok(ack_accepted(&result))
    }

    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String> {
        rt_block_on(self.client.download_content(seed, reference)).map_err(|e| e.to_string())
    }

    fn attach(&self, seed: &str) -> Result<(), String> {
        let result = rt_block_on(self.client.attach(seed));
        if ack_accepted(&result) {
            Ok(())
        } else {
            Err(format!("{:?}", result.map(|a| (a.status, a.code))))
        }
    }

    fn close(&self) {
        self.client.close();
    }

    fn events(&self) -> &mpsc::Receiver<EventBatch> {
        &self.batch_rx
    }
}

fn handle_spawn_subagent(ctx: ToolCallCtx) -> ToolResult {
    let name: String = ctx
        .args
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "sub".to_string());
    let task: String = ctx
        .args
        .get("task_description")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let context: String = ctx
        .args
        .get("context")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    // 模型面只暴露 4 个参数；工具白名单 / 模型 / base-url / max-tokens /
    // 超时默认值一律取自用户设置（cfg.subagent.*，前端设置页可调），
    // 空值=继承主代理。
    let (tools, model_override, base_url_override, max_tokens, cfg_timeout) =
        qaqh_config::Config::load()
            .ok()
            .map(|cfg| {
                (
                    cfg.subagent.default_tools.clone(),
                    cfg.subagent.model.clone(),
                    cfg.subagent.base_url.clone(),
                    cfg.subagent.max_tokens,
                    cfg.subagent.timeout_secs,
                )
            })
            .unwrap_or_default();
    let timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(cfg_timeout.max(1))
        .clamp(1, 3600);

    if task.trim().is_empty() {
        return ToolResult::error(qaqh_workspace::json_err(
            "MISSING_TASK",
            "spawn_subagent: task_description is required",
            "Provide a task description.",
        ));
    }
    let task_text = build_subagent_task(&task, &context);

    // 子代理继承主代理的工作区：CURRENT_WORKSPACE 为空/`.` 时不传，宿主侧
    // 同样跳过继承（子 actor 退化为 daemon cwd）。
    let parent_workspace = qaqh_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let workspace = if parent_workspace.is_empty() || parent_workspace == "." {
        None
    } else {
        Some(parent_workspace)
    };
    let model = if model_override.is_empty() {
        None
    } else {
        Some(model_override.as_str())
    };
    let base_url = if base_url_override.is_empty() {
        None
    } else {
        Some(base_url_override.as_str())
    };
    let max_tokens_opt = if max_tokens == 0 || max_tokens == 4096 {
        None
    } else {
        Some(max_tokens)
    };

    // ── 1. 选择传输：宿主直连（进程内，无 HTTP/SSE 回环）优先；双子代理
    //        actor 未安装宿主时回退旧 daemon HTTP/SSE 路径。两种方式产出
    //        `(seed, Box<dyn SubagentTransport>)`，后续流程共用。──
    let (seed, transport): (String, Box<dyn SubagentTransport>) = if let Some(host) = host() {
        log::info!(
            "[SUBAGENT] '{name}' using in-process host direct transport (tools={})",
            tools.len()
        );
        let seed = match host.spawn_subagent(
            &tools,
            model,
            base_url,
            max_tokens_opt,
            workspace.as_deref(),
        ) {
            Ok(seed) if !seed.is_empty() => seed,
            Ok(_) => {
                return ToolResult::error(qaqh_workspace::json_err(
                    "SPAWN_ERROR",
                    "spawn_subagent: host returned empty seed",
                    "Check host/daemon logs.",
                ));
            }
            Err(e) => {
                return ToolResult::error(qaqh_workspace::json_err(
                    "SPAWN_ERROR",
                    &format!("spawn_subagent: host rejected spawn: {e}"),
                    "Check that the daemon can start subagent actors.",
                ));
            }
        };
        let batch_rx = host.subscribe(&seed);
        (
            seed,
            Box::new(HostTransport { host, batch_rx }) as Box<dyn SubagentTransport>,
        )
    } else {
        log::warn!(
            "[SUBAGENT] '{name}' no in-process host — falling back to daemon HTTP/SSE loopback"
        );
        let (seed, client, batch_rx) =
            match spawn_via_http(&tools, model, base_url, max_tokens_opt, workspace) {
                Ok(spawned) => spawned,
                Err(err) => return err,
            };
        (
            seed,
            Box::new(HttpTransport { client, batch_rx }) as Box<dyn SubagentTransport>,
        )
    };
    log::info!("[SUBAGENT] '{name}' worker seed={seed}");

    // ── 2. Send the task (attach/lease 语义封装在 transport 内；宿主直连
    //        进程内直接入 actor 命令队列，无 lease)。──
    let send = RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: task_text,
        images: vec![],
        attachments: None,
        as_system: false,
    });
    // 任务发送校验：Rejected/Err 意味着子 actor 未收到任务，立即失败，不要
    // 让 collect 空等 timeout。
    let send_accepted = match transport.send_command(&seed, send) {
        Ok(accepted) if accepted => true,
        Ok(_) => {
            transport.close();
            return ToolResult::error(qaqh_workspace::json_err(
                "SEND_REJECTED",
                "spawn_subagent: daemon rejected task send",
                "Check daemon/worker logs for lease or state conflicts.",
            ));
        }
        Err(e) => {
            transport.close();
            return ToolResult::error(qaqh_workspace::json_err(
                "SEND_ERROR",
                &format!("spawn_subagent: send task: {e}"),
                "Check daemon/worker logs.",
            ));
        }
    };
    let _ = send_accepted;
    log::info!("[SUBAGENT] '{name}' task delivered to {seed} (accepted)");

    // ── 3. Register the process and collect the result in the background. ──
    //
    // 记录注册到 serve 进程（ProcessRegistry 权威进程，process 工具同进程），
    // 主代理的 `process check/wait/kill` 才能看到子代理；serve 不可达时回退
    // 本地注册表。最终结果仍经 Ringing 注入主代理会话回传。
    let registry_ref = register_subagent_process(&format!("subagent:{name}"));
    let registry_id = registry_ref.id();
    // 主代理会话 seed：collect 完成后把最终作答注入回主会话（模型下一轮自然看到）。
    let parent_seed = qaqh_workspace::runtime::context()
        .map(|ctx| ctx.active_session.clone())
        .unwrap_or_default();
    let name_bg = name.clone();
    let seed_bg = seed.clone();
    std::thread::spawn(move || {
        collect_subagent_result(
            transport,
            &seed_bg,
            &name_bg,
            registry_ref,
            timeout_secs,
            &parent_seed,
        );
    });

    log::info!("[SUBAGENT] '{name}' spawned (seed={seed}, process={registry_id})");
    ToolResult::ok(qaqh_workspace::json_ok(serde_json::json!({
        "process_id": registry_id,
        "seed": seed,
        "name": name,
        "content": format!("Subagent '{name}' spawned (process {registry_id}); the final answer will be injected into the conversation as a [SUBAGENT] system message when it completes."),
    })))
}

/// HTTP/SSE 回退路径：连 daemon → `subagent.spawn` action → 返回
/// `(seed, client, batch_rx)`。宿主不可用（非 daemon 进程 / 单元测试）时走此。
fn spawn_via_http(
    tools: &[String],
    model: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
    workspace: Option<String>,
) -> Result<(String, Client, mpsc::Receiver<EventBatch>), ToolResult> {
    let (batch_tx, batch_rx) = mpsc::channel::<EventBatch>();
    let handlers = ClientHandlers {
        on_batch: Arc::new(move |batch| {
            let _ = batch_tx.send(batch);
        }),
        ..Default::default()
    };
    let client = match Client::connect(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        daemon_path: None,
        start_timeout: Duration::from_secs(8),
        remote: None,
    }) {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolResult::error(qaqh_workspace::json_err(
                "DAEMON_CONNECT",
                &format!("spawn_subagent: cannot connect to daemon: {e}"),
                "Ensure the QAQ-Harness daemon is running.",
            )));
        }
    };
    let request = ActionRequest::SubagentSpawn {
        tools: tools.to_vec(),
        model: model.map(str::to_string),
        base_url: base_url.map(str::to_string),
        max_tokens,
        workspace,
    };
    let seed: String = match rt_block_on(client.action(request)) {
        Ok(value) => match value.get("seed").and_then(|v| v.as_str()) {
            Some(seed) if !seed.is_empty() => seed.to_string(),
            _ => {
                client.close();
                return Err(ToolResult::error(qaqh_workspace::json_err(
                    "SPAWN_ERROR",
                    "spawn_subagent: daemon returned no seed",
                    "Check daemon logs.",
                )));
            }
        },
        Err(e) => {
            client.close();
            return Err(ToolResult::error(qaqh_workspace::json_err(
                "SPAWN_ERROR",
                &format!("spawn_subagent: daemon rejected spawn: {e}"),
                "Check that the daemon can start agent workers.",
            )));
        }
    };
    // attach the sub-seed (HTTP/lease 语义)。
    if let Err(e) = rt_block_on(client.attach(&seed)) {
        client.close();
        return Err(ToolResult::error(qaqh_workspace::json_err(
            "ATTACH_ERROR",
            &format!("spawn_subagent: attach {seed}: {e}"),
            "Check daemon lease state.",
        )));
    }
    Ok((seed, client, batch_rx))
}

/// Background collector: watches the sub-seed's event stream (process-local or
/// HTTP/SSE, depending on the transport) until a terminal event, a kill
/// request, or the timeout — mirroring the old stdout-frame collector, but over
/// the Ringing event plane.
fn collect_subagent_result(
    transport: Box<dyn SubagentTransport>,
    seed: &str,
    name: &str,
    registry_ref: RegistryRef,
    timeout_secs: u64,
    parent_seed: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut final_answer = String::new();
    let mut exit_code: i32 = 0;
    let mut did_finish = false;
    let mut did_cancel = false;
    // 诊断：是否收到过子 seed 的任意事件（用于区分"子代理一开始就死了"
    // 与"中途卡死"——worker 侧 [SUBAGENT-WORKER] 日志 + 落盘开关配合）。
    let mut first_event_logged = false;

    while !did_finish && !did_cancel {
        // Kill requested (process kill {id}) → cancel the sub turn.
        // 状态权威在 serve 进程（Remote），或本地注册表（Local 降级）。
        if registry_ref.killed() {
            log::info!("[SUBAGENT] '{name}' kill requested via process registry — cancelling");
            let cancel = RingingCommand::Conversation(
                qaqh_client::ConversationCommand::ConversationCancel { turn_id: None },
            );
            if let Err(e) = transport.send_command(seed, cancel) {
                log::warn!("[SUBAGENT] '{name}' cancel send failed: {e}");
            }
            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
            did_cancel = true;
            break;
        }
        match transport.events().recv_timeout(Duration::from_millis(300)) {
            Ok(batch) => {
                if batch.seed != seed {
                    continue;
                }
                if !first_event_logged && !batch.envelopes.is_empty() {
                    first_event_logged = true;
                    log::info!(
                        "[SUBAGENT] '{name}' first event received ({} envelopes, stream_seq {})",
                        batch.envelopes.len(),
                        batch.from_stream_seq
                    );
                }
                for envelope in batch.envelopes {
                    match envelope.event {
                        RingingEvent::Conversation(ConversationEvent::RoundCompleted {
                            answer,
                            output_ref,
                            is_final,
                            ..
                        }) => {
                            // Prefer the authoritative full answer; fall back to
                            // externalized content when the body is large.
                            if let Some(answer) = answer {
                                if !answer.is_empty() {
                                    final_answer = answer;
                                }
                            } else if let Some(reference) = output_ref {
                                if let Ok(bytes) = transport.download_content(seed, &reference) {
                                    final_answer = String::from_utf8_lossy(&bytes).to_string();
                                }
                            }
                            if is_final && !final_answer.is_empty() {
                                did_finish = true;
                            }
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. }) => {
                            log::info!("[SUBAGENT] '{name}' turn completed");
                            did_finish = true;
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnFailed {
                            error, ..
                        }) => {
                            log::warn!("[SUBAGENT] '{name}' turn failed: {error:?}");
                            final_answer = format!("[SUBAGENT '{name}' ERROR] {error:?}");
                            exit_code = 1;
                            did_finish = true;
                        }
                        RingingEvent::Conversation(ConversationEvent::ConversationCancelled {
                            ..
                        }) => {
                            log::info!("[SUBAGENT] '{name}' conversation cancelled");
                            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
                            did_cancel = true;
                        }
                        // 控制面失败（compact 拒绝注入、lease 拒绝等）：此前被
                        // 静默忽略导致"等到超时"。至少记入日志便于归因。
                        RingingEvent::Control(ControlEvent::OperationFailed { error, .. }) => {
                            log::warn!(
                                "[SUBAGENT] '{name}' operation failed: code={:?} message={:?}",
                                error.code,
                                error.message
                            );
                        }
                        _ => {}
                    }
                    if did_finish || did_cancel {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    log::warn!(
                        "[SUBAGENT] '{name}' timeout after {timeout_secs}s (first_event={first_event_logged}) — cancelling sub turn"
                    );
                    let cancel = RingingCommand::Conversation(
                        qaqh_client::ConversationCommand::ConversationCancel { turn_id: None },
                    );
                    if let Err(e) = transport.send_command(seed, cancel) {
                        log::warn!("[SUBAGENT] '{name}' timeout cancel send failed: {e}");
                    }
                    final_answer = format!("[SUBAGENT '{name}' TIMEOUT after {timeout_secs}s]");
                    exit_code = 1;
                    did_finish = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Event stream closed before a terminal event (daemon gone?).
                log::warn!(
                    "[SUBAGENT] '{name}' event stream closed, partial answer_len={}",
                    final_answer.len()
                );
                did_finish = true;
            }
        }
    }

    let answer_len = final_answer.len();

    // ── 结果回传：注入主代理会话。 ──
    // 主代理 idle 时这条消息触发新回合（模型自动看到子代理结果并继续）；
    // 主代理仍在运行中则进入回合 lap 边界的见缝插针通道。注入被 daemon 拒绝
    // （lease/compact 等）时重试一次并告警，避免静默丢失。
    let header = if did_cancel {
        format!("[SUBAGENT '{name}' CANCELLED]")
    } else if exit_code != 0 {
        format!("[SUBAGENT '{name}' ERROR exit={exit_code}]")
    } else {
        format!("[SUBAGENT '{name}' COMPLETED]")
    };
    if !parent_seed.is_empty() {
        // 注入到主代理会话。主代理 idle 时该消息触发新回合；运行中则进入
        // cmd_rx 排队 / lap 边界见缝插针通道。daemon 的 Accepted ACK 只代表
        // "已转发"，不代表 worker 落地；worker 侧的 compact 拒绝已改为延迟处理
        // （loop_core deferral），此处再对转发级瞬时失败（daemon 写 stdin 阻塞 /
        // lease 波动 / 网络抖动）做带退避重试，避免一次失败就静默丢弃子代理结果。
        //
        // 401 lease_required 根因：collect 的 client 只 attach 了子 seed，向主
        // seed 发命令需先建立 owns 关系（SessionResume）。每次重试前重新 attach
        // （SessionResume 幂等），覆盖 lease 过期后的恢复。
        let inject = RingingCommand::Conversation(
            qaqh_client::ConversationCommand::ConversationSendMessage {
                text: format!("{header}\n\n{final_answer}"),
                images: vec![],
                attachments: None,
                // 以 system 角色注入（而非 user）：模型可见但不等同于用户输入，
                // 保留 [SUBAGENT ...] 标签供模型区分注入数据与系统指令。
                as_system: true,
            },
        );
        let mut accepted = false;
        let mut last_rejected: Option<String> = None;
        const INJECT_ATTEMPTS: usize = 5;
        for attempt in 0..INJECT_ATTEMPTS {
            if attempt > 0 {
                // 线性退避：300ms → 600ms → 1200ms → 2400ms
                std::thread::sleep(std::time::Duration::from_millis(
                    300 * (1 << attempt.min(3)),
                ));
            }
            // attach（HTTP/lease 语义；宿主直连为 no-op，覆盖 lease 过期后的恢复）。
            if let Err(e) = transport.attach(parent_seed) {
                log::warn!(
                    "[SUBAGENT] '{name}' attach parent {parent_seed} for inject (attempt {}): {e}",
                    attempt + 1
                );
            }
            match transport.send_command(parent_seed, inject.clone()) {
                Ok(true) => {
                    accepted = true;
                    break;
                }
                other => {
                    last_rejected = Some(format!("{other:?}"));
                    log::warn!(
                        "[SUBAGENT] '{name}' inject attempt {} not accepted: {:?}",
                        attempt + 1,
                        last_rejected
                    );
                }
            }
        }
        if accepted {
            log::info!("[SUBAGENT] '{name}' inject accepted ({} bytes)", answer_len);
        } else {
            log::error!(
                "[SUBAGENT] '{name}' inject FAILED after {INJECT_ATTEMPTS} attempts: {:?}",
                last_rejected
            );
        }
    }

    registry_ref.finish(&final_answer, exit_code);

    // ── 自动卸载：终态后关闭子 agent（actor / worker 进程），释放后台资源。──
    // SessionClose 语义由宿主执行（进程内 registry.close；HTTP 路径由 daemon
    // 拦截：registry.close → SessionShutdown 帧 → worker 优雅退出）。
    // 失败仅告警：结果已注入主会话 + 终态已回写注册表，残留不丢数据。
    if let Err(e) = transport.send_command(
        seed,
        RingingCommand::Control(qaqh_client::ControlCommand::SessionClose {
            seed: seed.to_string(),
        }),
    ) {
        log::warn!("[SUBAGENT] '{name}' close worker {seed} failed: {e}");
    } else {
        log::info!("[SUBAGENT] '{name}' sub agent {seed} closed (auto-unload)");
    }

    transport.close();
    log::info!(
        "[SUBAGENT] '{name}' collector complete (seed={seed}), answer_len={answer_len}, exit={exit_code}, cancelled={did_cancel}, first_event={first_event_logged}"
    );
}
