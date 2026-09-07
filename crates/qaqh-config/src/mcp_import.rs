//! PR-M3-2：Codex / Claude Code MCP 配置兼容（owner 决策 A+B，2026-09-07）。
//!
//! 路线 A（只读合并，[`merge_external`]）：daemon 装配时扫描**用户级**
//! 外部配置（`~/.codex/config.toml` 的 `[mcp_servers]`、`~/.claude.json`
//! 顶层 `mcpServers`），以 `ext-<source>-<name>` 前缀并入运行时视图——
//! **只读不回写**、明文 env 直通子进程（不落 QAQH secrets）、本地手写
//! 面碰撞时本地优先。开关 `[mcp].import_external`（默认 true）。
//!
//! 路线 B（显式导入，[`import_servers`]）：CLI 驱动（`qaqh-daemon mcp
//! import`），dry-run 默认、`--exec` 才写；项目级 `.mcp.json`（供应链面）
//! 需调用方逐 server 审批回调放行。写入 config.toml `[mcp.servers]` +
//! secrets.toml `[secrets.mcp]`（env 占位符化，D4 信任边界不扩）。
//!
//! 信任边界（D4）：只读合并只碰**用户级**文件（用户自己机器上的个人配置
//! 与手写 config 同信任级）；**项目级**文件是仓库里他人提交的内容，绝不
//! 进入只读合并——只能经显式导入 + 审批。

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{McpConfig, McpServerConfig, McpTransportKind};

/// 外部 server 来源（决定只读合并的前缀与导入器的默认审批策略）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    Codex,
    ClaudeUser,
    ClaudeProject,
}

impl ExternalSource {
    /// 只读合并前缀（`ext-<src>-<name>`；符合 server 名 `[a-z0-9_-]` 校验）。
    fn merge_prefix(self) -> &'static str {
        match self {
            ExternalSource::Codex => "ext-codex-",
            ExternalSource::ClaudeUser => "ext-claude-",
            ExternalSource::ClaudeProject => "ext-claproj-",
        }
    }

    /// 导入器 `--from` 值。
    pub fn from_flag(self) -> &'static str {
        match self {
            ExternalSource::Codex => "codex",
            ExternalSource::ClaudeUser => "claude",
            ExternalSource::ClaudeProject => "claude-project",
        }
    }
}

/// 外部 server 草稿（源内原样，未做 transport 推导/校验）。
#[derive(Debug, Clone)]
pub struct ExternalServer {
    /// 源内原始名（导入后原样入 [mcp.servers]；合并时加前缀）。
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// http/sse 类型的 endpoint（Claude Code `type: "http"|"sse"`；Codex 无）。
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub source: ExternalSource,
}

impl ExternalServer {
    /// → McpServerConfig（transport 推导与 map_mcp_config 同规则：
    /// command 非空 → stdio；否则 url 非空 → http）。
    pub fn to_server_config(&self) -> Result<McpServerConfig, String> {
        let transport = match (self.command.trim().is_empty(), self.url.trim().is_empty()) {
            (false, true) => McpTransportKind::Stdio,
            (true, false) => McpTransportKind::Http,
            _ => {
                return Err(format!(
                    "external server {:?}: 需要 command（stdio）或 url（http）二者其一",
                    self.name
                ));
            }
        };
        Ok(McpServerConfig {
            transport,
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            tools: None,
            resources_enabled: true,
            default_timeout_secs: 60,
            max_concurrent_calls: 1,
        })
    }
}

// ── 扫描 ──

