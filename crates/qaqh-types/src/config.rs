use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ── Config persistence ──

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentConfig {
    /// Provider ID (e.g. "deepseek", "mimo")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// P1-C3 退役：不再写入磁盘（读兼容保留）；模型族字段以
    /// `[profiles.<active>]` 为唯一持久化真相。
    #[serde(skip_serializing)]
    pub model: Option<String>,
    /// P1-C3 退役：不再写入磁盘（读兼容保留）；模型族字段以
    /// `[profiles.<active>]` 为唯一持久化真相。
    #[serde(skip_serializing)]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// P1-C3 退役：不再写入磁盘（读兼容保留）；模型族字段以
    /// `[profiles.<active>]` 为唯一持久化真相。
    #[serde(skip_serializing)]
    pub context_limit: Option<u32>,
    /// Endpoint within the provider: "openai" | "anthropic" | ...
    /// P1-C3 退役：不再写入磁盘（读兼容保留）；模型族字段以
    /// `[profiles.<active>]` 为唯一持久化真相。
    #[serde(skip_serializing)]
    pub endpoint: Option<String>,
    /// Reasoning effort: "high" or "max". Thinking is always enabled.
    /// P1-C3 退役：不再写入磁盘（读兼容保留）；模型族字段以
    /// `[profiles.<active>]` 为唯一持久化真相。
    #[serde(skip_serializing)]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<HashMap<String, ProfileConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,

    /// UI 字体（WinUI 壳全局 FontFamily）。`None`/空 = 跟随系统默认字体。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,

    /// UI 主题偏好：`system` | `light` | `dark` | `dark-gray`；`None`/空 = 跟随系统。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,

    /// 桌面通知开关。`None` = 开启（缺省）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications_enabled: Option<bool>,

    // ── Daemon session policy ──
    /// 空闲会话 worker 自动卸载阈值（秒）。`None`/0 = 禁用（缺省）。
    /// 卸载 = 优雅 seal + 从 registry 摘除 + 释放内存；下次输入自动
    /// load_for_resume 恢复。详见 docs/memory-governance-plan.md §E。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle_unload_secs: Option<u64>,

    // ── Subagent defaults ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<PersistentSubagentConfig>,

    // ── Compliance / content filter ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_extra_keywords: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_allowlist: Option<Vec<String>>,

    // ── Permission ──
    /// Agent permission level: 1=MaxLockdown, 2=ReadFree, 3=WorkspaceFree, 4=Unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<u8>,

    /// Path to a HuggingFace tokenizer.json for accurate token counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_path: Option<String>,

    /// Auto-compact threshold: fraction of context_limit (0.0-1.0).
    /// When total tokens exceed context_limit * threshold, compact is triggered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<f64>,

    /// 工具套件运行环境（qaqh-workspace serve）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PersistentWorkspaceConfig>,

    /// MCP 客户端配置（docs/mcp-client-design.md §6）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<PersistentMcpConfig>,
}

/// 工具套件运行环境配置。
///
/// - `local`（默认）：daemon 拉起本机 `qaqh-workspace serve`（Windows 原生）。
/// - `wsl`（仅 Windows）：daemon 经 `wsl.exe` 在 WSL 发行版内拉起
///   `qaqh-workspace serve`，Windows 端经 localhost 访问（WSL2 自动转发）。
///   Linux 原生系统无此选项——工具本来就在 Linux 环境运行。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentWorkspaceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Persistence-friendly subagent config with all-Option fields.
///
/// Stored as a subsection of the main config. All fields are `Option` so that
/// partial overrides work — `None` = inherit from parent agent config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentSubagentConfig {
    /// Override model for subagent. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override base URL. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Override API key. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override max output tokens. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Max time in seconds before the subagent is killed. `None` = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Default tool allowlist for subagents. Empty = all tools available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
}

/// MCP 客户端配置持久层（docs/mcp-client-design.md §6；全部 Option）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentMcpConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// PR-M3-2：用户级外部配置（Codex/Claude）只读合并开关；缺省 true。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_external: Option<bool>,
    /// idle 回收阈值（秒）；None/0 = 常驻不回收。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<HashMap<String, PersistentMcpServerConfig>>,
}

/// 单个 MCP server（stdio 与 streamable HTTP 互斥；config 声明即信任，D4/D5）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentMcpServerConfig {
    /// stdio 启动命令（如 "npx"）。与 url 互斥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// 注入 server 进程的环境变量；值可为 `${secret:name}` 占位符
    /// （secret 本体在 secrets.toml；qaqh-config 不求值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// streamable HTTP endpoint（M3）。与 command 互斥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// 工具白名单（server 侧原始名）；None = 全部暴露。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_calls: Option<u32>,
}

// ── Profile / Preferences ──
/// Named profile bundling model, token, and effort settings.
///
/// Profiles let users switch between config presets (e.g. "fast" vs "deep")
/// without manually changing individual settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// Model identifier for this profile.
    pub model: String,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// Reasoning effort: one of `low|medium|high|xhigh|max`, or `None` to use default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Maximum context window size (input tokens).
    pub context_limit: u32,
    /// API base URL for this profile.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Endpoint within the provider for this profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_base_url() -> String {
    "https://api.deepseek.com".into()
}

// ── ConfigStore: unified config I/O with atomic writes ──

/// Unified config I/O with atomic writes.
///
/// Writes use a temp-file + rename pattern to prevent corruption from
/// partial writes.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Create a ConfigStore for a specific file path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Create a ConfigStore for the default config.toml location.
    pub fn default_location() -> Self {
        Self::new(crate::platform::config_path())
    }

    /// Check whether the config file exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load and deserialize the config. Returns `None` if the file doesn't
    /// exist or is invalid TOML.
    pub fn load(&self) -> Option<PersistentConfig> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        toml::from_str(&data).ok()
    }

    /// Atomically write the config to disk using temp-file + rename.
    /// Returns `true` on success.
    pub fn save(&self, config: &PersistentConfig) -> bool {
        let content = match toml::to_string_pretty(config) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ConfigStore: serialization failed: {e}");
                return false;
            }
        };
        let tmp = self.path.with_extension("toml.tmp");
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "ConfigStore: create_dir_all({}) failed: {e}",
                parent.display()
            );
            return false;
        }
        if let Err(e) = std::fs::write(&tmp, &content) {
            eprintln!("ConfigStore: write({}) failed: {e}", tmp.display());
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            eprintln!(
                "ConfigStore: rename({} -> {}) failed: {e}",
                tmp.display(),
                self.path.display()
            );
            return false;
        }
        true
    }

    /// Load the config file as a raw `serde_json::Value`.
    /// Used for backward compatibility with JSON-based consumers.
    pub fn load_value(&self) -> Option<serde_json::Value> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        let tv: toml::Value = toml::from_str(&data).ok()?;
        // Convert toml::Value → serde_json::Value for backward compat
        serde_json::to_value(&tv).ok()
    }
}
