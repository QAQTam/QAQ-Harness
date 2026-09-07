//! PR-M1-3 验收（PLAN §4）：`cargo test -p qaqh-config --test secrets_mcp` 全绿。
//!
//! 覆盖（E-4/E-6）：
//! 1. `[secrets.mcp]` 通用命名段——set/has/load/list/delete 往返与隔离；
//! 2. 名字校验（与 server 名同规：`[a-z0-9_-]`、1..=64）；
//! 3. `${secret:name}` 占位符扫描/插值纯函数（命中/去重/残缺 fail-fast）；
//! 4. **load fail-fast**：env/args/headers 引用未注册 secret → load 即报
//!    （设计 §6：缺 key 不静默；不拖到首次调用）；
//! 5. **DTO/save 安全**：load 成功后 `Config.mcp` 仍持占位符原值，save 回写
//!    config.toml 只有占位符、绝无明文（审计 P0-1 前车之鉴）；
//! 6. DPAPI 往返（Windows 专项，本机 Linux 跳过——M1-5 双平台验收补跑）。
//!
//! 路径隔离手法沿用 mcp_config.rs（tempdir + 显式路径，无全局状态，
//! 无需 TEST_RUNTIME_SERIAL）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::path::{Path, PathBuf};

use qaqh_config::config::{Config, interpolate_secret_placeholders, secret_placeholder_names};
use qaqh_config::secrets::SecretStore;
use qaqh_types::ConfigStore;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qaqh-mcp-secrets-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn store_at(dir: &Path) -> SecretStore {
    SecretStore::new(dir.join("secrets.toml"))
}

fn load_config(dir: &Path) -> Result<Config, String> {
    let store = ConfigStore::new(dir.join("config.toml"));
    let secrets = store_at(dir);
    Config::load_from_paths_with(store, secrets)
}

// ── 1. [secrets.mcp] 命名段 ──

