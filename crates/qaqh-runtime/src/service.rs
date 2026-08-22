use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex};

use qaqh_domain::ControlCommand;
use qaqh_proto::SessionActivityState;
use qaqh_ringing::{RingingCommand, RingingWorkerCommandEnvelope};
use serde_json::{Value, json};

use crate::{AgentRegistry, RingingHub};

/// workspace serve 当前运行状态（daemon 内存态，供前端展示诊断依据）。
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRuntimeState {
    /// 配置的模式（config.toml [workspace] mode）。
    pub configured_mode: String,
    /// 实际运行模式（WSL 降级后为 local）。
    pub active_mode: String,
    /// serve endpoint（空 = 未启用）。
    pub endpoint: String,
    /// 每次 workspace 守护重启后单调递增；不含任何凭据。
    pub generation: u64,
}

#[derive(Clone)]
pub struct QaqhService {
    pub(crate) registry: Arc<Mutex<AgentRegistry>>,
    pub(crate) hub: std::sync::OnceLock<Arc<RingingHub>>,
    workspace_state: Arc<Mutex<WorkspaceRuntimeState>>,
}

impl QaqhService {
    pub fn init() -> Self {
        let _config = qaqh_config::Config::load().unwrap_or_default();
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
        // daemon 进程的工具注册表（供 `skills.list_tools` 等查询；worker 各自
        // 独立 init_tools，本进程只提供注册表快照，不参与工具执行）。
        // 不带 subagent 注册器：设置页勾选的是子代理可用工具，spawn_subagent
        // 本身不属于子代理工具集。
        qaqh_workspace::runtime::init_tools("daemon", &[], vec![]);
        Self {
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            hub: std::sync::OnceLock::new(),
            workspace_state: Arc::new(Mutex::new(WorkspaceRuntimeState::default())),
        }
    }

    /// 挂载 Ringing 运行时（worker 事件双投）。
    pub fn attach_ringing(&self, hub: Arc<RingingHub>) {
        let _ = self.hub.set(hub.clone());
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_ringing(hub);
    }

    /// 转发 Ringing 命令到 agent worker（wire 判别后由 worker reader 解析）。
    pub fn send_ringing_command(
        &self,
        seed: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ringing(seed, env)
    }

