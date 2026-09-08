use std::sync::{Arc, Mutex};

use qaqh_domain::ActivityState;
use qaqh_domain::ControlCommand;
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
    /// 会话存储句柄（PR-3-1 注入化：daemon main 装配点 init 后注入，
    /// service 内不再触达会话单例的全局访问器）。
    pub(crate) sessions: Arc<qaqh_session::SessionManager>,
}

impl QaqhService {
    pub fn init(sessions: Arc<qaqh_session::SessionManager>) -> Self {
        let mut config = qaqh_config::Config::load().unwrap_or_default();
        // PR-M3-2 路线 A：用户级外部 MCP 配置只读合并（Codex/Claude/opencode，
        // import_external 默认开）。只改运行时视图，不回写 config；报告落日志。
        // 注意 enabled 总闸不变：外部合并的 server 也受 [mcp].enabled 管辖。
        {
            let paths = qaqh_config::mcp_import::default_user_paths();
            if let Some(report) = qaqh_config::mcp_import::merge_external(&mut config.mcp, &paths)
            {
                if !report.merged.is_empty() {
                    log::info!("[mcp] external config merged: {:?}", report.merged);
                }
                for collision in &report.skipped_collisions {
                    log::info!(
                        "[mcp] external config skipped (name collision, local wins): {collision}"
                    );
                }
                for invalid in &report.skipped_invalid {
                    log::warn!("[mcp] external config skipped (invalid): {invalid}");
                }
            }
        }
        // MCP manager 装配（设计 §10-6 / PR-M1-5）：daemon 进程级单例（actor
        // 线程与其工具线程同进程，全局槽位可见）；装配不做同步网络操作，
        // 连接由下方预热（后台）+ lazy（工具执行路径）双入口拉起。secret
        // store 用默认位置（[secrets.mcp] 段）；禁用配置时 manager 以
        // disabled 形态拒绝一切调用（MCP_DISABLED）。
        qaqh_mcp::install_manager(qaqh_mcp::McpManager::new(config.mcp.clone()));
        // LSP manager 装配（docs/lsp-client-design.md §4：mcp 同款单例；
        // 默认关闭——enabled=false 时 disabled 形态拒一切调用 LSP_DISABLED）。
        qaqh_lsp::install_manager(qaqh_lsp::LspManager::new(config.lsp.clone()));
        // P2-1：热重载接线——①重载器：订阅 watch 单写口广播，[mcp] 段变化
        // → 外部配置重扫（Codex/Claude 用户级）→ apply_config diff 保连；
        // ②文件轮询器：手改 config.toml（不经单写口）→ mtime 检测 →
        // reload_from_disk 统一发布。fire-and-forget，不阻塞启动。守卫：
        // 仅在 tokio runtime 上下文内接线（daemon main 是 async；单元测试
        // 等同步调用方跳过——热重载只对长驻 daemon 有意义）。
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn_mcp_reloader();
            spawn_lsp_reloader();
            spawn_config_file_poller();
        }
        // 投影预热（PR-M1-5 冒烟修正）：lazy 连接的唯一触发点是工具执行，
        // 而工具要先投影才会被调用——不预热则全新 daemon 上模型首回合永远
        // 看不到 MCP 工具（鸡生蛋死锁）。fire-and-forget：逐 server 连接
        // + 缓存 tools/list + 置脏，不阻塞启动；未启用时内部 no-op。
        qaqh_mcp::prime_all_async();
        // daemon 进程的工具注册表（供 `skills.list_tools` 等查询；worker 各自
        // 独立 init_tools，本进程只提供注册表快照，不参与工具执行）。
        // 不带 subagent 注册器：设置页勾选的是子代理可用工具，spawn_subagent
        // 本身不属于子代理工具集。
        qaqh_workspace::runtime::init_tools("daemon", &[], vec![]);
        Self {
            registry: Arc::new(Mutex::new(AgentRegistry::new(sessions.clone()))),
            hub: std::sync::OnceLock::new(),
            workspace_state: Arc::new(Mutex::new(WorkspaceRuntimeState::default())),
            sessions,
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
        // D-3：关闭即终止该会话的取消标记。worker 优雅收尾可能已把它置假
        // （actor 线程 clear_cancel），但整项移除才能让 SESSION_CANCELS 与
        // 活跃会话同阶（种子频繁进出的 daemon 长期运行不无界增长）。
        qaqh_workspace::remove_session_cancel(seed);
        // 临时会话（子代理）用完即走：关闭后删除会话目录，磁盘零残留。
        // 目录已不存在（重复 close / 已被清理）时静默跳过，保持幂等。
        if self.sessions.is_ephemeral(seed) {
            match self.sessions.delete(seed) {
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
        self.release_seed_resident_state(seed);
        release_freed_heap_memory();
        Ok(())
    }

    /// D-1：会话关闭后的 per-seed 常驻内存清理（修复 IMAGE_REGISTRY 与 hub
    /// channels 的 per-seed 残留——诊断结论②③）。必须在 worker join 之后、
    /// 终态（Closed）发布之后调用，顺序不可换：join 前 reset/forget 会让
    /// 仍在运行的 worker 继续写回脏状态；终态发布前 forget 会让 publish
    /// 触发 lazy-load，把刚丢弃的状态原样重建回来，清理变成空操作。
    /// forget 之后该 seed 的 hub 态与 daemon 重启后的空态同构；磁盘索引
    /// 保留，下次访问走既有 lazy-load 重放路径（UI 历史不丢）。
    fn release_seed_resident_state(&self, seed: &str) {
        // 图片注册表按 seed 键控存 base64（read_image 的上传缓存）。resume
        // 路径（state/lifecycle.rs）会从持久化消息历史重建，关闭期清空
        // 不破坏 image_index 语义。
        qaqh_workspace::read_image::reset_images(seed);
        if let Some(hub) = self.hub.get() {
            hub.forget_seed(seed);
        }
    }

    /// E: idle 卸载空闲会话 worker（docs/memory-governance-plan.md §E）。
    /// `idle_secs` <= 0 时为 no-op（配置禁用）。对每个被卸载的 seed 发布
    /// `SessionStateChanged::Closed`（与手动 close_session 一致，UI 可感知）。
    /// 返回被卸载的 seed 列表。registry.close 是阻塞 join——调用方
    /// （daemon 周期任务）必须置于 spawn_blocking。
    pub fn unload_idle_sessions(&self, idle_secs: u64) -> Vec<String> {
        if idle_secs == 0 {
            return Vec::new();
        }
        let Ok(mut registry) = self.registry() else {
            return Vec::new();
        };
        let unloaded = registry.unload_idle_sessions(idle_secs);
        if let Some(hub) = self.hub.get() {
            for seed in &unloaded {
                let _ = hub.publish_with_causation(
                    seed,
                    qaqh_domain::DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionStateChanged {
                            seed: seed.to_string(),
                            state: qaqh_domain::SessionState::Closed,
                        },
                    ),
                    None,
                );
            }
        }
        for seed in &unloaded {
            self.release_seed_resident_state(seed);
        }
        if !unloaded.is_empty() {
            release_freed_heap_memory();
        }
        unloaded
    }

    /// 归档会话（标签 × 语义）：关闭 registry 实例 + meta `archived=true`。
    /// 磁盘与消息文件保留，左侧列表归档组可见可恢复。会话不存在同样
    /// 幂等成功（close 幂等 + set_archived 补写 meta）。
    pub fn archive_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.close_session(seed, causation_id)?;
        self.sessions.set_archived(seed, true);
        Ok(())
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, seed: &str) -> Result<(), String> {
        self.sessions.set_archived(seed, false);
        self.registry()?.get_or_spawn(seed)
    }