#[test]
fn named_secret_roundtrip_and_isolation() {
    let dir = temp_root("roundtrip");
    let store = store_at(&dir);

    assert!(!store.has_mcp("ctx_key"), "未注册 → has=false");
    assert!(store.load_mcp("ctx_key").is_none());
    assert!(store.list_mcp().is_empty());

    store.set_mcp("ctx_key", "sk-one").expect("set ctx_key");
    store
        .set_mcp("figma_pat", "pat-two")
        .expect("set figma_pat");
    assert_eq!(store.load_mcp("ctx_key").as_deref(), Some("sk-one"));
    assert_eq!(store.load_mcp("figma_pat").as_deref(), Some("pat-two"));
    assert_eq!(
        store.list_mcp(),
        vec!["ctx_key", "figma_pat"],
        "list 排序输出"
    );

    // 重写不丢邻居；删除只删自身。
    store.set_mcp("ctx_key", "sk-one-2").expect("re-set");
    assert_eq!(store.load_mcp("figma_pat").as_deref(), Some("pat-two"));
    store.delete_mcp("ctx_key").expect("delete");
    assert!(!store.has_mcp("ctx_key"));
    assert!(store.load_mcp("ctx_key").is_none());
    assert_eq!(store.load_mcp("figma_pat").as_deref(), Some("pat-two"));
    // 段不存在时 delete 幂等。
    store.delete_mcp("ctx_key").expect("idempotent delete");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn named_secret_rejects_invalid_names() {
    let dir = temp_root("badname");
    let store = store_at(&dir);

    for bad in [
        "",
        "UPPER",
        "with space",
        "dot.name",
        "中文",
        &"a".repeat(65),
    ] {
        assert!(store.set_mcp(bad, "v").is_err(), "非法名应被拒绝：{bad:?}");
    }
    // 读侧对非法名安全降级（None/false），不 panic。
    assert!(!store.has_mcp("UPPER"));
    assert!(store.load_mcp("UPPER").is_none());
    // 合法边界：64 字符、'-'/'_'。
    let edge = format!("a{}", "-_".repeat(31)); // 2+62=64
    store.set_mcp(&edge, "v").expect("64 字符合法");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn dpapi_roundtrip_named_secret() {
    let dir = temp_root("dpapi");
    let store = store_at(&dir);
    store.set_mcp("win_key", "sk-win-secret").expect("set");
    let raw = std::fs::read_to_string(store.path()).expect("read secrets.toml");
    assert!(
        raw.contains("dpapi:"),
        "windows 下 [secrets.mcp] 值应为 dpapi blob"
    );
    assert!(!raw.contains("sk-win-secret"), "明文绝不能落盘");
    assert_eq!(store.load_mcp("win_key").as_deref(), Some("sk-win-secret"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3. 占位符纯函数 ──

#[test]
fn placeholder_scan_hits_dedups_and_fails_fast() {
    let names =
        secret_placeholder_names("a=${secret:one} b=${secret:two} c=${secret:one}").unwrap();
    assert_eq!(names, vec!["one", "two"], "出现顺序去重");

    assert_eq!(
        secret_placeholder_names("no placeholder").unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        secret_placeholder_names("Bearer ${secret:k}!").unwrap(),
        vec!["k"]
    );

    // 残缺/空名/非法字符一律 Err（fail-fast）。
    assert!(secret_placeholder_names("${secret:").is_err(), "未闭合");
    assert!(secret_placeholder_names("${secret:}").is_err(), "空名");
    assert!(secret_placeholder_names("${secret:Bad}").is_err(), "大写");
    assert!(secret_placeholder_names("${secret:a b}").is_err(), "空格");
    assert!(
        secret_placeholder_names("${secret:${secret:a}}").is_err(),
        "嵌套"
    );
}

#[test]
fn interpolate_replaces_all_and_never_leaks_on_miss() {
    let resolve = |name: &str| match name {
        "k1" => Some("v-1".to_owned()),
        _ => None,
    };

    assert_eq!(
        interpolate_secret_placeholders("${secret:k1}+${secret:k1}", resolve).unwrap(),
        "v-1+v-1",
        "同名多出处全部替换"
    );
    assert_eq!(
        interpolate_secret_placeholders("Bearer ${secret:k1}", resolve).unwrap(),
        "Bearer v-1"
    );
    // 无占位符：原样返回，resolver 不该被调（无占位符路径提前返回）。
    assert_eq!(
        interpolate_secret_placeholders("plain", resolve).unwrap(),
        "plain"
    );
    // 缺失：Err 含名字、不含值。
    let err = interpolate_secret_placeholders("${secret:missing}", resolve).unwrap_err();
    assert!(err.contains("missing"), "报错应指明 secret 名：{err}");
    assert!(!err.contains("v-1"), "已解析值不能进报错");
}

// ── 4/5. load fail-fast + DTO/save 安全 ──

const CONFIG_WITH_PLACEHOLDER: &str = r#"
[mcp]
enabled = true
idle_shutdown_secs = 60

[mcp.servers.demo]
command = "npx"
args = ["-y", "@demo/server", "--key=${secret:demo_key}"]
env = { API_KEY = "${secret:demo_key}", MODE = "production" }
headers = { Authorization = "Bearer ${secret:demo_key}" }
"#;

#[test]
fn load_fails_fast_on_missing_secret_ref() {
    let dir = temp_root("failfast");
    std::fs::write(dir.join("config.toml"), CONFIG_WITH_PLACEHOLDER).expect("write config");

    let err = load_config(&dir).expect_err("未注册引用应 fail-fast");
    assert!(
        err.contains("demo_key"),
        "报错应指明缺失的 secret 名：{err}"
    );
    assert!(err.contains("已注册"), "报错应列出已注册名单提示");

    // 注册后 load 成功。
    store_at(&dir)
        .set_mcp("demo_key", "sk-demo-0001")
        .expect("register");
    let cfg = load_config(&dir).expect("注册后 load 应成功");
    let server = cfg.mcp.servers.get("demo").expect("demo server");
    assert_eq!(
        server.env.get("API_KEY").map(String::as_str),
        Some("${secret:demo_key}"),
        "Config.mcp 必须保留占位符原值（DTO/save 安全，明文只在连接时进子进程 env）"
    );
    assert_eq!(
        server.env.get("MODE").map(String::as_str),
        Some("production")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_scans_env_args_and_headers() {
    // args 里引用未注册 secret → fail-fast。
    let dir = temp_root("scan-args");
    std::fs::write(
        dir.join("config.toml"),
        r#"
[mcp.servers.a]
command = "npx"
args = ["--token=${secret:tok}"]
"#,
    )
    .expect("write");
    assert!(
        load_config(&dir).is_err(),
        "args 中的未注册引用应 fail-fast"
    );

    // headers 里引用未注册 secret → fail-fast。
    let dir = temp_root("scan-headers");
    std::fs::write(
        dir.join("config.toml"),
        r#"
[mcp.servers.h]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer ${secret:pat}" }
"#,
    )
    .expect("write");
    assert!(
        load_config(&dir).is_err(),
        "headers 中的未注册引用应 fail-fast"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_placeholder_fails_load() {
    let dir = temp_root("malformed");
    std::fs::write(
        dir.join("config.toml"),
        r#"
[mcp.servers.bad]
command = "npx"
env = { KEY = "${secret:Bad Name}" }
"#,
    )
    .expect("write");
    let err = load_config(&dir).expect_err("残缺占位符应 fail-fast");
    assert!(err.contains("非法"), "应提示占位符名非法：{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_roundtrip_keeps_placeholder_never_plaintext() {
    let dir = temp_root("save-keep");
    std::fs::write(dir.join("config.toml"), CONFIG_WITH_PLACEHOLDER).expect("write");
    store_at(&dir)
        .set_mcp("demo_key", "sk-demo-SAVE-0001")
        .expect("register");

    let cfg = load_config(&dir).expect("load");
    // save 回同一路径（对称 save_with；公开版与 load_from_paths_with 配对）。
    let store = ConfigStore::new(dir.join("config.toml"));
    cfg.save_with(&store, &store_at(&dir)).expect("save");

    let saved = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
    assert!(
        saved.contains("${secret:demo_key}"),
        "save 必须保留占位符（而非解析值）：{saved}"
    );
    assert!(
        !saved.contains("sk-demo-SAVE-0001"),
        "save 绝不能把明文写进 config.toml（审计 P0-1 前车之鉴）"
    );
    // secrets.toml 不受 save 影响，仍可解密。
    assert_eq!(
        store_at(&dir).load_mcp("demo_key").as_deref(),
        Some("sk-demo-SAVE-0001")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