    /// 关闭会话（Ringing `SessionClose` 命令语义，契约 §2）：
    /// 关闭 registry 实例并经 hub 发布 `SessionStateChanged { state: Closed }`，
    /// causation 挂命令 id。会话不存在同样返回 Ok（幂等关闭）。
    pub fn close_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.registry()?.close(seed);
        // 临时会话（子代理）用完即走：关闭后删除会话目录，磁盘零残留。
        // 目录已不存在（重复 close / 已被清理）时静默跳过，保持幂等。
        if qaqh_session::SessionManager::global().is_ephemeral(seed) {
            match qaqh_session::SessionManager::global().delete(seed) {
                Ok(()) => log::info!("[session] ephemeral session {seed} cleaned up (auto-unload)"),
                Err(e) if e.contains("Session not found") => {}
                Err(e) => log::warn!("[session] ephemeral cleanup {seed} failed: {e}"),
            }
        }
        if let Some(hub) = self.hub.get() {
            let _ = hub.publish_with_causation(
                seed,
                qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                    seed: seed.to_string(),
                    state: qaqh_domain::SessionState::Closed,
                }),
                causation_id,
            );
        }
        Ok(())
    }

    /// 归档会话（标签 × 语义）：关闭 registry 实例 + meta `archived=true`。
    /// 磁盘与消息文件保留，左侧列表归档组可见可恢复。会话不存在同样
    /// 幂等成功（close 幂等 + set_archived 补写 meta）。
    pub fn archive_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.close_session(seed, causation_id)?;
        qaqh_session::SessionManager::global().set_archived(seed, true);
        Ok(())
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, seed: &str) -> Result<(), String> {
        qaqh_session::SessionManager::global().set_archived(seed, false);
        self.registry()?.get_or_spawn(seed)
    }

    /// 彻底删除会话（左侧列表 × 语义）：先关实例（若运行，幂等）再删
    /// 磁盘目录与索引。会话不存在返回 Err（由 daemon 拦截层按幂等处理）。
    pub fn delete_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        let _ = self.close_session(seed, causation_id);
        qaqh_session::SessionManager::global().delete(seed)
    }

    pub fn handle(&self, method: &str, params: &Value) -> Result<Value, String> {
        let seed = || pstr(params, "seed");
        match method {
            "daemon.version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
            "workspace.set_mode" => {
                let mode = pstr(params, "mode")?;
                let supported = if cfg!(target_os = "windows") {
                    matches!(mode.as_str(), "local" | "wsl")
                } else {
                    // Linux 原生系统：工具本来就在 Linux 环境，无 WSL 选项。
                    mode == "local"
                };
                if !supported {
                    return Err(format!(
                        "invalid workspace mode '{mode}' (supported: {})",
                        if cfg!(target_os = "windows") {
                            "local | wsl"
                        } else {
                            "local"
                        }
                    ));
                }
                qaqh_config::Config::update(|config| {
                    config.workspace.mode = mode.clone();
                    Ok(())
                })
                .map_err(|e| format!("save config: {e}"))?;
                log::info!("[workspace] mode switched to {mode} (restart required)");
                Ok(json!({
                    "mode": mode,
                    "restart_required": true,
                }))
            }
            "workspace.status" => {
                let state = self
                    .workspace_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                Ok(json!({
                    "configured_mode": state.configured_mode,
                    "active_mode": state.active_mode,
                    "endpoint": state.endpoint,
                    "generation": state.generation,
                }))
            }
            // ── UI 工作区注册表（组织语义，与运行环境 workspace 解耦）──
            "workspace.list" => {
                let ws = qaqh_session::WorkspaceStore::global();
                let items: Vec<Value> = ws
                    .list()
                    .into_iter()
                    .map(|w| {
                        json!({
                            "id": w.id,
                            "path": w.path,
                            "title": w.title,
                            "order": w.order,
                            "session_ids": w.session_ids,
                            "missing_dir": !ws.path_status(&w.path),
                        })
                    })
                    .collect();
                Ok(Value::Array(items))
            }
            // 远端文件浏览（临时跨端模式）：路径一律是 daemon 侧绝对路径。
            "fs.list" => {
                let path = pstr(params, "path")?;
                list_remote_directory(&path)
            }
            "fs.read" => {
                let path = pstr(params, "path")?;
                let max_bytes = params
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(512 * 1024);
                read_remote_file(&path, max_bytes)
            }
            "workspace.create" => {
                let path = pstr(params, "path")?;
                let ws = qaqh_session::WorkspaceStore::global();
                let existing = qaqh_session::SessionManager::global().list();
                let created = ws.create(&path, &existing)?;
                Ok(serde_json::to_value(created).map_err(err)?)
            }
            "workspace.rename" => {
                let id = pstr(params, "id")?;
                let title = pstr(params, "title")?;
                let ws = qaqh_session::WorkspaceStore::global();
                let renamed = ws.rename(&id, title)?;
                Ok(serde_json::to_value(renamed).map_err(err)?)
            }
            "workspace.delete" => {
                let id = pstr(params, "id")?;
                qaqh_session::WorkspaceStore::global().delete(&id)?;
                Ok(Value::Null)
            }
            "workspace.move_session" => {
                let seed = pstr(params, "seed")?;
                let workspace_id = pstr(params, "workspace_id")?;
                qaqh_session::WorkspaceStore::global().move_session(&seed, &workspace_id)?;
                Ok(Value::Null)
            }
            "workspace.detach" => {
                let seed = pstr(params, "seed")?;
                qaqh_session::WorkspaceStore::global().remove_session(&seed);
                Ok(Value::Null)
            }
            "workspace.diagnose" => Ok(crate::workspace_supervisor::diagnose_wsl().map_err(err)?),
            "workspace.install_wsl" => {
                let repo_root = params
                    .get("repo_root")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(crate::workspace_supervisor::install_wsl(repo_root.as_deref()).map_err(err)?)
            }
            "session.list" => Ok(Value::Array(self.list_sessions())),
            "session.meta" => {
                let seed = seed()?;
                let manager = qaqh_session::SessionManager::global();
                let Some(meta) = manager.load_meta(&seed) else {
                    return Ok(Value::Null);
                };
                let mut value = serde_json::to_value(&meta).map_err(err)?;
                value["running"] = json!(self.registry()?.is_running(&meta.seed));
                Ok(value)
            }
            "session.activity" => {
                Ok(serde_json::to_value(self.registry()?.activities()).map_err(err)?)
            }
            "session.new" => {
                // 可选工具模式预置（TUI/CLI 壳在 create 时一次性锁定）。
                // 先于任何落盘校验：非法值必须整体拒绝，不得留下孤儿 meta。
                let preset = optional_tool_mode(params)?;
                let seed = qaqh_session::SessionManager::generate_seed();
                qaqh_session::SessionManager::global().clear_active();
                // 可选 cwd（前端在 workspace 上下文新建时传入）→ 记录 + 自动归属。
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                qaqh_session::SessionManager::global()
                    .persist_new_session_with_cwd(&seed, cwd.as_deref());
                // 先于 spawn 落盘：worker 的 init_session 从 meta 恢复并应用，
                // 保证 minimal:dsh 的极简 system prompt 首轮就生效。
                if let Some((tool_mode, custom_tools)) = preset {
                    qaqh_session::SessionManager::global()
                        .persist_tool_mode(&seed, &tool_mode, &custom_tools)
                        .map_err(|error| format!("persist tool_mode failed: {error}"))?;
                }
                self.registry()?.spawn_new(&seed)?;
                Ok(json!(seed))
            }
            "session.resume" => {
                let seed = seed()?;
                qaqh_session::SessionManager::global().set_active_seed(&seed);
                self.registry()?.get_or_spawn(&seed)?;
                Ok(Value::Null)
            }
            "session.set_tool_mode" => {
                let seed = seed()?;
                let tool_mode = pstr(params, "tool_mode")?;
                validate_tool_mode(&tool_mode)?;
                let custom_tools = pstrings(params, "custom_tools");
                if tool_mode == "custom" && custom_tools.is_empty() {
                    return Err(
                        "custom tool mode requires at least one tool in custom_tools".to_string(),
                    );
                }
                // 先持久化（meta.json，重启存活），再通知 worker 应用
                // （set_allowed_tools + tool_defs 刷新 = 模型侧源头过滤）。
                // CK-PERSIST：持久化失败 → 400 返回前端，前端回滚乐观值；
                // 不允许「应用成功但没落盘」的假切换（重启即丢）。
                qaqh_session::SessionManager::global()
                    .persist_tool_mode(&seed, &tool_mode, &custom_tools)
                    .map_err(|error| format!("persist tool_mode failed: {error}"))?;
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::SetToolMode {
                        tool_mode,
                        custom_tools,
                    }),
                )
            }
            "session.dashboard" => dashboard(&seed()?),
            "session.get_activity" => activity(&seed()?),
            "skills.operation" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsOperation {
                    operation_id: pstr2(params, "operation_id", "operationId")?,
                    action: pstr(params, "action")?,
                    name: pstr(params, "name")?,
                }),
            ),
            "skills.reload" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsReload),
            ),
            "skills.activate" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsActivate {
                    name: pstr(params, "name")?,
                }),
            ),
            "skills.list_tools" => Ok(json!(qaqh_workspace::runtime::process_all_tool_names())),
            "workspace.get" => Ok(json!(workspace(&seed()?))),
            "workspace.set" => {
                let seed = seed()?;
                // 统一数据源：运行环境工作目录存 meta.cwd（workspace.txt 退役）。
                qaqh_session::SessionManager::global().set_cwd(
                    &seed,
                    pstr(params, "path")?.trim(),
                    true,
                );
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::AgentReloadConfig),
                )?;
                Ok(Value::Null)
            }
            "git.diff" => git(
                &seed()?,
                |ws| qaqh_workspace::git::status_json(ws),
                json!([]),
            ),
            "git.branch" => git(
                &seed()?,
                |ws| qaqh_workspace::git::current_branch(ws),
                Value::Null,
            ),
            "git.branches" => git(
                &seed()?,
                |ws| qaqh_workspace::git::list_branches(ws),
                json!([]),
            ),
            "git.switch_branch" => git(
                &seed()?,
                |ws| {
                    qaqh_workspace::git::switch_branch(
                        ws,
                        &pstr(params, "branch")?,
                        pbool(params, "stash"),
                    )
                },
                Value::Null,
            ),
            "git.commit" => git(
                &seed()?,
                |ws| qaqh_workspace::git::commit_all(ws, &pstr(params, "message")?),
                Value::Null,
            ),
            "git.file_diff" => git(
                &seed()?,
                |ws| qaqh_workspace::git::file_diff(ws, &pstr2(params, "file_path", "filePath")?),
                Value::Null,
            ),
            "config.load" => load_config(),
            "config.save" => {
                self.save_config(params)?;
                Ok(Value::Null)
            }
            // 权限等级（L1-L4）：与 config.save 共用 Config::update 单写口；
            // 校验 → 写 config.toml → 广播 AgentReloadConfig 让所有活跃
            // worker（含子代理，子代理继承同一全局权限）重载。
            "config.set_permission_level" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "permission level (1-4) is required".to_string())?;
                if !(1..=4).contains(&level) {
                    return Err(format!("invalid permission level {level} (must be 1-4)"));
                }
                self.update_config_and_reload(|cfg| {
                    cfg.permission_level = level as u8;
                    Ok(())
                })?;
                log::info!("[config] permission level set to {level}");
                Ok(json!({ "permission_level": level }))
            }
            "profile.apply" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    if cfg.apply_profile(&name).is_none() {
                        return Err(format!("profile '{name}' not found"));
                    }
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "profile.save_current" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    cfg.save_profile(&name);
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "profile.delete" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    if !cfg.delete_profile(&name) {
                        return Err(format!(
                            "profile '{name}' cannot be deleted (not found or default)"
                        ));
                    }
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "todo.status" => parse_json_string(qaqh_workspace::todo::todo_status_json(&seed()?)?),
            "todo.cancel" => parse_json_string(qaqh_workspace::todo::todo_cancel_json(
                &seed()?,
                &pstr(params, "id")?,
            )?),
            "plan.context_stats" => context_stats(&seed()?),
            "stats.token_usage" => token_stats(pu64(params, "days") as u32),
            "plan.read" => read_plan(&seed()?),
            "plan.action" => {
                plan_action(
                    &seed()?,
                    &pstr2(params, "item_id", "itemId")?,
                    &pstr(params, "action")?,
                    value2(params, "user_comment", "userComment")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                Ok(Value::Null)
            }
            // ── Subagent orchestration ──────────────────────────────────────
            // Spawn an isolated subagent worker and return its seed. The
            // caller (parent agent) then attaches the seed and drives it with
            // ordinary Ringing commands/events (ConversationSendMessage →
            // TurnCompleted). The worker runs ephemeral with the given tool
            // allowlist and optional model/base-url/max-tokens overrides.
            "subagent.spawn" => {
                let tools: Vec<String> = params
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let model = params
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let base_url = params
                    .get("base_url")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let max_tokens = params
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                // 子代理继承主代理的 workspace：spawn 前写入
                // `sessions/{sub_seed}/workspace.txt`，子 worker 启动时
                // `load_session_workspace` 读到，从而正确解析相对路径并
                // 以主代理工作区为权限边界（修复子代理"不知道工作区、
                // 相对路径落到 daemon cwd"的问题）。
                let workspace = params
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|w| !w.is_empty() && *w != ".")
                    .map(String::from);
                let seed = qaqh_session::SessionManager::generate_seed();
                if let Some(workspace) = &workspace {
                    // 子代理继承主代理的 workspace：写入 meta.cwd（统一数据源），
                    // 子 worker 启动时 `load_session_workspace` 读到，从而正确解析
                    // 相对路径并以主代理工作区为权限边界。
                    qaqh_session::SessionManager::global().set_cwd(&seed, workspace, false);
                    log::info!("[subagent] inherited workspace for seed={seed}: {workspace}");
                }
                self.registry()?.spawn_subagent(
                    &seed,
                    &tools,
                    model.as_deref(),
                    base_url.as_deref(),
                    max_tokens,
                )?;
                log::info!(
                    "[subagent] spawned subagent worker seed={seed} tools={}",
                    tools.len()
                );
                Ok(json!({ "seed": seed }))
            }
            _ => Err(format!("unknown method: {method}")),
        }
    }

    pub fn shutdown(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_all();
    }

    /// F4: 死 worker 重生（daemon 周期任务调用；内部自带退避与关闭保护）。
    pub fn respawn_dead_agents(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .respawn_dead_agents();
    }

    /// True while stopping the daemon would interrupt work or abandon an
    /// interaction waiting for its lease owner. Used by lifecycle takeover so
    /// an updater cannot race a newly-started turn.
    pub fn has_active_work(&self) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities()
            .iter()
            .any(|activity| {
                matches!(
                    activity.state,
                    SessionActivityState::Starting
                        | SessionActivityState::Working
                        | SessionActivityState::WaitingUser
                )
            })
    }

    pub(crate) fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    /// 注入 workspace serve 连接信息与运行模式（worker spawn 时写入 env）。
    /// `mode` ∈ {"local", "wsl"}：local 工具执行保持进程内，wsl 才启用远程 HTTP。
    pub fn attach_workspace(&self, endpoint: String, token: String, mode: &str) {
        if let Ok(mut registry) = self.registry() {
            registry.attach_workspace(endpoint, token, mode);
        }
    }

    /// 记录 workspace serve 实际运行状态（server.rs 启动 supervisor 后调用）。
    pub fn attach_workspace_state(&self, state: WorkspaceRuntimeState) {
        *self
            .workspace_state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = state;
    }

    /// 构造 Ringing worker 命令信封并转发给 agent（legacy Ui2Agent 帧已拆除）。
    fn send_ringing_cmd(&self, seed: String, command: RingingCommand) -> Result<Value, String> {
        let env = RingingWorkerCommandEnvelope::new(seed.clone(), command_id(), command);
        self.send_ringing_command(&seed, &env)?;
        Ok(Value::Null)
    }

    fn list_sessions(&self) -> Vec<Value> {
        let manager = qaqh_session::SessionManager::global();
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let workspaces = qaqh_session::WorkspaceStore::global();
        manager
            .list()
            .into_iter()
            .map(|meta| {
                let mut value = serde_json::to_value(&meta).unwrap_or_default();
                value["running"] = json!(registry.is_running(&meta.seed));
                value["workspace_id"] = workspaces
                    .workspace_of(&meta.seed)
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                value
            })
            .collect()
    }

    /// 配置写后广播（所有 config.* / profile.* 写路径共用）。
    fn notify_config_changed(&self) {
        let failed = match self.registry() {
            Ok(mut registry) => registry
                .broadcast_ringing(&RingingCommand::Control(ControlCommand::AgentReloadConfig)),
            Err(error) => {
                log::warn!("[config] registry unavailable for reload broadcast: {error}");
                return;
            }
        };
        if !failed.is_empty() {
            log::warn!(
                "[config] reload broadcast failed for: {}",
                failed.join("; ")
            );
        }
    }

    /// 单写口：`Config::update` 成功后广播 reload（BUG-001/008）。
    fn update_config_and_reload<F>(&self, mutate: F) -> Result<qaqh_config::Config, String>
    where
        F: FnOnce(&mut qaqh_config::Config) -> Result<(), String>,
    {
        let config = qaqh_config::Config::update(mutate)?;
        self.notify_config_changed();
        Ok(config)
    }

    fn save_config(&self, params: &Value) -> Result<(), String> {
        // Never log config.save payloads: they may contain provider credentials.
        log::info!("[config.save] saving configuration");
        self.update_config_and_reload(|cfg| {
            // 审计 P0-1：apiKey 处理（2026-08 修复 Bug 根因 1）：
            // - "****" = 掩码占位符，保持现值（与 Web isMasked 一致）；
            // - 空串 = 保持现值（与 update_string 语义一致，防前端误发空导致误删）；
            //   显式删除需走专用接口/手动清文件，避免“任意保存都清密钥”的幽灵重置。
            //   此前 winui 前端在未编辑密钥时仍发送 ""，被误判为删除。
            if let Some(value) = value2(params, "api_key", "apiKey").and_then(Value::as_str) {
                if value != "****" && !value.is_empty() {
                    cfg.api_key = value.to_string();
                } else if value.is_empty() {
                    log::info!("[config.save] apiKey empty -> keep existing (skip delete)");
                }
            }
            update_string(&mut cfg.model, params, "model", "model");
            update_string(&mut cfg.base_url, params, "base_url", "baseUrl");
            update_string(&mut cfg.provider_id, params, "provider_id", "providerId");
            update_string(&mut cfg.endpoint, params, "endpoint", "endpoint");
            update_string(
                &mut cfg.reasoning_effort,
                params,
                "reasoning_effort",
                "reasoningEffort",
            );
            update_u32(&mut cfg.max_tokens, params, "max_tokens", "maxTokens");
            update_u32(
                &mut cfg.context_limit,
                params,
                "context_limit",
                "contextLimit",
            );
            if let Some(lang) = value2(params, "lang", "lang")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                cfg.lang = Some(lang.to_string());
            }
            // ── UI 字体（空字符串 = 恢复系统默认；掩码守卫同 update_string）──
            if let Some(value) = value2(params, "font_family", "fontFamily").and_then(Value::as_str)
            {
                if value != "****" {
                    cfg.font_family = value.to_string();
                }
            }
            // ── UI 主题（空值/null = 跟随系统）──
            if let Some(value) = value2(params, "theme", "theme").and_then(Value::as_str) {
                cfg.theme = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                };
            }
            // ── 桌面通知（缺省 = 开启）──
            if let Some(enabled) = value2(params, "notifications_enabled", "notificationsEnabled")
                .and_then(Value::as_bool)
            {
                cfg.notifications_enabled = Some(enabled);
            }
            update_string(
                &mut cfg.subagent.model,
                params,
                "subagent_model",
                "subagentModel",
            );
            update_string(
                &mut cfg.subagent.base_url,
                params,
                "subagent_base_url",
                "subagentBaseUrl",
            );
            update_string(
                &mut cfg.subagent.api_key,
                params,
                "subagent_api_key",
                "subagentApiKey",
            );
            update_u32(
                &mut cfg.subagent.max_tokens,
                params,
                "subagent_max_tokens",
                "subagentMaxTokens",
            );
            if let Some(value) = value2(params, "subagent_timeout_secs", "subagentTimeoutSecs")
                .and_then(Value::as_u64)
                .filter(|v| *v > 0)
            {
                cfg.subagent.timeout_secs = value;
            }
            // 允许空数组保存：配置语义中 `default_tools = []` 表示"全部工具可用"，
            // 用户在前端取消所有勾选时应能表达该状态（此前空数组被过滤、无法保存）。
            if let Some(values) = value2(params, "subagent_default_tools", "subagentDefaultTools")
                .and_then(Value::as_array)
            {
                cfg.subagent.default_tools = values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
            if let Some(path) =
                value2(params, "tokenizer_path", "tokenizerPath").and_then(Value::as_str)
            {
                cfg.tokenizer_path = (!path.is_empty()).then(|| path.to_string());
            }
            // ── Multimodal (vision) config ──
            if let Some(enabled) =
                value2(params, "multimodal_enabled", "multimodalEnabled").and_then(Value::as_bool)
            {
                cfg.multimodal.enabled = enabled;
            }
            update_string(
                &mut cfg.multimodal.provider_type,
                params,
                "multimodal_provider_type",
                "multimodalProviderType",
            );
            update_string(
                &mut cfg.multimodal.provider_id,
                params,
                "multimodal_provider_id",
                "multimodalProviderId",
            );
            update_string(
                &mut cfg.multimodal.api_key,
                params,
                "multimodal_api_key",
                "multimodalApiKey",
            );
            update_string(
                &mut cfg.multimodal.base_url,
                params,
                "multimodal_base_url",
                "multimodalBaseUrl",
            );
            update_string(
                &mut cfg.multimodal.model,
                params,
                "multimodal_model",
                "multimodalModel",
            );
            update_u32(
                &mut cfg.multimodal.max_tokens,
                params,
                "multimodal_max_tokens",
                "multimodalMaxTokens",
            );
            if let Some(threshold) =
                value2(params, "auto_compact_threshold", "autoCompactThreshold")
                    .and_then(Value::as_f64)
            {
                cfg.auto_compact_threshold = threshold;
            }
            if let Some(enabled) =
                value2(params, "compliance_enabled", "complianceEnabled").and_then(Value::as_bool)
            {
                cfg.compliance_enabled = enabled;
            }
            Ok(())
        })?;
        Ok(())
    }
}