    /// 彻底删除会话（左侧列表 × 语义）：先关实例（若运行，幂等）再删
    /// 磁盘目录与索引。会话不存在返回 Err（由 daemon 拦截层按幂等处理）。
    pub fn delete_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        let _ = self.close_session(seed, causation_id);
        self.sessions.delete(seed)
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
                let existing = self.sessions.list();
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
                let manager = &self.sessions;
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
                self.sessions.clear_active();
                // 可选 cwd（前端在 workspace 上下文新建时传入）→ 记录 + 自动归属。
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                self.sessions
                    .persist_new_session_with_cwd(&seed, cwd.as_deref());
                // 先于 spawn 落盘：worker 的 init_session 从 meta 恢复并应用，
                // 保证 minimal:dsh 的极简 system prompt 首轮就生效。
                if let Some((tool_mode, custom_tools)) = preset {
                    self.sessions
                        .persist_tool_mode(&seed, &tool_mode, &custom_tools)
                        .map_err(|error| format!("persist tool_mode failed: {error}"))?;
                }
                self.registry()?.spawn_new(&seed)?;
                Ok(json!(seed))
            }
            "session.resume" => {
                let seed = seed()?;
                self.sessions.set_active_seed(&seed);
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
                self.sessions
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
            "session.get_activity" => activity(&self.sessions, &seed()?),
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
            "workspace.get" => Ok(json!(workspace(&self.sessions, &seed()?))),
            "workspace.set" => {
                let seed = seed()?;
                // 空 path 防护：canonical_cwd("") = "" 会把 meta.cwd 清空，
                // 导致会话工作区/归属丢失（前端重启后回空 cwd 的 bug 通道）。
                let path = pstr(params, "path")?.trim().to_string();
                if path.is_empty() {
                    return Err("workspace.set: empty path rejected".into());
                }
                // 统一数据源：运行环境工作目录存 meta.cwd（workspace.txt 退役）。
                self.sessions.set_cwd(&seed, &path, true);
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::AgentReloadConfig),
                )?;
                Ok(Value::Null)
            }
            "git.diff" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::status_json,
                json!([]),
            ),
            "git.branch" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::current_branch,
                Value::Null,
            ),
            "git.branches" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::list_branches,
                json!([]),
            ),
            "git.switch_branch" => git(
                &self.sessions,
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
                &self.sessions,
                &seed()?,
                |ws| qaqh_workspace::git::commit_all(ws, &pstr(params, "message")?),
                Value::Null,
            ),
            "git.file_diff" => git(
                &self.sessions,
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
            "todo.set" => parse_json_string(qaqh_workspace::todo::todo_set_for(&seed()?, params)?),
            "todo.list" => {
                parse_json_string(qaqh_workspace::todo::todo_list_for(&seed()?, params)?)
            }
            "plan.context_stats" => context_stats(&self.sessions, &seed()?),
            "stats.token_usage" => token_stats(pu64(params, "days") as u32),
            "plan.read" => read_plan(&self.sessions, &seed()?),
            "plan.action" => {
                plan_action(
                    &self.sessions,
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
                    self.sessions.set_cwd(&seed, workspace, false);
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
        self.activity_snapshot().0
    }

    /// 只读活动快照（/activity 观测端点，冻结事故 P0）：逐会话活动状态 +
    /// 是否有活跃工作。单次加锁保证 flag 与列表一致；僵尸会话（如
    /// 2026-09-02 的 running 冻结）可直接从外部探测，不再依赖人肉轮询。
    pub fn activity_snapshot(&self) -> (bool, Vec<qaqh_domain::SessionActivity>) {
        let activities = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities();
        let has_active_work = activities.iter().any(|activity| {
            matches!(
                activity.state,
                ActivityState::Starting | ActivityState::Working | ActivityState::WaitingUser
            )
        });
        (has_active_work, activities)
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
        let manager = &self.sessions;
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
        // P2-D2：除 worker 广播外，向 Control 频道发布 ConfigChanged（空 seed =
        // 全局），前端/TUI/web 订阅后重拉 config.load——轮询降级为兜底。
        if let Some(hub) = self.hub.get() {
            let rev = qaqh_config::watch::latest().map_or(0, |_| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            });
            let _ = hub.publish_with_causation(
                "",
                qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::ConfigChanged { rev }),
                None,
            );
        }
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
        // P1-C2：wire 切换为 Merge Patch 契约（qaqh-config-api）——值域校验在
        // api 层 validate()；逐字段守卫语义集中在 qaqh_config::dto::apply_patch
        // （穷举映射，新增字段未同步会编译失败）。旧 snake/camel 双键 shim 就此退役。
        let patch: qaqh_config_api::ConfigPatch = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid config.save payload: {e}"))?;
        self.update_config_and_reload(|cfg| qaqh_config::dto::apply_patch(cfg, &patch))
            .map(|_| ())
    }
}

