use crate::secrets::{CONFIG_MARKER, SecretSlot, SecretStore};
use qaqh_types::{
    ConfigStore, PersistentConfig, PersistentMcpConfig, PersistentMcpServerConfig,
    PersistentSubagentConfig, PersistentWorkspaceConfig,
};
use std::collections::HashMap; // still used by profiles
use std::sync::{Mutex, OnceLock};

/// Subagent default configuration.
///
/// These are defaults applied when spawning sub-agents. Individual
/// `spawn_subagent` tool calls can override these on a per-instance basis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentConfig {
    /// Override model. Empty = inherit from parent agent config.
    #[serde(default)]
    pub model: String,
    /// Override API base URL. Empty = inherit.
    #[serde(default)]
    pub base_url: String,
    /// Override API key. Empty = inherit.
    #[serde(default)]
    pub api_key: String,
    /// Max output tokens for subagent responses. Default: 4096.
    #[serde(default = "default_subagent_max_tokens")]
    pub max_tokens: u32,
    /// Maximum lifetime in seconds before the subagent is killed. Default: 120.
    #[serde(default = "default_subagent_timeout")]
    pub timeout_secs: u64,
    /// Default tool allowlist. Empty = all tools available.
    #[serde(default)]
    pub default_tools: Vec<String>,
}

fn default_subagent_max_tokens() -> u32 {
    4096
}
fn default_subagent_timeout() -> u64 {
    120
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 4096,
            timeout_secs: 120,
            default_tools: vec!["read".into(), "exec".into()],
        }
    }
}

/// Runtime agent configuration built from PersistentConfig + registry.
///
/// This is the fully-resolved config used by the agent at runtime. It combines
/// user settings from config.toml with provider registry defaults and profile
/// overrides. All fields are concrete (no Option wrapping).
#[derive(Debug, Clone)]
pub struct Config {
    /// API key for the selected provider.
    pub api_key: String,
    /// Base URL for API requests (from provider registry).
    pub base_url: String,
    /// Active model identifier.
    pub model: String,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// Maximum context window size in tokens.
    pub context_limit: u32,
    /// Selected provider ID (e.g. "deepseek", "qwen").
    pub provider_id: String,
    /// Selected endpoint within the provider (e.g. "openai").
    pub endpoint: String,
    /// Reasoning effort: one of `low|medium|high|xhigh|max`, or empty (provider default).
    pub reasoning_effort: String,
    /// Named profiles for quick config switching.
    pub profiles: HashMap<String, qaqh_types::ProfileConfig>,
    /// Currently active profile name.
    pub active_profile: String,
    /// UI language preference.
    pub lang: Option<String>,
    /// UI font family（WinUI 壳全局字体；空 = 跟随系统默认）。
    pub font_family: String,
    /// UI 主题偏好：`system` | `light` | `dark` | `dark-gray`；`None`/空 = 跟随系统。
    pub theme: Option<String>,
    /// 桌面通知开关。`None` = 开启（缺省）。
    pub notifications_enabled: Option<bool>,
    /// Default configuration for sub-agent spawning.
    pub subagent: SubagentConfig,
    /// Whether the content filter is active.
    pub compliance_enabled: bool,
    /// Additional banned keywords for the content filter.
    pub compliance_extra_keywords: Vec<String>,
    /// Whitelisted patterns exempt from content filtering.
    pub compliance_allowlist: Vec<String>,
    /// Agent permission level:
    /// 1 = MaxLockdown, 2 = ReadFree, 3 = WorkspaceFree, 4 = Unrestricted.
    pub permission_level: u8,
    /// Path to a HuggingFace tokenizer.json. `None` = use heuristic fallback.
    pub tokenizer_path: Option<String>,
    /// Auto-compact threshold: fraction of context_limit (0.0-1.0).
    /// When total tokens exceed `context_limit * threshold`, compact is
    /// triggered before the next user message is processed. 0.0 disables.
    /// Default: 0.75 (compact at 75% capacity).
    pub auto_compact_threshold: f64,
    /// 空闲会话 worker 自动卸载阈值（秒）。0 = 禁用（缺省）。
    /// daemon 周期任务消费；运行时 Config 承载该值以保证 save 往返不丢
    /// 用户手写配置（save_with 从运行时 Config 全量重构 PersistentConfig）。
    pub session_idle_unload_secs: u64,
    /// 工具套件运行环境："local"（默认）| "wsl"（仅 Windows）。
    pub workspace: WorkspaceConfig,
    /// MCP 客户端配置（docs/mcp-client-design.md §6；load 时已 fail-fast 校验）。
    pub mcp: McpConfig,
}

/// 工具套件运行环境（daemon 据此拉起 qaqh-workspace serve）。
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub mode: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            mode: "local".into(),
        }
    }
}

// ── MCP 客户端配置（docs/mcp-client-design.md §6）──

/// MCP server 传输形态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum McpTransportKind {
    /// stdio 子进程（Phase 1 主形态；设计 D1/S1）。
    Stdio,
    /// streamable HTTP（M3）。
    Http,
}

