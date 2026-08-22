use std::collections::HashMap;
use std::process::Command;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, OnceLock};

use crate::actor::SubagentSpawnSpec;
use crate::{RingingHub, SessionActivityTracker};

static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

pub fn cache_system_path() {
    let mut path = std::env::var("PATH").unwrap_or_default();
    #[cfg(target_os = "windows")]
    for key in [
        r"HKCU\Environment",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ] {
        let mut command = background_command("reg");
        if let Ok(output) = command.args(["query", key, "/v", "Path"]).output() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = text
                .lines()
                .find(|line| line.contains("REG_"))
                .and_then(|line| {
                    line.split_once("REG_EXPAND_SZ")
                        .or_else(|| line.split_once("REG_SZ"))
                })
                .map(|(_, value)| value.trim())
            {
                for segment in value.split(';').filter(|value| !value.is_empty()) {
                    if !path
                        .split(';')
                        .any(|current| current.eq_ignore_ascii_case(segment))
                    {
                        if !path.is_empty() {
                            path.push(';')
                        }
                        path.push_str(segment)
                    }
                }
            }
        }
    }
    let _ = SYSTEM_PATH.set(path.clone());
    unsafe {
        std::env::set_var("PATH", path);
    }
}

pub fn detect_os_info() {
    #[cfg(target_os = "windows")]
    let info = background_command("cmd")
        .args(["/d", "/c", "ver"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("windows {}", std::env::consts::ARCH));
    #[cfg(not(target_os = "windows"))]
    let info = Command::new("uname")
        .arg("-a")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    let _ = qaqh_config::prompt::OS_INFO.set(info);
    let mut tools = Vec::new();
    for (program, args) in [
        ("git", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("node", vec!["--version"]),
        ("python", vec!["--version"]),
    ] {
        if let Ok(output) = background_command(program).args(args).output() {
            let value = if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            };
            let value = String::from_utf8_lossy(value).trim().to_string();
            if !value.is_empty() {
                tools.push(value)
            }
        }
    }
    let _ = qaqh_config::prompt::TOOLS_INFO.set(tools.join(", "));
}

fn background_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How an agent worker is attached to this daemon process.
enum AgentTransport {
    /// Knife-1 in-process worker: the Ringing Loop runs on a daemon thread and
    /// communicates through the same WorkerCommand/WriterEvent channel types as
    /// the pipe boundary.
    InProcess {
        cmd_tx: SyncSender<qaqh_msgloop::ringing_v1::types::WorkerCommand>,
        cancel: qaqh_msgloop::ringing_v1::types::CancelToken,
    },
}

enum AgentKind {
    Session,
    Subagent(SubagentSpawnSpec),
}

pub struct AgentInstance {
    seed: String,
    transport: AgentTransport,
    kind: AgentKind,
    /// Event consumer thread (stdout reader for process workers, event channel
    /// reader for in-process actors). daemon 关闭时必须 join：worker 退出 ≠
    /// 尾部 intent（含 seal_turn）已消费——管道/通道里的最后几个事件仍由
    /// 本线程读取并 publish（见 shutdown）。
    reader: Option<std::thread::JoinHandle<()>>,
    /// In-process loop thread. `None` for process workers.
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct AgentRegistry {
    instances: HashMap<String, AgentInstance>,
    activity: SessionActivityTracker,
    /// Ringing 运行时；None = 未启用 legacy worker-only 模式。
    hub: Option<Arc<RingingHub>>,
    /// daemon 拉起的 workspace serve endpoint + token（注入每个 worker env）。
    workspace_env: Option<(String, String)>,
    /// workspace 运行模式（"local" / "wsl"）。工具执行后端据此决定：
    /// local 默认进程内（worker 内最短路径，serve 可退役）；仅 wsl 才经 HTTP
    /// 远程到 WSL2 serve 执行跨 OS 工具。
    workspace_mode: String,
    /// daemon 正在关闭：worker 退出是预期的，禁止自动重生。
    shutting_down: bool,
    /// 最近一次 spawn 时间（防崩溃-重启风暴：同一 seed 1 秒内不重复拉起）。
    last_spawn: HashMap<String, std::time::Instant>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            activity: SessionActivityTracker::default(),
            hub: None,
            workspace_env: None,
            workspace_mode: "local".to_string(),
            shutting_down: false,
            last_spawn: HashMap::new(),
        }
    }

    /// 注入 workspace serve 连接信息与运行模式；worker spawn 时写入其环境变量。
    /// `mode` ∈ {"local", "wsl"}：local 时 worker 仍可用 endpoint（subagent 注册表
    /// 等），但工具执行后端保持进程内；wsl 时才启用 HTTP 远程工具执行。
    pub fn attach_workspace(&mut self, endpoint: String, token: String, mode: &str) {
        self.workspace_env = Some((endpoint, token));
        self.workspace_mode = mode.to_string();
    }

    /// 挂载 Ringing 运行时。Ringing worker 事件只进入 native hub。
    pub fn attach_ringing(&mut self, hub: Arc<RingingHub>) {
        self.hub = Some(hub);
    }

    pub fn get_or_spawn(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Ok(());
        }
        self.spawn(seed, None)?;
        // Diagnostic: the timeline snapshot is a best-effort async checkpoint
        // and a daemon restart can drop its tail. When it lags the message
        // store (meta.turn_count), the resumed transcript misses turns — the
        // frontend now backfills them from the Ringing conversation store, so
        // this is informational but valuable for restart forensics.
        if let Some(hub) = self.hub.as_ref()
            && let Some(meta) = qaqh_session::SessionManager::global().load_meta(seed)
            && let Some(snapshot) = hub.timeline_snapshot(seed)
        {
            let snapshot_turns = snapshot.turns.len();
            if snapshot_turns != meta.turn_count {
                log::warn!(
                    "[timeline] snapshot turns ({snapshot_turns}) != meta.turn_count ({}) for {seed}; transcript backfills from the conversation store",
                    meta.turn_count
                );
            }
        }
        // 新 worker 诞生意味着旧 worker 已死（daemon 重启或进程退出）。
        // timeline 中该 seed 任何未 seal 的 running turn 都是孤儿（如工具
        // 调用未返回 result 时进程被杀），立即收尾为 Cancelled，否则前端
        // 会永远把它投影为 running 并禁止发送新消息。
        if let Some(hub) = self.hub.as_ref() {
            hub.seal_orphan_running_turns(seed);
            // Ringing 三频道投影的等价收尾：重放的无终态 TurnStarted/
            // ToolStarted/InteractionRequested 同样会污染 bootstrap 快照，
            // 使前端显示陈旧 running turn 与无法批准的幽灵交互面板。
            // force=true：旧 worker 已死，所有挂起状态都是孤儿，无视活交互守卫。
            hub.seal_orphan_channel_state(seed, true);
        }
        Ok(())
    }

    pub fn spawn_new(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.spawn_with(seed, Some(seed), &[])
    }

    /// Spawn an isolated subagent worker **inside the daemon process**.
    ///
    /// Knife-1 step 1 removes `Command::new(current_exe)` and the child-process
    /// leg of the subagent path.
    ///
    /// The subagent is a normal Ringing V1
    /// [`qaqh_msgloop::ringing_v1::loop_core::Loop`] running on a daemon thread;
    /// its command/event channels are the same typed envelopes as the pipe wire,
    /// so the daemon publishes events and commands unchanged.
    pub fn spawn_subagent(
        &mut self,
        seed: &str,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        let persist = std::env::var("QAQH_SUBAGENT_PERSIST").is_ok_and(|value| {
            matches!(value.as_str(), "1" | "true" | "on")
        });
        let ephemeral = !persist;
        self.spawn_subagent_inprocess(
            seed,
            SubagentSpawnSpec {
                tools: tools.to_vec(),
                model: model.map(str::to_string),
                base_url: base_url.map(str::to_string),
                max_tokens,
                ephemeral,
            },
        )
    }

    fn spawn_subagent_inprocess(
        &mut self,
        seed: &str,
        spec: SubagentSpawnSpec,
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.last_spawn
            .insert(seed.to_string(), std::time::Instant::now());
        let (generation, _) = self.activity.begin(seed);

        let channels = qaqh_msgloop::ringing_v1::loop_core::LoopChannels::new();
        let qaqh_msgloop::ringing_v1::loop_core::LoopChannels {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel,
            writer_dead,
        } = channels;
        let cancel_for_sender = cancel.clone();

        let event_seed = seed.to_string();
        let activity = self.activity.clone();
        let hub = self.hub.clone();
        let reader = std::thread::spawn(move || {
            crate::actor::run_inprocess_event_reader(event_rx, event_seed, generation, activity, hub);
        });

        let actor_seed = seed.to_string();
        let actor_spec = spec.clone();
        let tools_len = spec.tools.len();
        let workspace_mode = self.workspace_mode.clone();
        let workspace_env = self.workspace_env.clone();
        let thread = std::thread::Builder::new()
            .name(format!("qaqh-subagent-{actor_seed}"))
            .spawn(move || {
                crate::actor::run_subagent_actor(
                    actor_seed,
                    actor_spec,
                    cmd_rx,
                    event_tx,
                    cancel,
                    writer_dead,
                    workspace_mode,
                    workspace_env,
                );
            })
            .map_err(|e| format!("spawn in-process subagent {seed}: {e}"))?;

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                transport: AgentTransport::InProcess {
                    cmd_tx,
                    cancel: cancel_for_sender,
                },
                kind: AgentKind::Subagent(spec),
                reader: Some(reader),
                thread: Some(thread),
            },
        );
        log::info!(
            "[subagent] spawned in-process actor seed={seed} tools={tools_len} (no child process)"
        );
        Ok(())
    }

    fn spawn(&mut self, seed: &str, new_seed: Option<&str>) -> Result<(), String> {
        self.spawn_with(seed, new_seed, &[])
    }

    fn spawn_with(
        &mut self,
        seed: &str,
        new_seed: Option<&str>,
        extra_args: &[String],
    ) -> Result<(), String> {
        self.spawn_session_inprocess(seed, new_seed, extra_args)
    }

    fn spawn_session_inprocess(
        &mut self,
        seed: &str,
        new_seed: Option<&str>,
        _extra_args: &[String],
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.last_spawn
            .insert(seed.to_string(), std::time::Instant::now());
        let (generation, _) = self.activity.begin(seed);

        let channels = qaqh_msgloop::ringing_v1::loop_core::LoopChannels::new();
        let qaqh_msgloop::ringing_v1::loop_core::LoopChannels {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel,
            writer_dead,
        } = channels;
        let cancel_for_sender = cancel.clone();

        let event_seed = seed.to_string();
        let activity = self.activity.clone();
        let hub = self.hub.clone();
        let reader = std::thread::spawn(move || {
            crate::actor::run_inprocess_event_reader(event_rx, event_seed, generation, activity, hub);
        });

        // Resume worker: timeline is the authoritative turn ledger. The meta
        // turn_count can lag the timeline after a daemon restart, so the actor
        // must start its turn allocator above any timeline turn already sealed.
        let timeline_turn_count = if new_seed.is_none() {
            if let Some(hub) = self.hub.as_ref()
                && let Some(snapshot) = hub.timeline_snapshot(seed)
            {
                snapshot
                    .turns
                    .iter()
                    .filter_map(|turn| turn.turn_id.strip_prefix('t'))
                    .filter_map(|seq| seq.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0)
            } else {
                0
            }
        } else {
            0
        };

        let actor_seed = seed.to_string();
        let resume_seed = if new_seed.is_none() {
            Some(seed.to_string())
        } else {
            None
        };
        let new_seed_owned = new_seed.map(str::to_string);
        let workspace_mode = self.workspace_mode.clone();
        let workspace_env = self.workspace_env.clone();
        let thread = std::thread::Builder::new()
            .name(format!("qaqh-session-{actor_seed}"))
            .spawn(move || {
                crate::actor::run_session_actor(
                    actor_seed,
                    resume_seed,
                    new_seed_owned,
                    timeline_turn_count,
                    cmd_rx,
                    event_tx,
                    cancel,
                    writer_dead,
                    workspace_mode,
                    workspace_env,
                );
            })
            .map_err(|e| format!("spawn in-process session {seed}: {e}"))?;

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                transport: AgentTransport::InProcess {
                    cmd_tx,
                    cancel: cancel_for_sender,
                },
                kind: AgentKind::Session,
                reader: Some(reader),
                thread: Some(thread),
            },
        );
        log::info!("[session] spawned in-process actor seed={seed} (no child process)");
        Ok(())
    }

    /// 发送 Ringing worker 命令帧（携带 `wire` 判别字段；worker reader 按 wire 解析）。
    pub fn send_ringing(
        &mut self,
        seed: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.get_or_spawn(seed)?;
        let write = |instance: &AgentInstance| -> Result<(), String> {
            match &instance.transport {
                AgentTransport::InProcess { cmd_tx, cancel } => {
                    // Mirror the pipe reader: interrupt frames set the cancel
                    // token before they enter the command queue so long-running
                    // gate/tool work observes the abort immediately.
                    if qaqh_msgloop::ringing_v1::loop_core::ringing_command_is_interrupt(env) {
                        cancel.set();
                        qaqh_workspace::set_cancel(true);
                    }
                    let cmd = qaqh_msgloop::ringing_v1::types::WorkerCommand {
                        frame: env.clone(),
                        causation: Some(env.command_id.clone()),
                    };
                    cmd_tx
                        .send(cmd)
                        .map_err(|e| format!("agent command channel send: {e}"))
                }
            }
        };
        if write(self.instances.get(seed).expect("spawned instance")).is_ok() {
            return Ok(());
        }
        let kind = self
            .instances
            .get(seed)
            .map(AgentInstance::kind_name)
            .unwrap_or(AgentKind::Session);
        if let Some(dead) = self.instances.remove(seed) {
            dead.shutdown();
        }
        match kind {
            AgentKind::Session => self.get_or_spawn(seed)?,
            AgentKind::Subagent(spec) => self.spawn_subagent_inprocess(seed, spec)?,
        }
        write(self.instances.get(seed).expect("respawned instance"))
    }

    /// 向所有活跃 worker（含子代理）广播同一条 Ringing 命令。
    /// 只发给已运行的实例，不触发 spawn。返回失败项列表（seed: error）。
    pub fn broadcast_ringing(
        &mut self,
        command: &qaqh_ringing::RingingCommand,
    ) -> Vec<String> {
        let seeds: Vec<String> = self.instances.keys().cloned().collect();
        let mut failed = Vec::new();
        for seed in seeds {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                &seed,
                broadcast_command_id(),
                command.clone(),
            );
            if let Err(error) = self.send_ringing(&seed, &env) {
                failed.push(format!("{seed}: {error}"));
            }
        }
        failed
    }

    pub fn close(&mut self, seed: &str) {
        if let Some(instance) = self.instances.remove(seed) {
            instance.shutdown();
        }
    }

    pub fn shutdown_all(&mut self) {
        self.shutting_down = true;
        let mut instances: Vec<AgentInstance> = self
            .instances
            .drain()
            .map(|(_, instance)| instance)
            .collect();
        // Signal every worker before waiting on any of them. In-process actors
        // run concurrently (per-actor thread-local state); signal all before
        // joining any, so a busy actor is not left waiting on its channel while
        // shut down.
        for instance in &mut instances {
            instance.signal_shutdown();
        }
        for mut instance in instances {
            instance.finish_shutdown();
        }
    }

    /// F4: 拉起所有已退出且非优雅关闭的 worker。由 daemon 侧周期任务调用；
    /// 带 1 秒退避防止崩溃-重启风暴。优雅关闭（收到 Shutdown 帧后退出、
    /// 或被 `close`/`shutdown_all` 主动结束）的实例不会重启。
    pub fn respawn_dead_agents(&mut self) {
        if self.shutting_down {
            return;
        }
        let dead: Vec<(String, AgentKind)> = self
            .instances
            .iter()
            .filter_map(|(seed, instance)| {
                instance
                    .is_dead()
                    .then(|| (seed.clone(), instance.kind_name()))
            })
            .collect();
        for (seed, kind) in dead {
            // 退避：同一 seed 最近 1 秒内刚 spawn 过（例如刚拉起又立刻崩溃）
            // 则跳过本轮，避免无意义的重启风暴。
            if self
                .last_spawn
                .get(&seed)
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
            {
                log::warn!("[AGENT:{seed}] worker exited immediately after spawn; backing off");
                continue;
            }
            if let Some(instance) = self.instances.remove(&seed) {
                instance.shutdown();
            }
            log::warn!("[AGENT:{seed}] in-process worker died; respawning");
            let spawned = match kind {
                AgentKind::Session => self.spawn(&seed, None),
                AgentKind::Subagent(spec) => self.spawn_subagent_inprocess(&seed, spec),
            };
            if let Err(error) = spawned {
                log::error!("[AGENT:{seed}] respawn failed: {error}");
            } else if let Some(hub) = self.hub.as_ref() {
                // 与 get_or_spawn 一致：新 worker 接管前，把 timeline 中任何
                // 未 seal 的 running turn 收尾为 Cancelled。
                hub.seal_orphan_running_turns(&seed);
                // force=true：worker 已死亡，挂起交互必为孤儿，强制收尾。
                hub.seal_orphan_channel_state(&seed, true);
            }
        }
    }

    pub fn activities(&self) -> Vec<qaqh_proto::SessionActivity> {
        self.activity.snapshot()
    }

    pub fn activity(&self, seed: &str) -> Option<qaqh_proto::SessionActivity> {
        self.activity.get(seed)
    }

    pub fn is_running(&self, seed: &str) -> bool {
        self.instances.contains_key(seed)
    }

    /// 向所有存活 agent 广播同一 Ringing 命令。
    pub fn send_ringing_all(&mut self, command: qaqh_ringing::RingingCommand) {
        let seeds: Vec<_> = self.instances.keys().cloned().collect();
        for seed in seeds {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                seed.clone(),
                "daemon-broadcast",
                command.clone(),
            );
            let _ = self.send_ringing(&seed, &env);
        }
    }
}