/// P2-1：MCP 热重载器——订阅 watch 单写口广播，只对 `[mcp]` 段变化响应。
///
/// 链路：`Config::update`（webUI/CLI）或文件轮询发布 → `watch::subscribe`
/// 唤醒 → 新配置的 [mcp] 与 manager 当前配置不等 → merge_external 重扫
/// （外部 Codex/Claude 用户级文件的变化在此一并捕获）→ `apply_config`
/// diff 热更新 → 投影置脏（下一回合投影批次自动重建）。日志记录 diff 报告。
fn spawn_mcp_reloader() {
    let mut rx = qaqh_config::watch::subscribe();
    tokio::spawn(async move {
        // 首个广播是启动期快照（manager 已按它装配）——消费掉，避免空转 apply。
        if rx.changed().await.is_ok() {
            rx.borrow_and_update();
        }
        while rx.changed().await.is_ok() {
            let Some(published) = rx.borrow_and_update().clone() else {
                continue;
            };
            let manager = qaqh_mcp::manager_slot();
            if published.mcp == manager.config() {
                continue; // 非 [mcp] 段变更：与 MCP 无关，跳过
            }
            let mut new_mcp = published.mcp.clone();
            // 外部配置重扫：ext-* 条目随用户级外部文件变化增删；本地
            // 手写面优先的碰撞语义与启动路径一致。
            let paths = qaqh_config::mcp_import::default_user_paths();
            let _ = qaqh_config::mcp_import::merge_external(&mut new_mcp, &paths);
            let report = manager.apply_config(new_mcp).await;
            log::info!(
                "[mcp] hot-reload applied (added={:?} updated={:?} removed={:?} kept={} conn)",
                report.added,
                report.updated,
                report.removed,
                report.kept.len()
            );
            // P2-1 缺口修复（用户实测发现）：热重载新增/变更的 server 无预热
            // 触发点——启动 prime 在 enabled=false 时是 no-op 且 reloader 不在
            // 启动路径，鸡生蛋重现（模型面永远看不到新 server 工具）。
            // apply 后补发幂等预热（已连接 skip；新增连接+缓存+置脏 → 下回合
            // 投影可见）。
            if !report.added.is_empty() || !report.updated.is_empty() {
                qaqh_mcp::prime_all_async();
            }
        }
        log::warn!("[mcp] hot-reload watcher channel closed; exiting");
    });
}

