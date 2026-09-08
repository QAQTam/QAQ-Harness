//! PR-M3-2 验收（PLAN §4 出口）：`cargo test -p qaqh-config --test mcp_import`
//! 全绿。
//!
//! 覆盖（A 只读合并 + B 显式导入）：
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `scan_codex_parses_toml` | `[mcp_servers]` TOML 解析（command/args/env） |
//! | `scan_claude_parses_stdio_and_http` | stdio + http/sse 两形态 |
//! | `merge_external_prefixes_and_avoids_collisions` | `ext-<src>-` 前缀；本地优先；非法 transport 跳过 |
//! | `merge_external_respects_switch` | `import_external=false` → None |
//! | `scan_opencode_parses_local_and_remote` | opencode local(command 数组拆分)/remote + enabled:false 跳过 |
//! | `merge_external_merges_opencode_source` | opencode 源以前缀 `ext-opencode-` 并入 |
//! | `import_writes_config_and_secretizes_env` | config 写入 + env 占位符化 + secrets 往返 |
//! | `import_skips_existing_and_respects_rejection` | 碰撞跳过 + 审批拒绝 |
//!
//! 信任边界用例（D4）：项目级源只能经 import_servers + confirm 回调——
//! merge_external 的扫描路径永不触碰项目级文件（结构上不可能：只收用户级
//! 路径参数）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;

use qaqh_config::mcp_import::{
    self, ExternalServer, ExternalSource, UserPaths, import_servers, merge_external, scan_claude,
    scan_codex, scan_opencode,
};
use qaqh_config::secrets::SecretStore;
use qaqh_types::ConfigStore;

// ── fixture ──

const CODEX_TOML: &str = r#"
model = "gpt-5"

[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
env = { CONTEXT7_API_KEY = "k-123" }

[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
"#;

const CLAUDE_JSON: &str = r#"
{
  "mcpServers": {
    "deepwiki": { "type": "http", "url": "https://mcp.deepwiki.com/mcp", "headers": { "Authorization": "Bearer t-1" } },
    "playwright": { "command": "npx", "args": ["-y", "@playwright/mcp@latest"] }
  }
}
"#;

const OPENCODE_JSON: &str = r#"
{
  "mcp": {
    "context7": { "type": "remote", "url": "https://mcp.context7.com/mcp", "headers": { "Authorization": "Bearer t-9" } },
    "codegraph": { "type": "local", "command": ["codegraph", "serve", "--mcp"], "environment": { "CG_PORT": "17313" } },
    "off": { "type": "local", "command": ["never-run"], "enabled": false }
  }
}
"#;

fn write_opencode(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("opencode.json");
    std::fs::write(&path, OPENCODE_JSON).unwrap();
    path
}

fn user_paths(codex: std::path::PathBuf, claude: std::path::PathBuf) -> UserPaths {
    UserPaths {
        codex,
        claude,
        opencode: std::path::PathBuf::from("/nonexistent-opencode.json"),
    }
}

fn write_codex(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("codex-config.toml");
    std::fs::write(&path, CODEX_TOML).unwrap();
    path
}

fn write_claude(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("claude.json");
    std::fs::write(&path, CLAUDE_JSON).unwrap();
    path
}

fn base_cfg(import_external: bool) -> qaqh_config::config::McpConfig {
    qaqh_config::config::McpConfig {
        enabled: true,
        idle_shutdown_secs: 300,
        servers: BTreeMap::from([(
            "local".to_owned(),
            qaqh_config::config::McpServerConfig {
                transport: qaqh_config::config::McpTransportKind::Stdio,
                command: "node".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                url: String::new(),
                headers: BTreeMap::new(),
                tools: None,
                resources_enabled: true,
                default_timeout_secs: 60,
                max_concurrent_calls: 1,
                cwd: String::new(),
            },
        )]),
        import_external,
    }
}

// ── 扫描 ──

#[test]
fn scan_codex_parses_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_codex(dir.path());
    let servers = scan_codex(&path);
    assert_eq!(servers.len(), 2);
    let context7 = servers.iter().find(|s| s.name == "context7").unwrap();
    assert_eq!(context7.command, "npx");
    assert_eq!(context7.args, vec!["-y", "@upstash/context7-mcp"]);
    assert_eq!(
        context7.env.get("CONTEXT7_API_KEY").map(String::as_str),
        Some("k-123")
    );
    assert_eq!(context7.source, ExternalSource::Codex);
}

#[test]
fn scan_claude_parses_stdio_and_http() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_claude(dir.path());
    let servers = scan_claude(&path, ExternalSource::ClaudeUser);
    assert_eq!(servers.len(), 2);
    let deepwiki = servers.iter().find(|s| s.name == "deepwiki").unwrap();
    assert_eq!(deepwiki.url, "https://mcp.deepwiki.com/mcp");
    assert_eq!(
        deepwiki.headers.get("Authorization").map(String::as_str),
        Some("Bearer t-1")
    );
    let playwright = servers.iter().find(|s| s.name == "playwright").unwrap();
    assert_eq!(playwright.command, "npx");
    assert_eq!(playwright.source, ExternalSource::ClaudeUser);
}