impl AgentInstance {
    fn is_dead(&self) -> bool {
        match &self.transport {
            AgentTransport::InProcess { .. } => self
                .thread
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
        }
    }

    fn kind_name(&self) -> AgentKind {
        match &self.kind {
            AgentKind::Session => AgentKind::Session,
            AgentKind::Subagent(spec) => AgentKind::Subagent(spec.clone()),
        }
    }

    fn signal_shutdown(&mut self) {
        // 优雅关闭：agent 侧只识别 Ringing 帧（legacy Ui2Agent 已拆除）。
        let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
            self.seed.clone(),
            "daemon-shutdown",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionShutdown),
        );
        match &self.transport {
            AgentTransport::InProcess { cmd_tx, cancel } => {
                cancel.set();
                qaqh_workspace::set_cancel(true);
                let cmd = qaqh_msgloop::ringing_v1::types::WorkerCommand {
                    frame: env,
                    causation: Some("daemon-shutdown".into()),
                };
                let _ = cmd_tx.send(cmd);
            }
        }
    }

    fn finish_shutdown(&mut self) {
        // Join the loop thread first so it drops `event_tx`; the event reader
        // then drains the channel tail and publishes the last intents
        // (含 seal_turn——terminal intent 同步落盘).
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        log::info!("stopped agent {}", self.seed);
    }

    fn shutdown(mut self) {
        self.signal_shutdown();
        self.finish_shutdown();
    }
}

