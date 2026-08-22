//! 回归测试：自定义 base_url（端点）在 load / apply_profile 后必须保留。
//!
//! 背景（Bug#1）：前端修改 max_tokens 保存后，provider 端点被强制改回 registry
//! 预设。根因是后端 `Config::load()` / `apply_profile()` 在"已保存值 ≠ endpoint
//! 预设"时无条件把 base_url 覆盖为预设（apply_profile 还会落盘，造成数据丢失）。
//! 修复原则：预设仅作空值兜底——配置文件为空（base_url 缺失）时才预设，
//! 用户已保存的值（含自定义 URL）绝不覆盖。
//!
//! 注意：QAQH_DATA_DIR 是进程级环境变量，多个 #[test] 并行会互相污染，
//! 因此所有场景在单个测试函数内串行执行。

use std::path::PathBuf;

fn setup(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-config-base-url-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp data root");
    // SAFETY: 单线程串行执行，设置后立即使用，无并发读
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };
    root
}

fn write_config(root: &PathBuf, toml: &str) {
    std::fs::write(root.join("config.toml"), toml).expect("write test config");
}

const CUSTOM_URL: &str = "https://opencode.ai/zen/go/v1";
const DEEPSEEK_PRESET: &str = "https://api.deepseek.com";

/// 用户真实配置：deepseek/openai + 自定义 base_url + 大 max_tokens。
const USER_CONFIG: &str = r#"provider_id = "deepseek"
api_key = "sk-test"
model = "deepseek-v4-flash"
base_url = "https://opencode.ai/zen/go/v1"
max_tokens = 324000
context_limit = 1000000
endpoint = "openai"
reasoning_effort = "max"
active_profile = "default"
lang = "zh"
compliance_enabled = true
permission_level = 4
auto_compact_threshold = 0.75

[profiles.default]
model = "deepseek-v4-flash"
max_tokens = 324000
effort = "max"
context_limit = 1000000
base_url = "https://opencode.ai/zen/go/v1"
endpoint = "openai"

[subagent]
max_tokens = 128000
timeout_secs = 120
default_tools = ["file", "exec"]

[multimodal]
enabled = false
provider_type = "mimo"
provider_id = "mimo"
model = "mimo-v2.5"
max_tokens = 4096

[workspace]
mode = "local"
"#;

#[test]
fn base_url_preset_only_when_empty() {
    // 1) 用户真实配置：load 必须保留自定义 base_url
    let root = setup("load");
    write_config(&root, USER_CONFIG);
    let cfg = qaqh_config::Config::load().expect("load ok");
    assert_eq!(
        cfg.base_url, CUSTOM_URL,
        "load() 把自定义 base_url 强制改回了预设端点"
    );
    assert_eq!(cfg.max_tokens, 324000);
    assert_eq!(
        cfg.subagent.default_tools,
        vec!["read".to_string(), "exec".to_string()],
        "存量配置的旧工具名 file 必须迁移为 read"
    );

    // 2) apply_profile 不得破坏自定义 base_url（此前会强制预设并落盘）。
    // 单写口：profile 切换必须经 Config::update 持久化。
    let root = setup("apply");
    write_config(&root, USER_CONFIG);
    let saved = qaqh_config::Config::update(|cfg| {
        cfg.apply_profile("default")
            .map(|_| ())
            .ok_or_else(|| "profile default missing".to_string())
    })
    .expect("update ok");
    assert_eq!(saved.base_url, CUSTOM_URL);
    let reloaded = qaqh_config::Config::load().expect("reload ok");
    assert_eq!(
        reloaded.base_url, CUSTOM_URL,
        "apply_profile 把自定义 base_url 强制改回预设并落盘"
    );

    // 3) 空配置（有 provider/endpoint 但无 base_url）→ 预设兜底仍生效
    let root = setup("empty");
    write_config(
        &root,
        r#"provider_id = "deepseek"
endpoint = "openai"
max_tokens = 16384
"#,
    );
    let cfg = qaqh_config::Config::load().expect("load ok");
    assert_eq!(
        cfg.base_url, DEEPSEEK_PRESET,
        "空配置（无 base_url）应回退到 endpoint 预设"
    );

    // 4) 完全没有配置文件 → Config::default()（first provider 预设）
    let root = setup("missing");
    let _ = root; // 不写文件
    let cfg = qaqh_config::Config::load().expect("load ok");
    assert!(!cfg.base_url.is_empty(), "无配置文件时应带预设 base_url");

    // 5) 用户显式保存的 base_url 恰为预设值 → load 后保持一致（无漂移）
    let root = setup("preset");
    write_config(
        &root,
        &format!(
            r#"provider_id = "deepseek"
endpoint = "openai"
base_url = "{DEEPSEEK_PRESET}"
max_tokens = 16384

[profiles.default]
model = "deepseek-chat"
max_tokens = 16384
effort = "high"
context_limit = 1000000
base_url = "{DEEPSEEK_PRESET}"
endpoint = "openai"
"#
        ),
    );
    let cfg = qaqh_config::Config::load().expect("load ok");
    assert_eq!(cfg.base_url, DEEPSEEK_PRESET);

    // 6) 子代理自定义 base_url 必须保留
    let root = setup("subagent");
    write_config(
        &root,
        &format!(
            r#"provider_id = "deepseek"
endpoint = "openai"
base_url = "{CUSTOM_URL}"
max_tokens = 16384

[subagent]
base_url = "{CUSTOM_URL}"
max_tokens = 128000
timeout_secs = 120
default_tools = ["file", "exec"]
"#
        ),
    );
    let cfg = qaqh_config::Config::load().expect("load ok");
    assert_eq!(cfg.subagent.base_url, CUSTOM_URL);
    assert_eq!(
        cfg.subagent.default_tools,
        vec!["read".to_string(), "exec".to_string()],
        "存量配置的旧工具名 file 必须迁移为 read"
    );

    // 7) Config::update 是受锁单写口：局部修改不会丢其它字段。
    let root = setup("update");
    write_config(&root, USER_CONFIG);
    let saved = qaqh_config::Config::update(|cfg| {
        cfg.max_tokens = 123456;
        Ok(())
    })
    .expect("update ok");
    assert_eq!(saved.max_tokens, 123456);
    let reloaded = qaqh_config::Config::load().expect("reload ok");
    assert_eq!(reloaded.max_tokens, 123456);
    assert_eq!(reloaded.base_url, CUSTOM_URL, "unrelated fields must survive update");
    assert_eq!(reloaded.permission_level, 4);

    // 8) update 的 mutator 返回 Err 时不得写盘（事务语义）。
    let root = setup("update-err");
    write_config(&root, USER_CONFIG);
    let err = qaqh_config::Config::update(|_| Err("reject mutation".to_string()))
        .expect_err("mutation error must propagate");
    assert_eq!(err, "reject mutation");
    let unchanged = qaqh_config::Config::load().expect("reload ok");
    assert_eq!(unchanged.max_tokens, 324000, "failed update must not write");
}