#[test]
fn scan_opencode_parses_local_and_remote() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_opencode(dir.path());
    let servers = scan_opencode(&path);
    // enabled:false 的 off 条目被跳过，只剩 2 个。
    assert_eq!(servers.len(), 2);
    // remote 形态：url + headers。
    let context7 = servers.iter().find(|s| s.name == "context7").unwrap();
    assert_eq!(context7.url, "https://mcp.context7.com/mcp");
    assert_eq!(
        context7.headers.get("Authorization").map(String::as_str),
        Some("Bearer t-9")
    );
    assert_eq!(context7.source, ExternalSource::Opencode);
    // local 形态：command 数组首项为 binary，余项为 args；environment → env。
    let codegraph = servers.iter().find(|s| s.name == "codegraph").unwrap();
    assert_eq!(codegraph.command, "codegraph");
    assert_eq!(codegraph.args, vec!["serve", "--mcp"]);
    assert_eq!(codegraph.env.get("CG_PORT").map(String::as_str), Some("17313"));
}

#[test]
fn scan_opencode_missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let servers = scan_opencode(&dir.path().join("not-exist.json"));
    assert!(servers.is_empty(), "缺文件 → 空结果不报错");
}

#[test]
fn merge_external_merges_opencode_source() {
    let dir = tempfile::tempdir().unwrap();
    let codex = write_codex(dir.path());
    let claude = write_claude(dir.path());
    let opencode = write_opencode(dir.path());
    let mut cfg = base_cfg(true);
    let paths = UserPaths { codex, claude, opencode };
    let report = merge_external(&mut cfg, &paths).expect("import_external=true → Some");
    let names: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
    assert!(names.contains(&"ext-opencode-context7"), "{names:?}");
    assert!(names.contains(&"ext-opencode-codegraph"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("off")), "enabled:false 不合并：{names:?}");
    assert!(
        report.merged.iter().any(|n| n == "ext-opencode-codegraph"),
        "报告含 opencode 条目：{report:?}"
    );
    // transport 推导：local → Stdio，remote → Http。
    assert!(matches!(
        cfg.servers.get("ext-opencode-codegraph").unwrap().transport,
        qaqh_config::config::McpTransportKind::Stdio
    ));
    assert!(matches!(
        cfg.servers.get("ext-opencode-context7").unwrap().transport,
        qaqh_config::config::McpTransportKind::Http
    ));
}

#[test]
fn merge_external_prefixes_and_avoids_collisions() {
    let dir = tempfile::tempdir().unwrap();
    let codex = write_codex(dir.path());
    let claude = write_claude(dir.path());
    let mut cfg = base_cfg(true);
    // 碰撞样本：ext- 前缀名已被本地占用 → 本地优先跳过。
    cfg.servers.insert(
        "ext-codex-context7".to_owned(),
        cfg.servers.get("local").unwrap().clone(),
    );

    let report = merge_external(&mut cfg, &user_paths(codex, claude)).expect("import_external=true → Some");
    let names: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
    assert!(names.contains(&"ext-codex-filesystem"), "{names:?}");
    assert!(names.contains(&"ext-claude-deepwiki"), "{names:?}");
    assert!(names.contains(&"ext-claude-playwright"), "{names:?}");
    assert_eq!(
        report.skipped_collisions,
        vec!["ext-codex-context7"],
        "碰撞：本地优先（D4 手写面权威）"
    );
    // 本地面未被破坏。
    assert!(cfg.servers.contains_key("local"));
    // 合并后的 server 可推导 transport（http 源 → Http）。
    assert!(matches!(
        cfg.servers.get("ext-claude-deepwiki").unwrap().transport,
        qaqh_config::config::McpTransportKind::Http
    ));
}