/// P2-1 同款：`[lsp]` 热重载——新配置与 manager 当前配置不等 →
/// `apply_config` diff 保连。LSP 无外部源重扫（只认手写面）。
fn spawn_lsp_reloader() {
    let mut rx = qaqh_config::watch::subscribe();
    tokio::spawn(async move {
        if rx.changed().await.is_ok() {
            rx.borrow_and_update();
        }
        while rx.changed().await.is_ok() {
            let Some(published) = rx.borrow_and_update().clone() else {
                continue;
            };
            let manager = qaqh_lsp::manager_slot();
            if published.lsp == manager.config() {
                continue; // 非 [lsp] 段变更：与 LSP 无关，跳过
            }
            let report = manager.apply_config(published.lsp.clone()).await;
            log::info!(
                "[lsp] hot-reload applied (added={:?} updated={:?} removed={:?} kept={} conn)",
                report.added,
                report.updated,
                report.removed,
                report.kept.len()
            );
        }
        log::warn!("[lsp] hot-reload watcher channel closed; exiting");
    });
}

/// P2-1：config.toml 文件轮询器——手改文件（不经 `Config::update` 单写口）
/// 也能触发热重载。
///
/// 策略：mtime 检测（1.5s 周期）→ 变化即 `reload_from_disk()`（内部：
/// 解析失败——编辑器写一半——静默跳过，下轮再试；成功则统一发布，
/// 重载器随之生效）。轮询而非 inotify：无新依赖，低频配置变更场景足够。
fn spawn_config_file_poller() {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
    tokio::spawn(async move {
        let path = qaqh_types::ConfigStore::default_location()
            .path()
            .to_path_buf();
        let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if mtime.is_some() && mtime != last_mtime {
                last_mtime = mtime;
                if qaqh_config::watch::reload_from_disk() {
                    log::info!(
                        "[config] file change detected ({}); reloaded",
                        path.display()
                    );
                }
            } else if mtime.is_none() && last_mtime.is_some() {
                // 文件被删除：记录状态，不发布（等待恢复或编辑器原子替换）。
                log::warn!("[config] config file missing ({}); waiting", path.display());
                last_mtime = None;
            }
        }
    });
}

pub(crate) mod common;
pub(crate) mod fs_git;
pub(crate) mod params;
pub(crate) mod plan;
pub(crate) mod stats;

use self::common::{command_id, err, parse_json_string, release_freed_heap_memory};
use self::fs_git::{git, list_remote_directory, read_remote_file, workspace};
use self::params::{
    optional_tool_mode, pbool, pstr, pstr2, pstrings, pu64, validate_tool_mode, value2,
};
use self::plan::{plan_action, read_plan, token_stats};
use self::stats::{activity, context_stats, dashboard, load_config};

#[cfg(test)]
mod tool_mode_tests {
    use super::params::{optional_tool_mode, validate_tool_mode};

    #[test]
    fn optional_tool_mode_defaults_to_none() {
        assert_eq!(optional_tool_mode(&serde_json::json!({})).unwrap(), None);
    }

    #[test]
    fn optional_tool_mode_rejects_deprecated_minimal_dsh() {
        // minimal:dsh 已随 bash/pwsh 拆分下线：废弃模式必须被 KNOWN_MODES
        // 白名单拒绝，不得回流。
        assert!(
            optional_tool_mode(&serde_json::json!({
                "tool_mode": "minimal:dsh",
            }))
            .is_err()
        );
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
            "custom_tools": ["exec"],
        }))
        .unwrap()
        .unwrap();
        assert_eq!(mode, "custom");
        assert_eq!(tools, vec!["exec"]);
    }

    #[test]
    fn validate_tool_mode_accepts_all_known_presets() {
        for mode in qaqh_types::KNOWN_MODES {
            assert!(validate_tool_mode(mode).is_ok(), "{mode}");
        }
        assert!(validate_tool_mode("turbo").is_err());
    }
}