/// 单个 MCP server 的运行时配置（已通过 fail-fast 校验）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub transport: McpTransportKind,
    /// stdio 启动命令（如 "npx"）；http 时为空。
    pub command: String,
    pub args: Vec<String>,
    /// 注入 server 进程的环境变量原值；`${secret:name}` 占位符由 qaqh-mcp
    /// 在启动时解析（qaqh-config 不做 secret 求值）。
    pub env: std::collections::BTreeMap<String, String>,
    /// streamable HTTP endpoint；stdio 时为空。
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
    /// 工具白名单（server 侧原始名，投影时加 `mcp__{server}__` 前缀）；None = 全部。
    pub tools: Option<Vec<String>>,
    pub resources_enabled: bool,
    /// 单次工具调用默认超时（秒）；1..=3600。
    pub default_timeout_secs: u64,
    /// per-server 并发上限；1..=64。
    pub max_concurrent_calls: u32,
}

/// MCP 客户端运行时配置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpConfig {
    pub enabled: bool,
    /// idle 回收阈值（秒）；0 = 常驻不回收。判定口径：`inflight == 0` 连续该时长。
    pub idle_shutdown_secs: u64,
    /// server 名（已校验 `[a-z0-9_-]+`）→ 配置。
    pub servers: std::collections::BTreeMap<String, McpServerConfig>,
    /// PR-M3-2：用户级外部 MCP 配置只读合并（Codex/Claude Code，A 路线）。
    /// 默认开启；关闭后仅认 [mcp.servers] 手写面。
    #[serde(default = "default_import_external")]
    pub import_external: bool,
}

fn default_import_external() -> bool {
    true
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_shutdown_secs: 300,
            servers: std::collections::BTreeMap::new(),
            import_external: default_import_external(),
        }
    }
}