#[test]
fn merge_external_respects_switch() {
    let dir = tempfile::tempdir().unwrap();
    let codex = write_codex(dir.path());
    let claude = write_claude(dir.path());
    let mut cfg = base_cfg(false);
    let report = merge_external(&mut cfg, &user_paths(codex, claude));
    assert_eq!(report, None, "import_external=false → None（不扫描）");
    assert_eq!(cfg.servers.len(), 1, "未合并");
}

#[test]
fn merge_external_missing_files_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = base_cfg(true);
    let missing = dir.path().join("not-exist.toml");
    let paths = UserPaths {
        codex: missing.clone(),
        claude: missing.clone(),
        opencode: missing,
    };
    let report = merge_external(&mut cfg, &paths).expect("开关开 → Some");
    assert_eq!(
        report.merged,
        Vec::<String>::new(),
        "缺文件 → 空合并，不报错"
    );
}

// ── B：显式导入 ──

fn sample_external() -> ExternalServer {
    ExternalServer {
        name: "context7".to_owned(),
        command: "npx".to_owned(),
        args: vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()],
        env: BTreeMap::from([("Context7_API_Key".to_owned(), "secret-value-1".to_owned())]),
        url: String::new(),
        headers: BTreeMap::new(),
        source: ExternalSource::Codex,
    }
}

#[test]
fn import_writes_config_and_secretizes_env() {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(dir.path().join("config.toml"));
    let secrets = SecretStore::new(dir.path().join("secrets.toml"));
    let mut cfg = base_cfg(true);

    let all = |_: &ExternalServer| true;
    let report = import_servers(&[sample_external()], &all, &mut cfg, &secrets).expect("import ok");
    assert_eq!(report.imported, vec!["context7"]);

    // config：原名进入 [mcp.servers]，env 值已占位符化（无明文残留；键原样
    // 保留——大小写对某些 server 有意义，仅 secret 名 lowercase）。
    let server = cfg.servers.get("context7").expect("imported");
    let env_value = server.env.get("Context7_API_Key").expect("key preserved");
    assert_eq!(
        env_value, "${secret:mcp-context7-context7_api_key}",
        "{env_value}"
    );
    assert!(
        !serde_json::to_string(&cfg)
            .unwrap()
            .contains("secret-value-1")
    );

    // secrets 往返：明文可取回。
    assert_eq!(
        secrets.load_mcp("mcp-context7-context7_api_key").as_deref(),
        Some("secret-value-1")
    );
    let _ = store; // save_with 由调用方（CLI）执行；此处只验库函数语义
}

#[test]
fn import_skips_existing_and_respects_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretStore::new(dir.path().join("secrets.toml"));
    let mut cfg = base_cfg(true);

    let reject_all = |_: &ExternalServer| false;
    let report = import_servers(&[sample_external()], &reject_all, &mut cfg, &secrets).unwrap();
    assert_eq!(report.rejected, vec!["context7"], "审批拒绝");
    assert!(!cfg.servers.contains_key("context7"));

    // 碰撞：同名已存在 → skip。
    let approve_all = |_: &ExternalServer| true;
    let _ = import_servers(&[sample_external()], &approve_all, &mut cfg, &secrets).unwrap();
    let report = import_servers(&[sample_external()], &approve_all, &mut cfg, &secrets).unwrap();
    assert_eq!(report.skipped_existing, vec!["context7"]);
    assert!(report.imported.is_empty());
}

#[test]
fn merge_scan_never_touches_project_level() {
    // D4 结构性守卫：merge_external 只接受显式传入的用户级路径结构体——
    // 项目级 .mcp.json 不在任何默认扫描路径里（default_user_paths 只返回
    // ~/.codex/config.toml、~/.claude.json 与 opencode.json 三个用户级路径）。
    let paths = mcp_import::default_user_paths();
    assert!(paths.codex.ends_with(".codex/config.toml"));
    assert!(paths.claude.ends_with(".claude.json"));
    assert!(paths.opencode.ends_with(".config/opencode/opencode.json"));
}
