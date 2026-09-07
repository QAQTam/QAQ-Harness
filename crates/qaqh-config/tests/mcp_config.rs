//! PR-M1-1 验收（PLAN §4）：`[mcp]` 解析 + fail-fast 校验 + 持久层往返。
//!
//! 路径隔离手法沿用 config.rs 内联测试惯例（tempdir + 显式路径 load，
//! 不触碰全局 platform 路径，无共享状态，无需 TEST_RUNTIME_SERIAL）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例）

use std::collections::HashMap;

use qaqh_config::config::{McpConfig, McpTransportKind};
use qaqh_config::{Config, secrets::SecretStore};
use qaqh_types::{ConfigStore, PersistentConfig, PersistentMcpConfig, PersistentMcpServerConfig};

fn load_toml(tag: &str, content: &str) -> Result<Config, String> {
    load_toml_with_secrets(tag, content, &[])
}

/// E-4 起 `${secret:name}` 引用必须在 secrets.toml 注册（fail-fast）——
/// 需要占位符的夹具先经 `named` 注册。
fn load_toml_with_secrets(
    tag: &str,
    content: &str,
    named: &[(&str, &str)],
) -> Result<Config, String> {
    let dir = std::env::temp_dir().join(format!("qaqh-mcp-cfg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("config.toml");
    std::fs::write(&path, content).expect("write config");
    let store = ConfigStore::new(path);
    let secrets = SecretStore::new(dir.join("secrets.toml"));
    for (name, value) in named {
        secrets.set_mcp(name, value).expect("register named secret");
    }
    let result = Config::load_from_paths_with(store, secrets);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// ── 正常路径 ──

#[test]
fn absent_section_defaults_to_disabled() {
    let cfg = load_toml("absent", "lang = \"zh-CN\"\n").expect("load 应成功");
    assert!(!cfg.mcp.enabled, "未配置 [mcp] → 默认关闭");
    assert!(cfg.mcp.servers.is_empty());
    assert_eq!(cfg.mcp.idle_shutdown_secs, 300, "默认 idle 300s");
}

#[test]
fn parses_stdio_and_http_servers_with_defaults() {
    let toml_text = r#"
[mcp]
enabled = true
idle_shutdown_secs = 120

[mcp.servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
env = { CONTEXT7_API_KEY = "${secret:context7_key}" }

[mcp.servers.figma]
url = "https://mcp.figma.com/mcp"
headers = { Authorization = "Bearer test" }
tools = ["get_code"]
resources = false
default_timeout_secs = 120
max_concurrent_calls = 2
"#;
    let cfg = load_toml_with_secrets("parse", toml_text, &[("context7_key", "sk-test-1234")])
        .expect("load 应成功");
    assert!(cfg.mcp.enabled);
    assert_eq!(cfg.mcp.idle_shutdown_secs, 120);
    assert_eq!(cfg.mcp.servers.len(), 2, "两个 server 都应解析");

    let c7 = cfg.mcp.servers.get("context7").expect("context7 存在");
    assert!(matches!(c7.transport, McpTransportKind::Stdio));
    assert_eq!(c7.command, "npx");
    assert_eq!(c7.args, vec!["-y", "@upstash/context7-mcp"]);
    assert_eq!(
        c7.env.get("CONTEXT7_API_KEY").map(String::as_str),
        Some("${secret:context7_key}"),
        "env 原样保存，secret 占位符不求值"
    );
    // 默认值：resources 开、超时 60s、并发 1、无白名单
    assert!(c7.resources_enabled);
    assert_eq!(c7.default_timeout_secs, 60);
    assert_eq!(c7.max_concurrent_calls, 1);
    assert!(c7.tools.is_none());

    let figma = cfg.mcp.servers.get("figma").expect("figma 存在");
    assert!(matches!(figma.transport, McpTransportKind::Http));
    assert_eq!(figma.url, "https://mcp.figma.com/mcp");
    assert_eq!(figma.tools, Some(vec!["get_code".to_owned()]));
    assert!(!figma.resources_enabled, "显式关闭 resources");
    assert_eq!(figma.default_timeout_secs, 120);
    assert_eq!(figma.max_concurrent_calls, 2);
}

// ── fail-fast 校验 ──

#[test]
fn rejects_illegal_server_name() {
    let err = load_toml("badname", "[mcp.servers.Context7]\ncommand = \"npx\"\n")
        .expect_err("大写字母应被拒绝");
    assert!(err.contains("非法字符"), "实际错误: {err}");
}

#[test]
fn rejects_missing_transport() {
    let err = load_toml("notransport", "[mcp.servers.x1]\nargs = [\"-y\"]\n")
        .expect_err("缺 command 且缺 url 应被拒绝");
    assert!(err.contains("缺少 transport"), "实际错误: {err}");
}

#[test]
fn rejects_conflicting_command_and_url() {
    let err = load_toml(
        "conflict",
        "[mcp.servers.x1]\ncommand = \"npx\"\nurl = \"https://mcp.example.com\"\n",
    )
    .expect_err("command 与 url 同时出现应被拒绝");
    assert!(err.contains("互斥"), "实际错误: {err}");
}

#[test]
fn rejects_bad_url_scheme() {
    let err = load_toml(
        "badscheme",
        "[mcp.servers.x1]\nurl = \"ftp://mcp.example.com\"\n",
    )
    .expect_err("非 http(s) url 应被拒绝");
    assert!(err.contains("http:// 或 https://"), "实际错误: {err}");
}

#[test]
fn rejects_timeout_out_of_range() {
    let err = load_toml(
        "badtimeout",
        "[mcp.servers.x1]\ncommand = \"npx\"\ndefault_timeout_secs = 0\n",
    )
    .expect_err("timeout=0 应被拒绝");
    assert!(err.contains("1..=3600"), "实际错误: {err}");
}

#[test]
fn rejects_concurrency_out_of_range() {
    let err = load_toml(
        "badconc",
        "[mcp.servers.x1]\ncommand = \"npx\"\nmax_concurrent_calls = 65\n",
    )
    .expect_err("并发 65 应被拒绝");
    assert!(err.contains("1..=64"), "实际错误: {err}");
}

// ── 持久层往返（serde 层，不经过运行时映射）──

#[test]
fn persistent_roundtrip_keeps_mcp_section() {
    let dir = std::env::temp_dir().join(format!("qaqh-mcp-cfg-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("config.toml");

    let mut servers = HashMap::new();
    servers.insert(
        "ctx7".to_owned(),
        PersistentMcpServerConfig {
            command: Some("npx".to_owned()),
            args: Some(vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]),
            ..Default::default()
        },
    );
    let pc = PersistentConfig {
        mcp: Some(PersistentMcpConfig {
            enabled: Some(true),
            idle_shutdown_secs: Some(120),
            servers: Some(servers),
        }),
        ..Default::default()
    };

    let store = ConfigStore::new(path.clone());
    assert!(store.save(&pc), "save 应成功");

    let loaded = store.load().expect("roundtrip 后应能读回");
    let mcp = loaded.mcp.expect("mcp 段应保留");
    assert_eq!(mcp.enabled, Some(true));
    assert_eq!(mcp.idle_shutdown_secs, Some(120));
    let servers = mcp.servers.expect("servers 应保留");
    let ctx7 = servers.get("ctx7").expect("ctx7 应保留");
    assert_eq!(ctx7.command.as_deref(), Some("npx"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 运行时类型守卫（防误用）──

#[test]
fn runtime_default_matches_design() {
    let d = McpConfig::default();
    assert!(!d.enabled);
    assert_eq!(d.idle_shutdown_secs, 300);
    assert!(d.servers.is_empty());
}