/// PersistentMcpConfig → 运行时 [`McpConfig`]，含 fail-fast 校验。
///
/// 校验规则（设计 §6 + PLAN PR-M1-1）：
/// - server 名非空、仅 `[a-z0-9_-]`、≤64 字符；
/// - stdio/http 互斥：stdio 必须有非空 command 且无 url；http 必须有
///   `http(s)://` url 且无 command；
/// - tools 白名单条目非空；
/// - `default_timeout_secs` ∈ 1..=3600（越界即错，不做静默 clamp——静默修正
///   会掩盖配置错误）；
/// - `max_concurrent_calls` ∈ 1..=64。
///
/// 注意：TOML 层面的重复表（`[mcp.servers.x]` 写两次）由 toml 解析器拒绝，
/// 走 `ConfigStore::load` 返回 None → 整体回退默认的既有语义（见 PLAN §8）。
pub(crate) fn map_mcp_config(
    mcp: Option<qaqh_types::PersistentMcpConfig>,
) -> Result<McpConfig, String> {
    let Some(mcp) = mcp else {
        return Ok(McpConfig::default());
    };
    let mut servers = std::collections::BTreeMap::new();
    for (name, ps) in mcp.servers.unwrap_or_default() {
        validate_server_name(&name)?;
        let command = ps.command.unwrap_or_default();
        let url = ps.url.unwrap_or_default();
        let transport = match (command.trim().is_empty(), url.trim().is_empty()) {
            (false, true) => McpTransportKind::Stdio,
            (true, false) => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!(
                        "[mcp] server {name:?}: url 必须以 http:// 或 https:// 开头"
                    ));
                }
                McpTransportKind::Http
            }
            (false, false) => {
                return Err(format!(
                    "[mcp] server {name:?}: command 与 url 互斥，只能配置其一"
                ));
            }
            (true, true) => {
                return Err(format!(
                    "[mcp] server {name:?}: 缺少 transport 配置——stdio 需要 command，http 需要 url"
                ));
            }
        };
        if let Some(tools) = &ps.tools
            && tools.iter().any(|t| t.trim().is_empty())
        {
            return Err(format!("[mcp] server {name:?}: tools 白名单包含空条目"));
        }
        let default_timeout_secs = match ps.default_timeout_secs {
            None => 60,
            Some(0) => {
                return Err(format!(
                    "[mcp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 0）"
                ));
            }
            Some(n) if n > 3600 => {
                return Err(format!(
                    "[mcp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        let max_concurrent_calls = match ps.max_concurrent_calls {
            None => 1,
            Some(0) => {
                return Err(format!(
                    "[mcp] server {name:?}: max_concurrent_calls 必须在 1..=64（得到 0）"
                ));
            }
            Some(n) if n > 64 => {
                return Err(format!(
                    "[mcp] server {name:?}: max_concurrent_calls 必须在 1..=64（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        servers.insert(
            name,
            McpServerConfig {
                transport,
                command,
                args: ps.args.unwrap_or_default(),
                env: ps.env.unwrap_or_default().into_iter().collect(),
                url,
                headers: ps.headers.unwrap_or_default().into_iter().collect(),
                tools: ps
                    .tools
                    .map(|tools| tools.into_iter().map(|t| t.trim().to_owned()).collect()),
                resources_enabled: ps.resources.unwrap_or(true),
                default_timeout_secs,
                max_concurrent_calls,
            },
        );
    }
    Ok(McpConfig {
        enabled: mcp.enabled.unwrap_or(!servers.is_empty()),
        idle_shutdown_secs: mcp.idle_shutdown_secs.unwrap_or(300),
        servers,
        import_external: mcp.import_external.unwrap_or(true),
    })
}

// ── `${secret:name}` 占位符（设计 §6/E-4）──
//
// 职责边界（M1-3 定稿）：
// - **load 时只校验不解析**：扫出占位符名 → `SecretStore::has_mcp`，缺失即
//   Err（fail-fast，防拼写错拖到首次调用才爆）；`Config.mcp` 中的 env/args
//   /headers 始终保留占位符原值 —— DTO（webUI）与 save 回写永不接触明文；
// - **解析在 qaqh-mcp 连接时**（子进程 env 组装前），见 adapter.rs；
// - 名字字符集与 server 名同规（`[a-z0-9_-]`），残缺/非法占位符一律 fail-fast。

/// 扫描 `value` 中的 `${secret:name}` 占位符，返回按出现顺序去重的名字。
/// 残缺（未闭合）/空名/非法字符 → Err（fail-fast）。
pub fn secret_placeholder_names(value: &str) -> Result<Vec<String>, String> {
    const PREFIX: &str = "${secret:";
    let mut names = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(PREFIX) {
        // 工作区红线 string_slice=deny：一律用 `get`（Option，无 panic 面）。
        // PREFIX 为 ASCII，start+PREFIX.len() 必在字符边界；防御式处理仍保留。
        let Some(after) = rest.get(start + PREFIX.len()..) else {
            return Err(format!("占位符位置非字符边界：{value:?}"));
        };
        let Some(end) = after.find('}') else {
            return Err(format!("占位符未闭合：{value:?} 中的 {PREFIX}…"));
        };
        let Some(name) = after.get(..end) else {
            return Err(format!("占位符名位置非字符边界：{value:?}"));
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "占位符名非法（仅允许 [a-z0-9_-]）：{value:?} 中的 {PREFIX}{name}}}"
            ));
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
        let Some(tail) = after.get(end + 1..) else {
            return Err(format!("占位符尾部非字符边界：{value:?}"));
        };
        rest = tail;
    }
    Ok(names)
}

/// 把 `value` 中所有 `${secret:name}` 替换为 `resolve(name)` 的结果。
/// 解析失败（未注册/解密失败）→ Err，报错只含名字不含值（E-6 红线）。
/// 由 qaqh-mcp 在连接时调用；qaqh-config 的 load/save 不调用本函数。
pub fn interpolate_secret_placeholders<F>(value: &str, resolve: F) -> Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let names = secret_placeholder_names(value)?;
    if names.is_empty() {
        return Ok(value.to_owned());
    }
    let mut resolved = std::collections::BTreeMap::new();
    for name in &names {
        let Some(value) = resolve(name) else {
            return Err(format!("secret {name:?} 不可用（未注册或解密失败）"));
        };
        resolved.insert(name.clone(), value);
    }
    let mut out = value.to_owned();
    for (name, secret) in &resolved {
        let placeholder = format!("${{secret:{name}}}");
        out = out.replace(&placeholder, secret);
    }
    Ok(out)
}

/// 启动校验：扫 server 配置里所有可能携带占位符的字符串字段，
/// 引用的 secret 名必须在 store 中已注册（E-4：缺 key 不静默）。
fn validate_mcp_secret_refs(mcp: &McpConfig, secrets: &SecretStore) -> Result<(), String> {
    for (name, server) in &mcp.servers {
        let available = || secrets.list_mcp();
        let check = |field: &str, value: &str| -> Result<(), String> {
            for secret_name in secret_placeholder_names(value)? {
                if !secrets.has_mcp(&secret_name) {
                    return Err(format!(
                        "[mcp] server {name:?} {field}: 引用的 secret {secret_name:?} 未注册；已注册：{:?}",
                        available()
                    ));
                }
            }
            Ok(())
        };
        for (key, value) in &server.env {
            check(&format!("env[{key}]"), value)?;
        }
        for (key, value) in &server.headers {
            check(&format!("headers[{key}]"), value)?;
        }
        for (index, arg) in server.args.iter().enumerate() {
            check(&format!("args[{index}]"), arg)?;
        }
    }
    Ok(())
}

/// server 名校验：非空、仅 `[a-z0-9_-]`、≤64 字符。
fn validate_server_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("[mcp] server 名长度必须 1..=64（得到 {name:?}）"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "[mcp] server 名 {name:?} 含非法字符：仅允许小写字母、数字、'_'、'-'"
        ));
    }
    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        let (provider_id, endpoint) = crate::registry::first_provider_endpoint();
        let base_url = crate::registry::base_url_for(&provider_id, &endpoint);
        let model = crate::registry::default_model_for(&provider_id, &endpoint);

        let mut profiles = HashMap::new();
        profiles.insert(
            "default".into(),
            qaqh_types::ProfileConfig {
                model: model.clone(),
                max_tokens: 16384,
                effort: Some("high".into()),
                context_limit: 1_000_000,
                base_url: base_url.clone(),
                endpoint: None,
            },
        );
        Self {
            api_key: String::new(),
            base_url,
            model,
            max_tokens: 16384,
            context_limit: 1_000_000,
            provider_id,
            endpoint,
            reasoning_effort: "high".into(),
            profiles,
            active_profile: "default".into(),
            lang: None,
            font_family: String::new(),
            theme: None,
            notifications_enabled: None,
            subagent: SubagentConfig::default(),
            compliance_enabled: true,
            compliance_extra_keywords: Vec::new(),
            compliance_allowlist: Vec::new(),
            permission_level: 4, // Unrestricted — backward compat
            tokenizer_path: None,
            auto_compact_threshold: 0.75,
            session_idle_unload_secs: 0,
            workspace: WorkspaceConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

impl Config {
    /// Serializes all process-local config read-modify-write cycles.
    ///
    /// `ConfigStore` already uses atomic rename for corruption safety, but
    /// concurrent daemon actions (config.save + set_permission_level + profile
    /// actions) used to perform independent load→mutate→save transactions and
    /// could overwrite each other. Every public load/save/update now goes
    /// through this lock.
    fn config_io_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Load config from disk (TOML primary store).
    pub fn load() -> Result<Self, String> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        Self::load_unlocked()
    }

    fn load_unlocked() -> Result<Self, String> {
        let store = ConfigStore::default_location();
        Self::load_from_paths_with(store, SecretStore::default_location())
    }

    /// The single config write port (BUG-008 / roadmap 刀6).
    ///
    /// Loads the current config, applies `mutate`, and persists it in one
    /// locked transaction. If `mutate` returns `Err`, nothing is written and
    /// the error is propagated. Daemon actions must mutate config exclusively
    /// through this method instead of `load()` + direct field writes + `save()`.
    pub fn update<F>(mutate: F) -> Result<Self, String>
    where
        F: FnOnce(&mut Self) -> Result<(), String>,
    {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        let mut config = Self::load_unlocked()?;
        mutate(&mut config)?;
        config.save_unlocked()?;
        // P2-D1：磁盘先落盘，再广播内存快照（消费者永远读到已持久化状态）。
        crate::watch::publish(std::sync::Arc::new(config.clone()));
        Ok(config)
    }

    /// 从显式路径加载（公开给集成测试与多实例/数据根重定向场景；
    /// 生产路径走 [`Config::load`] 的全局 platform 路径）。
    pub fn load_from_paths_with(store: ConfigStore, secrets: SecretStore) -> Result<Self, String> {
        let mut cfg = Self::default();

        let pc = store.load();

        let mut needs_rewrite = false;
        // 审计 P0-1：API key 不落 config.toml 明文。
        // - 标记 "set" → 从 secrets.toml 解密（失败 = 无 key，绝不回退旧明文）；
        // - 旧明文（升级前配置）→ 迁移入 secrets.toml，config.toml 写回标记；
        // - None/空 → 未配置。

        if let Some(mut pc) = pc {
            // P1-C3：扁平模型族六字段退役检测——存在即触发一次性迁移写回
            // （值固化进 profile 后从顶层剥离，见下方 needs_rewrite 块）。
            let legacy_flat_fields = pc.model.is_some()
                || pc.base_url.is_some()
                || pc.max_tokens.is_some()
                || pc.context_limit.is_some()
                || pc.endpoint.is_some()
                || pc.reasoning_effort.is_some();
            needs_rewrite |= legacy_flat_fields;
            // ── Backward compat: migrate old provider_id → new (provider_id, endpoint) ──
            let raw_pid = pc.provider_id.unwrap_or_default();
            let (provider_id, endpoint) = if raw_pid.is_empty() {
                crate::registry::first_provider_endpoint()
            } else {
                crate::registry::migrate_provider_id(&raw_pid)
            };
            cfg.provider_id = provider_id;
            // New endpoint field takes priority over backward-compat migration
            cfg.endpoint = pc.endpoint.filter(|e| !e.is_empty()).unwrap_or(endpoint);

            // ── Resolve base_url from endpoint ──
            // 预设仅作空值兜底：仅当配置文件中未保存 base_url（空文件/旧版无此字段）
            // 时才用 endpoint 预设；用户已保存的值（含自定义 URL）绝不在此覆盖，
            // 由下方 pc.base_url 权威回填。修复：改 max_tokens 后端点被强制改回预设。
            if pc.base_url.as_deref().is_none_or(|u| u.is_empty()) {
                let endpoint_base_url =
                    crate::registry::base_url_for(&cfg.provider_id, &cfg.endpoint);
                if !endpoint_base_url.is_empty() {
                    cfg.base_url = endpoint_base_url.clone();
                }
            }

            if let Some(profiles) = pc.profiles {
                cfg.profiles = profiles;
            }
            if let Some(ref active) = pc.active_profile {
                cfg.active_profile = active.clone();
                if let Some(profile) = cfg.profiles.get(active) {
                    cfg.model = profile.model.clone();
                    cfg.max_tokens = profile.max_tokens;
                    cfg.reasoning_effort = profile.effort.clone().unwrap_or_else(|| "high".into());
                    cfg.context_limit = profile.context_limit;
                    cfg.base_url = profile.base_url.clone();
                    if let Some(ref ep) = profile.endpoint
                        && !ep.is_empty()
                    {
                        cfg.endpoint = ep.clone();
                        // 仅 profile 未配置 base_url（空值）时回退到 endpoint 预设；
                        // profile 已保存的值（含自定义 URL）绝不覆盖。
                        let ep_burl = crate::registry::base_url_for(&cfg.provider_id, ep);
                        if cfg.base_url.is_empty() && !ep_burl.is_empty() {
                            cfg.base_url = ep_burl;
                        }
                    }
                }
            }
            if let Some(k) = pc.api_key
                && !k.is_empty()
            {
                if k == CONFIG_MARKER {
                    cfg.api_key = secrets.load(SecretSlot::Main).unwrap_or_default();
                } else {
                    match secrets.set(SecretSlot::Main, &k) {
                        Ok(()) => needs_rewrite = true,
                        Err(e) => log::warn!(
                            "[config] migrate main api key to secret store failed: {e}; keeping plaintext until retry"
                        ),
                    }
                    cfg.api_key = k;
                }
            }
            if let Some(m) = pc.model
                && !m.is_empty()
            {
                cfg.model = m;
            }
            // User base_url override: 用户显式保存的值（含自定义 URL）无条件优先。
            // 预设仅在配置文件为空（base_url 缺失）时兜底，用户修改后不再预设。
            if let Some(ref u) = pc.base_url
                && !u.is_empty()
            {
                cfg.base_url = u.clone();
            }
            if let Some(mt) = pc.max_tokens {
                cfg.max_tokens = mt;
            }
            if let Some(cl) = pc.context_limit {
                cfg.context_limit = cl;
            }
            if let Some(ref l) = pc.lang
                && !l.is_empty()
            {
                cfg.lang = Some(l.clone());
            }
            // ── UI 字体（空 = 跟随系统默认）──
            if let Some(ref f) = pc.font_family
                && !f.is_empty()
            {
                cfg.font_family = f.clone();
            }
            // ── UI 主题（空/缺失 = 跟随系统）──
            match pc.theme.as_deref() {
                Some(theme) if !theme.is_empty() => cfg.theme = Some(theme.to_string()),
                _ => cfg.theme = None,
            }
            // ── 桌面通知（缺失 = 开启）──
            if let Some(enabled) = pc.notifications_enabled {
                cfg.notifications_enabled = Some(enabled);
            }
            // ── Subagent defaults ──
            if let Some(ref mut s) = pc.subagent {
                if let Some(ref m) = s.model
                    && !m.is_empty()
                {
                    cfg.subagent.model = m.clone();
                }
                if let Some(ref u) = s.base_url
                    && !u.is_empty()
                {
                    cfg.subagent.base_url = u.clone();
                }
                if let Some(k) = s.api_key.clone()
                    && !k.is_empty()
                {
                    if k == CONFIG_MARKER {
                        cfg.subagent.api_key =
                            secrets.load(SecretSlot::Subagent).unwrap_or_default();
                    } else {
                        match secrets.set(SecretSlot::Subagent, &k) {
                            Ok(()) => needs_rewrite = true,
                            Err(e) => log::warn!(
                                "[config] migrate subagent api key to secret store failed: {e}; keeping plaintext until retry"
                            ),
                        }
                        cfg.subagent.api_key = k;
                    }
                }
                if let Some(mt) = s.max_tokens {
                    cfg.subagent.max_tokens = mt;
                }
                if let Some(ts) = s.timeout_secs {
                    cfg.subagent.timeout_secs = ts;
                }
                if let Some(ref tools) = s.default_tools {
                    // 存量迁移：工具改名后（file→read、read_file→read、
                    // edit_file_v2→edit）旧配置持久化的
                    // 默认白名单仍指向旧名；加载时归一化为当前正式词汇表，
                    // 否则 apply_init 会静默剔除旧名，子代理丢失读文件能力。
                    cfg.subagent.default_tools = tools
                        .iter()
                        .map(|t| match t.as_str() {
                            "file" | "read_file" => "read",
                            "edit_file_v2" => "edit",
                            _ => t,
                        })
                        .map(String::from)
                        .collect();
                }
            }

            // ── Compliance ──
            if let Some(enabled) = pc.compliance_enabled {
                cfg.compliance_enabled = enabled;
            }
            if let Some(ref keywords) = pc.compliance_extra_keywords {
                cfg.compliance_extra_keywords = keywords.clone();
            }
            if let Some(ref allowlist) = pc.compliance_allowlist {
                cfg.compliance_allowlist = allowlist.clone();
            }

            // ── Multimodal (vision) ──
            // 2026-08 移除：外挂视觉模型配置已废弃（read_image 工具直接走
            // 主模型视觉输入）。旧 config.toml 中的 [multimodal] 段被
            // serde 忽略；残留的 secrets.toml multimodal 槽位不再读取。

            // ── Permission ──
            if let Some(pl) = pc.permission_level {
                cfg.permission_level = pl;
            }

            // ── Tokenizer ──
            if let Some(ref tp) = pc.tokenizer_path {
                cfg.tokenizer_path = Some(tp.clone());
            }

            // ── Auto-compact ──
            if let Some(act) = pc.auto_compact_threshold {
                cfg.auto_compact_threshold = act;
            }

            // ── Idle unload（daemon 级会话策略；0 = 禁用）──
            if let Some(v) = pc.session_idle_unload_secs {
                cfg.session_idle_unload_secs = v;
            }

            // ── 工具套件运行环境 ──
            if let Some(ref ws) = pc.workspace
                && let Some(ref mode) = ws.mode
            {
                cfg.workspace.mode = mode.clone();
            }

            // ── MCP 客户端（fail-fast：非法名/互斥冲突/缺 command 直接 load 失败）──
            cfg.mcp = map_mcp_config(pc.mcp.clone())?;
            // E-4 fail-fast：`${secret:name}` 引用的名字必须在 secrets.toml
            // 已注册（拼错名启动即报，不拖到首次调用；值本身留在占位符，
            // DTO/save 永不见明文——解析在 qaqh-mcp 连接时）。
            validate_mcp_secret_refs(&cfg.mcp, &secrets)?;

            // 迁移写回：config.toml 中旧明文已入 secret store，把明文替换为
            // "set" 标记（重新 load 磁盘原值，只改"确为明文"的槽位——迁移
            // 失败的槽位保持明文，下次 load 重试；已标记/未配置的原样保留）。
            if needs_rewrite && let Some(mut fresh) = store.load() {
                let is_plain = |k: &Option<String>| {
                    k.as_deref()
                        .is_some_and(|v| !v.is_empty() && v != CONFIG_MARKER)
                };
                if is_plain(&fresh.api_key) {
                    fresh.api_key = Some(CONFIG_MARKER.to_owned());
                }
                if let Some(ref mut s) = fresh.subagent
                    && is_plain(&s.api_key)
                {
                    s.api_key = Some(CONFIG_MARKER.to_owned());
                }
                // C3 迁移：扁平值先固化进 active/default profile（已有条目
                // 不覆盖——profile 为权威），再剥离顶层键。fresh 无 profiles
                // 时就地建表；缺失的分量用合并结果 cfg 兜底，确保零丢失。
                if legacy_flat_fields {
                    let active = fresh
                        .active_profile
                        .clone()
                        .unwrap_or_else(|| "default".to_string());
                    // 先在 profiles 借用前固化兜底条目，避免可变借用重叠。
                    let fallback = qaqh_types::ProfileConfig {
                        model: fresh.model.clone().unwrap_or_else(|| cfg.model.clone()),
                        max_tokens: fresh.max_tokens.unwrap_or(cfg.max_tokens),
                        effort: Some(
                            fresh
                                .reasoning_effort
                                .clone()
                                .unwrap_or_else(|| cfg.reasoning_effort.clone()),
                        ),
                        context_limit: fresh.context_limit.unwrap_or(cfg.context_limit),
                        base_url: fresh
                            .base_url
                            .clone()
                            .unwrap_or_else(|| cfg.base_url.clone()),
                        endpoint: Some(
                            fresh
                                .endpoint
                                .clone()
                                .unwrap_or_else(|| cfg.endpoint.clone()),
                        ),
                    };
                    let profiles = fresh.profiles.get_or_insert_with(HashMap::new);
                    // 无条件以扁平值覆盖：历史读语义是"扁平胜出"，扁平即用户
                    // 最新意图；旧条目只可能是更早一次保存的陈值。
                    profiles.insert(active, fallback);
                    fresh.model = None;
                    fresh.base_url = None;
                    fresh.max_tokens = None;
                    fresh.context_limit = None;
                    fresh.endpoint = None;
                    fresh.reasoning_effort = None;
                }
                log::info!("[config] legacy flat model fields migrated into [profiles.*]");
                let _ = store.save(&fresh);
            }
        }

        if !cfg.profiles.contains_key("default") {
            cfg.profiles.insert(
                "default".into(),
                qaqh_types::ProfileConfig {
                    model: cfg.model.clone(),
                    max_tokens: cfg.max_tokens,
                    effort: Some(cfg.reasoning_effort.clone()),
                    context_limit: cfg.context_limit,
                    base_url: cfg.base_url.clone(),
                    endpoint: Some(cfg.endpoint.clone()),
                },
            );
        }

        // Initialize tokenizer if configured
        if let Some(ref path) = cfg.tokenizer_path {
            let _ = qaqh_types::token::init_tokenizer(path);
        }

        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        self.save_unlocked()
    }

    fn save_unlocked(&self) -> Result<(), String> {
        self.save_with(
            &ConfigStore::default_location(),
            &SecretStore::default_location(),
        )
    }

    /// 显式路径版 save（与 [`Self::load_from_paths_with`] 对称；集成测试与
    /// 多实例/数据根重定向场景使用）。
    pub fn save_with(&self, store: &ConfigStore, secrets: &SecretStore) -> Result<(), String> {
        // 审计 P0-1：凭据先入 secret store；失败则中止保存（config.toml 永不
        // 出现明文）。cfg.api_key 为空时删除对应 secret 槽位——2026-08 起
        // config.save 不再把空串当"删除"（空串/掩码 = 保持现值，防前端误发空
        // 清密钥），此处仅在配置本身被清空（如未来显式删除接口）时收尾删除。
        if self.api_key.is_empty() {
            let _ = secrets.delete(SecretSlot::Main);
        } else {
            secrets
                .set(SecretSlot::Main, &self.api_key)
                .map_err(|e| format!("failed to store main api key: {e}"))?;
        }
        if self.subagent.api_key.is_empty() {
            let _ = secrets.delete(SecretSlot::Subagent);
        } else {
            secrets
                .set(SecretSlot::Subagent, &self.subagent.api_key)
                .map_err(|e| format!("failed to store subagent api key: {e}"))?;
        }

        let mut profiles = self.profiles.clone();
        profiles.insert(
            self.active_profile.clone(),
            qaqh_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_limit: self.context_limit,
                base_url: self.base_url.clone(),
                endpoint: Some(self.endpoint.clone()),
            },
        );
        let pc = PersistentConfig {
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(CONFIG_MARKER.to_owned())
            },
            // P1-C3：扁平模型族字段退役——一律 None（serde skip_serializing），
            // 唯一持久化真相是上方 upsert 进 profiles[active] 的条目。
            model: None,
            base_url: None,
            max_tokens: None,
            context_limit: None,
            provider_id: Some(self.provider_id.clone()),
            endpoint: None,
            reasoning_effort: None,
            profiles: Some(profiles),
            active_profile: Some(self.active_profile.clone()),
            lang: self.lang.clone(),
            font_family: if self.font_family.is_empty() {
                None
            } else {
                Some(self.font_family.clone())
            },
            theme: self.theme.clone(),
            notifications_enabled: self.notifications_enabled,
            subagent: Some(PersistentSubagentConfig {
                model: if self.subagent.model.is_empty() {
                    None
                } else {
                    Some(self.subagent.model.clone())
                },
                base_url: if self.subagent.base_url.is_empty() {
                    None
                } else {
                    Some(self.subagent.base_url.clone())
                },
                api_key: if self.subagent.api_key.is_empty() {
                    None
                } else {
                    Some(CONFIG_MARKER.to_owned())
                },
                max_tokens: Some(self.subagent.max_tokens),
                timeout_secs: Some(self.subagent.timeout_secs),
                default_tools: if self.subagent.default_tools.is_empty() {
                    None
                } else {
                    Some(self.subagent.default_tools.clone())
                },
            }),
            compliance_enabled: Some(self.compliance_enabled),
            compliance_extra_keywords: if self.compliance_extra_keywords.is_empty() {
                None
            } else {
                Some(self.compliance_extra_keywords.clone())
            },
            compliance_allowlist: if self.compliance_allowlist.is_empty() {
                None
            } else {
                Some(self.compliance_allowlist.clone())
            },
            permission_level: Some(self.permission_level),
            tokenizer_path: self.tokenizer_path.clone(),
            auto_compact_threshold: Some(self.auto_compact_threshold),
            session_idle_unload_secs: (self.session_idle_unload_secs > 0)
                .then_some(self.session_idle_unload_secs),
            workspace: Some(PersistentWorkspaceConfig {
                mode: Some(self.workspace.mode.clone()),
            }),
            mcp: Some(PersistentMcpConfig {
                enabled: Some(self.mcp.enabled),
                import_external: Some(self.mcp.import_external),
                idle_shutdown_secs: (self.mcp.idle_shutdown_secs > 0)
                    .then_some(self.mcp.idle_shutdown_secs),
                servers: (!self.mcp.servers.is_empty()).then(|| {
                    self.mcp
                        .servers
                        .iter()
                        .map(|(name, s)| {
                            (
                                name.clone(),
                                PersistentMcpServerConfig {
                                    command: (!s.command.is_empty()).then(|| s.command.clone()),
                                    args: (!s.args.is_empty()).then(|| s.args.clone()),
                                    env: (!s.env.is_empty())
                                        .then(|| s.env.clone().into_iter().collect()),
                                    url: (!s.url.is_empty()).then(|| s.url.clone()),
                                    headers: (!s.headers.is_empty())
                                        .then(|| s.headers.clone().into_iter().collect()),
                                    tools: s.tools.clone(),
                                    resources: Some(s.resources_enabled),
                                    default_timeout_secs: Some(s.default_timeout_secs),
                                    max_concurrent_calls: Some(s.max_concurrent_calls),
                                },
                            )
                        })
                        .collect()
                }),
            }),
        };
        log::info!(
            "[Config::save] writing to {}",
            qaqh_types::platform::config_path().display()
        );
        if !store.save(&pc) {
            return Err(format!(
                "Failed to save config to {}",
                qaqh_types::platform::config_path().display()
            ));
        }

        Ok(())
    }

    /// Pure profile switch. Persistence is the caller's responsibility via
    /// [`Config::update`] — profile methods must not create another write port.
    pub fn apply_profile(&mut self, name: &str) -> Option<String> {
        let profile = self.profiles.get(name)?.clone();
        self.model = profile.model;
        self.max_tokens = profile.max_tokens;
        self.reasoning_effort = profile.effort.unwrap_or_else(|| "high".into());
        self.context_limit = profile.context_limit;
        self.base_url = profile.base_url;
        if let Some(ref ep) = profile.endpoint {
            self.endpoint = ep.clone();
            // 仅 profile 未配置 base_url（空值）时回退到 endpoint 预设；
            // 已保存的值（含自定义 URL）绝不覆盖（此前 `ep_burl != self.base_url`
            // 会把自定义 URL 强制改回预设并落盘——修改 max_tokens 后端点被重置的根因）。
            let ep_burl = crate::registry::base_url_for(&self.provider_id, ep);
            if self.base_url.is_empty() && !ep_burl.is_empty() {
                self.base_url = ep_burl;
            }
        }
        self.active_profile = name.to_string();
        Some(name.to_string())
    }

    pub fn save_profile(&mut self, name: &str) {
        self.profiles.insert(
            name.to_string(),
            qaqh_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_limit: self.context_limit,
                base_url: self.base_url.clone(),
                endpoint: Some(self.endpoint.clone()),
            },
        );
        self.active_profile = name.to_string();
    }

    pub fn delete_profile(&mut self, name: &str) -> bool {
        if name == "default" {
            return false;
        }
        self.profiles.remove(name).is_some()
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;
    use crate::secrets::{SecretSlot, SecretStore};
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "qaqh-config-secret-test-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn legacy_plaintext_migrates_to_secret_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");
        std::fs::write(
            &config_path,
            "provider_id = \"deepseek\"\napi_key = \"sk-legacy-secret\"\n",
        )
        .expect("write legacy config");

        let store = ConfigStore::new(config_path.clone());
        let secrets = SecretStore::new(secrets_path.clone());
        let cfg = Config::load_from_paths_with(store, secrets.clone()).expect("load");

        // 运行时拿到明文（内存），secrets 已迁移，config.toml 不再有明文。
        assert_eq!(cfg.api_key, "sk-legacy-secret");
        assert!(secrets.has(SecretSlot::Main));
        assert_eq!(
            secrets.load(SecretSlot::Main).as_deref(),
            Some("sk-legacy-secret")
        );
        let on_disk = std::fs::read_to_string(&config_path).expect("read back");
        assert!(
            !on_disk.contains("sk-legacy-secret"),
            "plaintext must not remain"
        );
        assert!(on_disk.contains("api_key = \"set\""), "marker written back");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_reads_secret_from_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");
        std::fs::write(&config_path, "api_key = \"set\"\n").expect("write marker config");
        let secrets = SecretStore::new(secrets_path);
        secrets
            .set(SecretSlot::Main, "sk-from-store")
            .expect("set secret");

        let store = ConfigStore::new(config_path);
        let cfg = Config::load_from_paths_with(store, secrets).expect("load");
        assert_eq!(cfg.api_key, "sk-from-store");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_marker_not_plaintext() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");

        let cfg = Config {
            api_key: "sk-new-secret".to_owned(),
            ..Default::default()
        };
        let store = ConfigStore::new(config_path.clone());
        let secrets = SecretStore::new(secrets_path.clone());
        cfg.save_with(&store, &secrets).expect("save");

        let on_disk = std::fs::read_to_string(&config_path).expect("read back");
        assert!(
            !on_disk.contains("sk-new-secret"),
            "plaintext must not be written"
        );
        assert!(on_disk.contains("api_key = \"set\""));
        assert_eq!(
            secrets.load(SecretSlot::Main).as_deref(),
            Some("sk-new-secret")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_key_deletes_from_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        secrets.set(SecretSlot::Main, "sk-to-delete").expect("set");
        assert!(secrets.has(SecretSlot::Main));

        // service 层空串语义 → save 时 delete。
        secrets.delete(SecretSlot::Main).expect("delete");
        assert!(!secrets.has(SecretSlot::Main));
        assert!(secrets.load(SecretSlot::Main).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── P1-C3：双真相源收敛迁移测试 ─────────────────────────────────────
#[cfg(test)]
mod c3_migration_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("qaqh-config-c3-{tag}-{}-{n}", std::process::id()))
    }

    fn setup(tag: &str, toml_text: &str) -> (PathBuf, ConfigStore, SecretStore) {
        let dir = temp_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("config.toml"), toml_text).expect("write toml");
        let store = ConfigStore::new(dir.join("config.toml"));
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        (dir, store, secrets)
    }

    fn top_level_absent(text: &str) -> bool {
        // 顶层模型族键必须全部消失（[profiles.*] 内的同名键不算）。
        let doc: toml::Value = toml::from_str(text).expect("parse toml");
        doc.get("model").is_none()
            && doc.get("base_url").is_none()
            && doc.get("max_tokens").is_none()
            && doc.get("context_limit").is_none()
            && doc.get("endpoint").is_none()
            && doc.get("reasoning_effort").is_none()
    }

    /// 旧形态（纯扁平）：load 即触发一次性迁移——值固化进 profile、顶层剥离；
    /// 重载后值不丢。
    #[test]
    fn legacy_flat_migrates_on_first_load() {
        let (dir, store, secrets) = setup(
            "legacy",
            "model = \"legacy-model\"
             base_url = \"https://legacy/v1\"
             max_tokens = 8192
             context_limit = 256000
             endpoint = \"openai\"
             reasoning_effort = \"max\"
             active_profile = \"default\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "legacy-model");
        assert_eq!(cfg.context_limit, 256000);

        // load 的 needs_rewrite 已完成迁移写回。
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(
            top_level_absent(&text),
            "top-level flats must be stripped: {text}"
        );
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        assert_eq!(
            doc["profiles"]["default"]["model"].as_str(),
            Some("legacy-model")
        );
        assert_eq!(
            doc["profiles"]["default"]["context_limit"].as_integer(),
            Some(256000)
        );

        // 重载幂等：值经 profile 回来，不再有扁平覆盖。
        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.model, "legacy-model");
        assert_eq!(cfg2.context_limit, 256000);
        assert_eq!(cfg2.reasoning_effort, "max");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 混合形态：扁平与 profile 并存且值不同 → 读时扁平胜出（历史语义），
    /// 迁移写回后收敛为扁平值入 profile。
    #[test]
    fn mixed_shape_flat_wins_then_converges() {
        let (dir, store, secrets) = setup(
            "mixed",
            "model = \"flat-model\"
             active_profile = \"default\"
             [profiles.default]
             model = \"profile-model\"
             max_tokens = 4096
             context_limit = 128000
             base_url = \"https://p/v1\"
             endpoint = \"openai\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "flat-model");

        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(top_level_absent(&text), "{text}");
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        // 扁平值为最新意图：迁移时无条件覆盖同名 profile 条目。
        assert_eq!(
            doc["profiles"]["default"]["model"].as_str(),
            Some("flat-model")
        );
        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.model, "flat-model");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新形态（纯 profile）：读写稳定，无迁移发生。
    #[test]
    fn new_format_profile_only_stable() {
        let (dir, store, secrets) = setup(
            "newfmt",
            "active_profile = \"default\"
             [profiles.default]
             model = \"m1\"
             max_tokens = 96000
             effort = \"max\"
             context_limit = 1000000
             base_url = \"https://x/v1\"
             endpoint = \"openai\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "m1");
        assert_eq!(cfg.context_limit, 1_000_000);
        cfg.save_with(&store, &secrets).expect("save");
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(top_level_absent(&text), "{text}");
        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.model, "m1");
        assert_eq!(cfg2.reasoning_effort, "max");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