/// Codex CLI：`~/.codex/config.toml` 的 `[mcp_servers.<name>]`。
///
/// 字段（以 Codex 当前文档为准）：`command`（必填）、`args`（可选）、
/// `env`（可选，明文）。解析失败/缺文件 → 空（外部配置缺失不是错误）。
pub fn scan_codex(path: &Path) -> Vec<ExternalServer> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        log::warn!("[mcp import] {} 解析失败，跳过 codex 源", path.display());
        return Vec::new();
    };
    let Some(servers) = value.get("mcp_servers").and_then(|v| v.as_table()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| {
            let command = entry.get("command").and_then(|v| v.as_str())?.to_owned();
            Some(ExternalServer {
                name: name.clone(),
                command,
                args: entry
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|args| {
                        args.iter()
                            .filter_map(|a| a.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                env: toml_string_map(entry.get("env")),
                url: String::new(),
                headers: BTreeMap::new(),
                source: ExternalSource::Codex,
            })
        })
        .collect()
}

/// Claude Code：JSON 里的 `mcpServers` 键（`~/.claude.json` 用户级顶层；
/// 项目级 `.mcp.json` 整个文件即 `{ "mcpServers": … }`）。
///
/// stdio：`{ command, args?, env? }`；http/sse：`{ type: "http"|"sse", url,
/// headers? }`。s起 server 的 trust/approve 语义在源侧，这里只做结构解析。
pub fn scan_claude(path: &Path, source: ExternalSource) -> Vec<ExternalServer> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        log::warn!("[mcp import] {} 解析失败，跳过 claude 源", path.display());
        return Vec::new();
    };
    let Some(servers) = value.get("mcpServers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| {
            let server_type = entry
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("stdio");
            if server_type == "http" || server_type == "sse" {
                let url = entry.get("url").and_then(|v| v.as_str())?.to_owned();
                Some(ExternalServer {
                    name: name.clone(),
                    command: String::new(),
                    args: vec![],
                    env: BTreeMap::new(),
                    url,
                    headers: string_map(entry.get("headers")),
                    source,
                })
            } else {
                let command = entry.get("command").and_then(|v| v.as_str())?.to_owned();
                Some(ExternalServer {
                    name: name.clone(),
                    command,
                    args: entry
                        .get("args")
                        .and_then(|v| v.as_array())
                        .map(|args| {
                            args.iter()
                                .filter_map(|a| a.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                    env: string_map(entry.get("env")),
                    url: String::new(),
                    headers: BTreeMap::new(),
                    source,
                })
            }
        })
        .collect()
}

/// 默认用户级扫描路径组（A 路线用；文件缺失 → 空结果，不报错）。
pub fn default_user_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    (
        Path::new(&home).join(".codex").join("config.toml"),
        Path::new(&home).join(".claude.json"),
    )
}

fn string_map(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// toml::Value 版（scan_codex 专用；env 表全部为字符串值）。
fn toml_string_map(value: Option<&toml::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

// ── A：只读合并 ──

/// 合并报告（daemon 启动日志 + 观测）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub merged: Vec<String>,
    pub skipped_collisions: Vec<String>,
    pub skipped_invalid: Vec<String>,
}

/// 用户级外部配置 → 运行时视图（A 路线；`cfg.import_external == false` →
/// 返回 None，不做扫描）。
///
/// 合并规则：`ext-<source>-<name>` 前缀避撞；与手写面同名 → **本地优先**
/// （skip 碰撞 + warn）；transport 推导失败的条目 → skip_invalid。
pub fn merge_external(
    cfg: &mut McpConfig,
    codex_path: &Path,
    claude_path: &Path,
) -> Option<MergeReport> {
    if !cfg.import_external {
        return None;
    }
    let mut report = MergeReport::default();
    let mut candidates = scan_codex(codex_path);
    candidates.extend(scan_claude(claude_path, ExternalSource::ClaudeUser));
    for external in candidates {
        let merged_name = format!("{}{}", external.source.merge_prefix(), external.name);
        if cfg.servers.contains_key(&merged_name) {
            report.skipped_collisions.push(merged_name);
            continue;
        }
        match external.to_server_config() {
            Ok(server_cfg) => {
                cfg.servers.insert(merged_name.clone(), server_cfg);
                report.merged.push(merged_name);
            }
            Err(reason) => report
                .skipped_invalid
                .push(format!("{}: {reason}", external.name)),
        }
    }
    Some(report)
}

// ── B：显式导入 ──

/// 导入报告。
#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub rejected: Vec<String>,
}

/// 显式导入（B 路线）：逐 server 写入 config.toml `[mcp.servers]` +
/// secrets.toml `[secrets.mcp]`（env 值占位符化：`mcp-<server>-<envkey>`）。
///
/// `confirm`：审批回调（CLI 交互；用户级源默认 true，项目级必须逐个放行）。
/// 返回 false → 该 server 进 `rejected`。env 为空 map 时留空键不写（写空
/// 段会污染 secrets 文件）。
pub fn import_servers(
    servers: &[ExternalServer],
    confirm: &dyn Fn(&ExternalServer) -> bool,
    config: &mut McpConfig,
    secrets: &crate::secrets::SecretStore,
) -> Result<ImportReport, String> {
    let mut report = ImportReport::default();
    for external in servers {
        if !confirm(external) {
            report.rejected.push(external.name.clone());
            continue;
        }
        if config.servers.contains_key(&external.name) {
            report.skipped_existing.push(external.name.clone());
            continue;
        }
        let mut server_cfg = external.to_server_config()?;
        // env 占位符化：明文值进 secrets（三段式契约，DTO/save 永不见明文）。
        let mut env = BTreeMap::new();
        for (key, plaintext) in &server_cfg.env {
            let secret_name = format!("mcp-{}-{}", external.name, key.to_lowercase());
            secrets.set_mcp(&secret_name, plaintext)?;
            env.insert(key.clone(), format!("${{secret:{secret_name}}}"));
        }
        server_cfg.env = env;
        config.servers.insert(external.name.clone(), server_cfg);
        report.imported.push(external.name.clone());
    }
    config.enabled = true;
    Ok(report)
}
