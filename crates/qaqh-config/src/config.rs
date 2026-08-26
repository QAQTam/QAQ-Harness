use crate::secrets::{CONFIG_MARKER, SecretSlot, SecretStore};
use qaqh_types::{
    ConfigStore, PersistentConfig, PersistentSubagentConfig, PersistentWorkspaceConfig,
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

/// RAG（检索增强生成）配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RagConfig {
    /// 是否启用 RAG（false 时不加载向量引擎）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// HuggingFace 模型 ID
    #[serde(default = "default_rag_model")]
    pub model: String,
    /// 嵌入向量维度（512 = bge-small, 768 = bge-base）
    #[serde(default = "default_embed_dim")]
    pub embed_dim: usize,
    /// 数据存储目录（None = 自动选择 ~/.qaqh/vector/）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_dir: Option<String>,
    /// 技能语义检索 top-K
    #[serde(default = "default_skill_top_k")]
    pub skill_top_k: usize,
    /// 记忆检索 top-K
    #[serde(default = "default_memory_top_k")]
    pub memory_top_k: usize,
    /// 本地模型目录（设置后跳过 HF Hub 下载）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_model: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_rag_model() -> String {
    "BAAI/bge-small-zh-v1.5".into()
}
fn default_embed_dim() -> usize {
    512
}
fn default_skill_top_k() -> usize {
    5
}
fn default_memory_top_k() -> usize {
    3
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "BAAI/bge-small-zh-v1.5".into(),
            embed_dim: 512,
            store_dir: None,
            skill_top_k: 5,
            memory_top_k: 3,
            local_model: None,
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
    /// Reasoning effort: "high", "max", or empty.
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
    /// RAG 向量引擎配置（embedding / 语义搜索 / 跨会话记忆）
    pub rag: RagConfig,
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
    /// 工具套件运行环境："local"（默认）| "wsl"（仅 Windows）。
    pub workspace: WorkspaceConfig,
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
            rag: RagConfig::default(),
            permission_level: 4, // Unrestricted — backward compat
            tokenizer_path: None,
            auto_compact_threshold: 0.75,
            workspace: WorkspaceConfig::default(),
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

    fn load_from_paths_with(store: ConfigStore, secrets: SecretStore) -> Result<Self, String> {
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
            if pc.base_url.as_deref().map_or(true, |u| u.is_empty()) {
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

            // ── 工具套件运行环境 ──
            if let Some(ref ws) = pc.workspace {
                if let Some(ref mode) = ws.mode {
                    cfg.workspace.mode = mode.clone();
                }
            }

            // 迁移写回：config.toml 中旧明文已入 secret store，把明文替换为
            // "set" 标记（重新 load 磁盘原值，只改"确为明文"的槽位——迁移
            // 失败的槽位保持明文，下次 load 重试；已标记/未配置的原样保留）。
            if needs_rewrite {
                if let Some(mut fresh) = store.load() {
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

    fn save_with(&self, store: &ConfigStore, secrets: &SecretStore) -> Result<(), String> {
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
            workspace: Some(PersistentWorkspaceConfig {
                mode: Some(self.workspace.mode.clone()),
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

    pub fn is_ready(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Protocol derived from (provider_id, endpoint) in the registry.
    pub fn protocol(&self) -> String {
        crate::registry::protocol_for(&self.provider_id, &self.endpoint)
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

        let mut cfg = Config::default();
        cfg.api_key = "sk-new-secret".to_owned();
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