fn value2<'a>(params: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    params.get(snake).or_else(|| params.get(camel))
}
fn pstr(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {key}"))
}
fn pstr2(params: &Value, snake: &str, camel: &str) -> Result<String, String> {
    value2(params, snake, camel)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {snake}"))
}
fn pbool(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}
fn pu64(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or_default()
}
fn pstrings(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// `fs.list`：目录条目（目录优先 + 名称排序），返回 daemon 侧绝对路径。
///
/// 临时跨端版本有意不做路径沙箱/权限校验，只要求绝对路径。
fn list_remote_directory(path: &str) -> Result<Value, String> {
    let dir = std::path::Path::new(path);
    if !dir.is_absolute() {
        return Err("fs.list requires an absolute path".to_string());
    }
    let mut entries: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("fs.list {path}: {e}"))? {
        let entry = entry.map_err(|e| format!("fs.list {path}: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // 软链接/权限问题不阻塞整个目录，标记 unknown 继续。
            Err(_) => {
                entries.push(json!({
                    "name": name,
                    "path": entry_path.to_string_lossy(),
                    "is_dir": false,
                    "is_file": false,
                    "size": 0,
                    "modified_ms": null,
                }));
                continue;
            }
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        entries.push(json!({
            "name": name,
            "path": entry_path.to_string_lossy(),
            "is_dir": meta.is_dir(),
            "is_file": meta.is_file(),
            "size": meta.len(),
            "modified_ms": modified_ms,
        }));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (
            a["is_dir"].as_bool().unwrap_or(false),
            b["is_dir"].as_bool().unwrap_or(false),
        );
        match bd.cmp(&ad) {
            std::cmp::Ordering::Equal => a["name"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .cmp(&b["name"].as_str().unwrap_or_default().to_ascii_lowercase()),
            other => other,
        }
    });
    Ok(Value::Array(entries))
}

/// `fs.read`：文本预览。读满 `max_bytes + 1` 以判断截断；内容按 UTF-8
/// lossy 返回（临时版不处理二进制编码协商）。
fn read_remote_file(path: &str, max_bytes: u64) -> Result<Value, String> {
    let file_path = std::path::Path::new(path);
    if !file_path.is_absolute() {
        return Err("fs.read requires an absolute path".to_string());
    }
    let meta = std::fs::metadata(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("fs.read {path}: not a file"));
    }
    let cap = max_bytes.clamp(1, 8 * 1024 * 1024) as usize;
    let file = std::fs::File::open(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    let mut data = Vec::new();
    std::io::Read::take(file, (cap + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("fs.read {path}: {e}"))?;
    let truncated = data.len() > cap;
    data.truncate(cap);
    Ok(json!({
        "path": path,
        "size": meta.len(),
        "truncated": truncated,
        "content": String::from_utf8_lossy(&data),
    }))
}

/// 可选工具模式预置：缺省 = None（保持旧行为）；显式空串 = standard 零迁移。
/// 供 create 路径（session.new）在 spawn 前落盘使用。
fn optional_tool_mode(params: &Value) -> Result<Option<(String, Vec<String>)>, String> {
    let Some(tool_mode) = params.get("tool_mode").and_then(Value::as_str) else {
        return Ok(None);
    };
    // 显式空串 = 未指定（与 SessionMeta.tool_mode 的空串零迁移语义一致）。
    if tool_mode.is_empty() {
        return Ok(None);
    }
    validate_tool_mode(tool_mode)?;
    let custom_tools = pstrings(params, "custom_tools");
    if tool_mode == "custom" && custom_tools.is_empty() {
        return Err("custom tool mode requires at least one tool in custom_tools".to_string());
    }
    Ok(Some((tool_mode.to_string(), custom_tools)))
}

/// 工具模式白名单校验（session.new 预置与 session.set_tool_mode 共用）。
/// 白名单由 `qaqh_types::tool_mode::KNOWN_MODES` 单一契约提供（BUG-013）。
fn validate_tool_mode(tool_mode: &str) -> Result<(), String> {
    if qaqh_types::is_known(tool_mode) {
        Ok(())
    } else {
        Err(format!(
            "invalid tool_mode '{tool_mode}' (expected {})",
            qaqh_types::KNOWN_MODES.join(" | ")
        ))
    }
}
fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn parse_json_string(value: String) -> Result<Value, String> {
    serde_json::from_str(&value).map_err(err)
}

fn update_string(target: &mut String, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        // Guard: skip the masked placeholder used by load_config
        if value == "****" {
            log::info!("[update_string] skipping masked placeholder '****' for field '{snake}'");
            return;
        }
        *target = value.to_string();
    }
}
fn update_u32(target: &mut u32, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
    {
        *target = value as u32;
    }
}

fn workspace(seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    // 统一数据源：meta.cwd（workspace.txt 退役，读取侧惰性迁移）。
    qaqh_session::workspace::session_workspace_cwd(seed).unwrap_or_default()
}

fn git<F>(seed: &str, operation: F, empty: Value) -> Result<Value, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let workspace = workspace(seed);
    if workspace.is_empty() {
        return Ok(empty);
    }
    let value = operation(&workspace)?;
    serde_json::from_str(&value).or_else(|_| Ok(json!(value)))
}

fn dashboard(seed: &str) -> Result<Value, String> {
    let dir = qaqh_types::platform::sessions_dir().join(seed);
    let tasks: Vec<Value> = qaqh_workspace::todo::todo_status_json(seed)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("items")?.as_array().map(|arr| {
                arr.iter()
                    .map(|item| {
                        json!({
                            "id": item["id"],
                            "subject": item["title"],
                            "description": item["description"],
                            "status": item["status"],
                            "evidence": item["evidence"],
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let mut edits = std::fs::File::open(dir.join("code_stats.jsonl"))
        .ok()
        .into_iter()
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    edits.reverse();
    edits.dedup();
    edits.truncate(10);
    Ok(json!({"tasks":tasks,"recent_edits":edits}))
}

fn activity(seed: &str) -> Result<Value, String> {
    let (_, messages) = qaqh_session::SessionManager::global()
        .load(seed)
        .ok_or_else(|| "session not found".to_string())?;
    let mut tools = std::collections::HashMap::new();
    for message in &messages {
        if message.role == "assistant" {
            for block in &message.content {
                if let qaqh_types::ContentBlock::ToolUse { id, name, input } = block {
                    tools.insert(id.clone(), (name.clone(), input.to_string()));
                }
            }
        }
    }
    let mut result = Vec::new();
    for message in &messages {
        if message.role == "tool" {
            for block in &message.content {
                if let qaqh_types::ContentBlock::ToolResult {
                    tool_use_id,
                    result: tool_result,
                } = block
                {
                    let (name, args) = tools.get(tool_use_id).cloned().unwrap_or_default();
                    result.push(json!({"tool_name":name,"summary":tool_result.summary,"status":serde_json::to_value(tool_result.status).unwrap_or_default(),"time":message.msg_id.map(|v|v.to_string()).unwrap_or_default(),"args":args}));
                }
            }
        }
    }
    result.reverse();
    Ok(Value::Array(result))
}

fn load_config() -> Result<Value, String> {
    let cfg = qaqh_config::Config::load().map_err(err)?;
    let providers = qaqh_config::registry::all_providers().into_iter().map(|provider| json!({"id":provider.id,"display":provider.display,"endpoints":provider.endpoints.into_iter().map(|endpoint|json!({"id":endpoint.id,"display":endpoint.display,"protocol":endpoint.protocol,"base_url":endpoint.base_url,"default_model":endpoint.default_model,"models":endpoint.models,"stateful":endpoint.stateful,"beta":endpoint.beta})).collect::<Vec<_>>() })).collect::<Vec<_>>();
    // profile 名称列表（前端 profile 管理 UI 用；不含敏感字段）。
    let profile_names: Vec<String> = cfg.profiles.keys().cloned().collect();
    Ok(
        json!({"api_key":if cfg.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.api_key.is_empty(),"model":cfg.model,"base_url":cfg.base_url,"provider_id":cfg.provider_id,"endpoint":cfg.endpoint,"max_tokens":cfg.max_tokens,"context_limit":cfg.context_limit,"reasoning_effort":cfg.reasoning_effort,"auto_compact_threshold":cfg.auto_compact_threshold,"permission_level":cfg.permission_level,"lang":cfg.lang,"font_family":cfg.font_family,"theme":cfg.theme,"notifications_enabled":cfg.notifications_enabled.unwrap_or(true),"active_profile":cfg.active_profile,"profiles":profile_names,"compliance_enabled":cfg.compliance_enabled,"providers":providers,"subagent":{"model":cfg.subagent.model,"base_url":cfg.subagent.base_url,"api_key":if cfg.subagent.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.subagent.api_key.is_empty(),"max_tokens":cfg.subagent.max_tokens,"timeout_secs":cfg.subagent.timeout_secs,"default_tools":cfg.subagent.default_tools},"multimodal":{"enabled":cfg.multimodal.enabled,"provider_type":cfg.multimodal.provider_type,"provider_id":cfg.multimodal.provider_id,"api_key":if cfg.multimodal.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.multimodal.api_key.is_empty(),"base_url":cfg.multimodal.base_url,"model":cfg.multimodal.model,"max_tokens":cfg.multimodal.max_tokens},"workspace":{"mode":cfg.workspace.mode},"tokenizer_path":cfg.tokenizer_path}),
    )
}

fn context_stats(seed: &str) -> Result<Value, String> {
    // 统一数据源：meta.json 的 context_stats 字段（原独立文件退役）。
    // 旧 context_stats.json 为可再生缓存，忽略不迁移。
    if let Some(meta) = qaqh_session::SessionManager::global().load_meta(seed) {
        if let Some(stats) = meta.context_stats {
            return Ok(stats);
        }
    }
    Ok(
        json!({"messages":0,"chat_text":0,"thinking":0,"tool_calls":0,"tool_results":0,"tools_schema":0,"system_prompt":0,"thinking_blocks":0,"tool_call_blocks":0}),
    )
}

fn token_stats(days: u32) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let days = days.max(1);
    let cutoff = days_before_today(days);
    let mut daily: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(file) =
        std::fs::File::open(qaqh_types::platform::data_dir().join("token_stats.jsonl"))
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let date = entry["date"].as_str().unwrap_or_default().to_string();
            if date < cutoff {
                continue;
            }
            let day=daily.entry(date).or_insert_with(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_hit",
                "cache_miss",
            ] {
                day[key] = json!(day[key].as_u64().unwrap_or(0) + entry[key].as_u64().unwrap_or(0));
            }
            day["calls"] = json!(day["calls"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut values = Vec::new();
    let mut prompt = 0;
    let mut completion = 0;
    let mut hit = 0;
    let mut miss = 0;
    let mut calls = 0;
    for offset in (0..days).rev() {
        let date = days_before_today(offset);
        let entry=daily.get(&date).cloned().unwrap_or_else(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
        prompt += entry["prompt_tokens"].as_u64().unwrap_or(0);
        completion += entry["completion_tokens"].as_u64().unwrap_or(0);
        hit += entry["cache_hit"].as_u64().unwrap_or(0);
        miss += entry["cache_miss"].as_u64().unwrap_or(0);
        calls += entry["calls"].as_u64().unwrap_or(0);
        values.push(json!({"date":date,"prompt_tokens":entry["prompt_tokens"],"completion_tokens":entry["completion_tokens"],"cache_hit":entry["cache_hit"],"cache_miss":entry["cache_miss"],"calls":entry["calls"]}));
    }
    let pct = if hit + miss > 0 {
        (hit as f64 / (hit + miss) as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };
    Ok(
        json!({"daily":values,"totals":{"prompt_tokens":prompt,"completion_tokens":completion,"calls":calls,"cache_hit_pct":pct}}),
    )
}
fn days_before_today(days: u32) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d) = qaqh_types::platform::civil_from_days((seconds / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn qaqh_dir(seed: &str) -> std::path::PathBuf {
    let workspace = workspace(seed);
    if workspace.is_empty() || workspace == "." {
        qaqh_types::platform::data_dir().join("workspace")
    } else {
        std::path::Path::new(&workspace).join(".deepx")
    }
}
fn read_plan(seed: &str) -> Result<Value, String> {
    let content = match std::fs::read_to_string(qaqh_dir(seed).join("PLAN.md")) {
        Ok(value) => value,
        Err(_) => return Ok(json!([])),
    };
    let items = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("- [") {
                return None;
            }
            let end = line.find(']')?;
            let status = line.get(3..end)?.trim();
            let rest = line.get(end + 1..)?.trim();
            let (id, title) = rest.split_once(": ")?;
            Some(json!({"id":id,"title":title,"status":status,"comment":"","actions":[]}))
        })
        .collect();
    Ok(Value::Array(items))
}
fn plan_action(seed: &str, item_id: &str, action: &str, comment: &str) -> Result<(), String> {
    let path = qaqh_dir(seed).join("PLAN.md");
    let content = std::fs::read_to_string(&path).map_err(err)?;
    let mut found = false;
    let output = content
        .lines()
        .filter_map(|line| {
            if !found && line.trim().starts_with("- [") && line.contains(&format!(" {item_id}: ")) {
                found = true;
                if action == "delete" {
                    return None;
                }
                let end = line.find(']')?;
                // ']' 为单字节 ASCII，end+1 必为 char boundary。
                let rest = line.split_at(end + 1).1;
                let base = format!("- [ ]{rest}");
                return Some(match action {
                    "approve" => base.replacen("- [ ]", "- [✓]", 1),
                    "reject" => {
                        let value = base.replacen("- [ ]", "- [-]", 1);
                        if comment.is_empty() {
                            value
                        } else {
                            format!("{value} | {comment}")
                        }
                    }
                    "ask" => base.replacen("- [ ]", "- [?]", 1),
                    _ => line.to_string(),
                });
            }
            Some(line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !found {
        return Err(format!("plan item {item_id} not found"));
    }
    std::fs::write(path, output).map_err(err)
}

#[allow(dead_code)]
#[cfg(test)]
mod tool_mode_tests {
    use super::{optional_tool_mode, validate_tool_mode};

    #[test]
    fn optional_tool_mode_defaults_to_none() {
        assert_eq!(optional_tool_mode(&serde_json::json!({})).unwrap(), None);
    }

    #[test]
    fn optional_tool_mode_parses_minimal_dsh() {
        let (mode, tools) = optional_tool_mode(&serde_json::json!({
            "tool_mode": "minimal:dsh",
        }))
        .unwrap()
        .unwrap();
        assert_eq!(mode, "minimal:dsh");
        assert!(tools.is_empty());
    }

    #[test]
    fn optional_tool_mode_rejects_unknown_values() {
        assert!(optional_tool_mode(&serde_json::json!({ "tool_mode": "turbo" })).is_err());
    }

    #[test]
    fn optional_tool_mode_custom_requires_tools() {
        assert!(optional_tool_mode(&serde_json::json!({ "tool_mode": "custom" })).is_err());
        let (mode, tools) = optional_tool_mode(&serde_json::json!({
            "tool_mode": "custom",
            "custom_tools": ["bash"],
        }))
        .unwrap()
        .unwrap();
        assert_eq!(mode, "custom");
        assert_eq!(tools, vec!["bash"]);
    }

    #[test]
    fn validate_tool_mode_accepts_all_known_presets() {
        for mode in qaqh_types::KNOWN_MODES {
            assert!(validate_tool_mode(mode).is_ok(), "{mode}");
        }
        assert!(validate_tool_mode("turbo").is_err());
    }
}

fn command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("svc-{nanos:x}")
}