pub(crate) fn externalize_large_content(
    hub: &RingingHub,
    seed: &str,
    event: qaqh_domain::DomainEvent,
) -> qaqh_domain::DomainEvent {
    let qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result,
    }) = event
    else {
        return event;
    };
    let full_text = result.model.text.as_str();
    if full_text.len() <= crate::ringing::content_store::CONTENT_STORE_THRESHOLD_BYTES {
        return qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
            tool_call_id,
            turn_id,
            round_num,
            result,
        });
    }
    let content_id = hub.put_content(seed, "text/plain", full_text.as_bytes().to_vec(), true);
    let tail = tail_text(full_text, CONTENT_TAIL_BYTES);
    let mut projected = result;
    projected.summary = tail
        .chars()
        .take(qaqh_types::TOOL_SUMMARY_MAX_CHARS)
        .collect();
    projected.model.text = tail;
    projected.model.truncated = true;
    projected.output_ref = Some(qaqh_domain::ContentRef {
        content_id: content_id.clone(),
        media_type: "text/plain".into(),
        sha256: content_id.clone(),
        truncated: true,
    });
    qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result: projected,
    })
}

/// 事件内可渲染 tail 上限（与 ToolProgress tail 对齐）。
const CONTENT_TAIL_BYTES: usize = 256 * 1024;

