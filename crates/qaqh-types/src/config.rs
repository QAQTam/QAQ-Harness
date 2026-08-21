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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    /// Endpoint within the provider: "openai" | "anthropic" | ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Reasoning effort: "high" or "max". Thinking is always enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
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

    // ── Multimodal (vision) LLM config ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multimodal: Option<PersistentMultimodalConfig>,

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

/// Persistence-friendly multimodal (vision) LLM config.
///
/// Separate from the main LLM provider because multimodal models (e.g. MiMo)
/// may have different endpoints, API keys, and model names than the primary
/// text-only provider.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentMultimodalConfig {
    /// Whether multimodal image understanding is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Provider type: "mimo", "ollama", "openai_compat", "lmstudio".
    /// Determines which backend adapter is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    /// Provider ID for multimodal (e.g. "mimo"). If None, uses the main provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// API key for multimodal provider. If None, uses the main API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Base URL for multimodal API. If None, uses provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model name for multimodal (e.g. "mimo-v2.5").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Max output tokens for multimodal requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
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
    /// Reasoning effort: "high", "max", or `None` to use default.
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

/// Balance/info response from the provider's balance endpoint.
///
/// Fields correspond to the JSON response from `GET /user/balance` or
/// equivalent provider-specific endpoints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BalanceInfo {
    /// Whether the balance endpoint returned usable data.
    pub is_available: bool,
    /// Currency code (e.g. "USD", "CNY").
    pub currency: String,
    /// Total balance available.
    pub total_balance: String,
    /// Granted (free-tier) balance.
    pub granted_balance: String,
    /// Top-up (purchased) balance.
    pub topped_up_balance: String,
}
