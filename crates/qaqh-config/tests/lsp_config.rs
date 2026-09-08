//! lsp_config 验收：`[lsp]` 解析 + fail-fast 校验 + 持久层往返。
//!
//! 路径隔离沿用 mcp_config.rs 惯例（tempdir + 显式路径 load，不触碰全局）。
//!
//! | 用例 | 覆盖 |
//! |---|----|
//! | `absent_section_defaults_to_disabled` | 未配 `[lsp]` → 默认关闭 |
//! | `parses_stdio_server_with_routing` | command/args/env/extensions/timeout 解析 + 归一化 |
//! | `rejects_*` | 非法名/缺 command/空扩展名/超时越界 fail-fast |
//! | `persistent_roundtrip_keeps_lsp_section` | 持久层往返 |
//! | `runtime_default_matches_design` | 默认值守卫（disabled/idle 120s） |

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例）

use std::collections::HashMap;

use qaqh_config::config::{LspConfig, LspServerConfig};
use qaqh_config::{Config, secrets::SecretStore};
use qaqh_types::{ConfigStore, PersistentConfig, PersistentLspConfig, PersistentLspServerConfig};

fn load_toml(tag: &str, content: &str) -> Result<Config, String> {
    load_toml_with_secrets(tag, content, &[])
}

fn load_toml_with_secrets(
    tag: &str,
    content: &str,
    named: &[(&str, &str)],
) -> Result<Config, String> {
    let dir = std::env::temp_dir().join(format!("qaqh-lsp-cfg-{tag}-{}", std::process::id()));
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
    assert!(!cfg.lsp.enabled, "未配置 [lsp] → 默认关闭");
    assert!(cfg.lsp.servers.is_empty());
    assert_eq!(cfg.lsp.idle_shutdown_secs, 120, "默认 idle 120s");
}

#[test]
fn parses_stdio_server_with_routing() {
    let toml_text = r#"
[lsp]
enabled = true
idle_shutdown_secs = 60

[lsp.servers.rust]
command = "rust-analyzer"
env = { RA_LOG = "${secret:ra_log}" }
extensions = ["rs", ".TOML", "rs"]
startup_timeout_secs = 45
default_timeout_secs = 20
"#;
    let cfg = load_toml_with_secrets("parse", toml_text, &[("ra_log", "info")]).expect("load 应成功");
    assert!(cfg.lsp.enabled);
    assert_eq!(cfg.lsp.idle_shutdown_secs, 60);
    assert_eq!(cfg.lsp.servers.len(), 1);

    let rust: &LspServerConfig = cfg.lsp.servers.get("rust").expect("rust 存在");
    assert_eq!(rust.command, "rust-analyzer");
    assert_eq!(
        rust.env.get("RA_LOG").map(String::as_str),
        Some("${secret:ra_log}"),
        "env 原样保存，secret 占位符不求值"
    );
    // 扩展名归一化：去点 + 小写 + 去重。
    assert_eq!(rust.extensions, vec!["rs", "toml"]);
    assert_eq!(rust.startup_timeout_secs, 45);
    assert_eq!(rust.default_timeout_secs, 20);
}

#[test]
fn parses_defaults_when_optional_missing() {
    let toml_text = r#"
[lsp.servers.py]
command = "pyright-langserver"
args = ["--stdio"]
extensions = ["py"]
"#;
    let cfg = load_toml("defaults", toml_text).expect("load 应成功");
    // enabled 缺省 = 有 server 即开（mcp 同款语义）。
    assert!(cfg.lsp.enabled);
    let py = cfg.lsp.servers.get("py").expect("py 存在");
    assert_eq!(py.args, vec!["--stdio"]);
    assert_eq!(py.startup_timeout_secs, 30);
    assert_eq!(py.default_timeout_secs, 30);
}

// ── fail-fast 校验 ──

#[test]
fn rejects_illegal_server_name() {
    let err = load_toml("badname", "[lsp.servers.Rust]\ncommand = \"ra\"\n")
        .expect_err("大写字母应被拒绝");
    assert!(err.contains("非法字符"), "实际错误: {err}");
}