/// 按 char 边界截取文本末尾最多 max_bytes（UTF-8 保守按 4 字节/字符）。
fn tail_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let max_chars = max_bytes / 4;
    text.chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_finished(summary: String) -> qaqh_domain::DomainEvent {
        let mut result = qaqh_domain::ToolResult::ok(summary.clone());
        // The worker normally sends the bounded model projection. This test
        // helper also covers the pre-projection large-output boundary used by
        // the content store.
        if summary.len() > qaqh_types::TOOL_MODEL_MAX_CHARS {
            result.model.text = summary;
            result.model.truncated = false;
        }
        qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
            tool_call_id: "t1".into(),
            turn_id: "turn1".into(),
            round_num: 0,
            result,
        })
    }

    #[test]
    fn large_tool_finished_is_externalized() {
        let hub = RingingHub::new("test");
        let big = "x".repeat(crate::ringing::content_store::CONTENT_STORE_THRESHOLD_BYTES + 1024);
        let out = externalize_large_content(&hub, "s1", tool_finished(big.clone()));
        match out {
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
                result,
                ..
            }) => {
                assert!(result.model.text.len() <= CONTENT_TAIL_BYTES);
                assert!(result.summary.chars().count() <= qaqh_types::TOOL_SUMMARY_MAX_CHARS);
                let rf = result.output_ref.expect("output_ref set");
                assert!(rf.truncated);
                assert_eq!(rf.media_type, "text/plain");
                // 完整内容可从 ContentStore 读回（会话所有权校验）
                let entry = hub.get_content("s1", &rf.content_id).expect("stored");
                assert_eq!(entry.bytes.len(), big.len());
                assert_eq!(entry.sha256, rf.sha256);
                // 跨会话不可读
                assert!(hub.get_content("other", &rf.content_id).is_none());
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn small_tool_finished_is_not_externalized() {
        let hub = RingingHub::new("test");
        let out = externalize_large_content(&hub, "s1", tool_finished("small".into()));
        match out {
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
                result,
                ..
            }) => {
                assert_eq!(result.summary, "small");
                assert!(result.output_ref.is_none());
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn non_tool_event_passes_through() {
        let hub = RingingHub::new("test");
        let ev =
            qaqh_domain::DomainEvent::Conversation(qaqh_domain::ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            });
        let out = externalize_large_content(&hub, "s1", ev);
        assert!(matches!(
            out,
            qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::TurnStarted {
                    turn_id,
                    user_text,
                }
            ) if turn_id == "t1" && user_text == "hi"
        ));
    }

    #[test]
    fn tail_text_respects_char_boundaries() {
        // 中文 3 字节/字符：按 4 字节/字符保守截取，不得切半个字符
        let text = "汉".repeat(200_000);
        let tail = tail_text(&text, 1024);
        assert!(tail.len() <= 1024);
        assert!(tail.chars().all(|c| c == '汉'));
        assert_eq!(tail, "汉".repeat(tail.chars().count()));
    }
}

/// Broadcast 命令 id（时间戳十六进制，语义同 service.rs 的 `command_id`）。
fn broadcast_command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("bcast-{nanos:x}")
}