#[test]
fn rejects_missing_command() {
    let err = load_toml("nocommand", "[lsp.servers.rust]\nextensions = [\"rs\"]\n")
        .expect_err("缺 command 应被拒绝");
    assert!(err.contains("command"), "实际错误: {err}");
}

#[test]
fn rejects_empty_extension() {
    let err = load_toml(
        "badext",
        "[lsp.servers.rust]\ncommand = \"ra\"\nextensions = [\".\"]\n",
    )
    .expect_err("空扩展名应被拒绝");
    assert!(err.contains("extensions"), "实际错误: {err}");
}

#[test]
fn rejects_timeout_out_of_range() {
    let err = load_toml(
        "badtimeout",
        "[lsp.servers.rust]\ncommand = \"ra\"\nstartup_timeout_secs = 0\n",
    )
    .expect_err("startup=0 应被拒绝");
    assert!(err.contains("1..=600"), "实际错误: {err}");
    let err = load_toml(
        "badtimeout2",
        "[lsp.servers.rust]\ncommand = \"ra\"\ndefault_timeout_secs = 3601\n",
    )
    .expect_err("timeout 越界应被拒绝");
    assert!(err.contains("1..=3600"), "实际错误: {err}");
}

#[test]
fn rejects_unregistered_secret_ref() {
    let err = load_toml(
        "badsecret",
        "[lsp.servers.rust]\ncommand = \"ra\"\nenv = { K = \"${secret:nope}\" }\n",
    )
    .expect_err("未注册 secret 应 fail-fast");
    assert!(err.contains("nope"), "实际错误: {err}");
}

// ── 持久层往返 ──

#[test]
fn persistent_roundtrip_keeps_lsp_section() {
    let dir = std::env::temp_dir().join(format!("qaqh-lsp-cfg-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("config.toml");

    let mut servers = HashMap::new();
    servers.insert(
        "rust".to_owned(),
        PersistentLspServerConfig {
            command: Some("rust-analyzer".to_owned()),
            extensions: Some(vec!["rs".to_owned()]),
            ..Default::default()
        },
    );
    let pc = PersistentConfig {
        lsp: Some(PersistentLspConfig {
            enabled: Some(true),
            idle_shutdown_secs: Some(60),
            servers: Some(servers),
        }),
        ..Default::default()
    };

    let store = ConfigStore::new(path.clone());
    assert!(store.save(&pc), "save 应成功");

    let loaded = store.load().expect("roundtrip 后应能读回");
    let lsp = loaded.lsp.expect("lsp 段应保留");
    assert_eq!(lsp.enabled, Some(true));
    assert_eq!(lsp.idle_shutdown_secs, Some(60));
    let rust = lsp.servers.expect("servers 应保留").remove("rust").expect("rust 保留");
    assert_eq!(rust.command.as_deref(), Some("rust-analyzer"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 运行时类型守卫 ──

#[test]
fn runtime_default_matches_design() {
    let d = LspConfig::default();
    assert!(!d.enabled, "默认关闭（M1 落地后默认关一版）");
    assert_eq!(d.idle_shutdown_secs, 120);
    assert!(d.servers.is_empty());
}

// ── save 回写 ──

#[test]
fn save_roundtrip_keeps_lsp_servers() {
    let dir = std::env::temp_dir().join(format!("qaqh-lsp-cfg-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let store = ConfigStore::new(dir.join("config.toml"));
    let secrets = SecretStore::new(dir.join("secrets.toml"));

    let toml_text = r#"
[lsp]
enabled = true

[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]
"#;
    std::fs::write(store.path(), toml_text).expect("write config");
    let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load ok");
    cfg.save_with(&store, &secrets).expect("save ok");
    let reloaded = Config::load_from_paths_with(store, secrets).expect("reload ok");
    let rust = reloaded.lsp.servers.get("rust").expect("rust 往返保留");
    assert_eq!(rust.extensions, vec!["rs"]);
    let _ = std::fs::remove_dir_all(&dir);
}
