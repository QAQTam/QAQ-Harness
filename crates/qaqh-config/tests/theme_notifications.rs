//! Regression test for backend issue #9:
//! PersistentConfig must carry theme + notifications_enabled,
//! old config files without these fields must keep loading.

use std::path::PathBuf;

fn setup(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-config-theme-notifications-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: this test file is a single serial test function; no parallel env var races.
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };
    root
}

fn write_config(root: &PathBuf, toml: &str) {
    std::fs::write(root.join("config.toml"), toml).unwrap();
}

#[test]
fn theme_and_notifications_roundtrip_and_legacy_compat() {
    // 1) New config: theme + notifications_enabled load correctly.
    let root = setup("roundtrip");
    write_config(
        &root,
        r#"provider_id = "deepseek"
endpoint = "openai"
max_tokens = 16384
theme = "dark"
notifications_enabled = false

[profiles.default]
model = "deepseek-chat"
max_tokens = 16384
effort = "high"
context_limit = 1000000
base_url = "https://api.deepseek.com"
endpoint = "openai"
"#,
    );
    let cfg = qaqh_config::Config::load().expect("load with new fields");
    assert_eq!(cfg.theme.as_deref(), Some("dark"));
    assert_eq!(cfg.notifications_enabled, Some(false));

    // 2) Update through the single write port and reload to verify persistence.
    let root = setup("update");
    write_config(
        &root,
        r#"provider_id = "deepseek"
endpoint = "openai"
max_tokens = 16384
"#,
    );
    let saved = qaqh_config::Config::update(|cfg| {
        cfg.theme = Some("dark-gray".to_string());
        cfg.notifications_enabled = Some(true);
        Ok(())
    })
    .expect("update ok");
    assert_eq!(saved.theme.as_deref(), Some("dark-gray"));
    assert_eq!(saved.notifications_enabled, Some(true));
    let reloaded = qaqh_config::Config::load().expect("reload after update");
    assert_eq!(reloaded.theme.as_deref(), Some("dark-gray"));
    assert_eq!(reloaded.notifications_enabled, Some(true));

    // 3) Empty theme means system default.
    let root = setup("empty-theme");
    write_config(
        &root,
        r#"provider_id = "deepseek"
endpoint = "openai"
max_tokens = 16384
theme = ""
notifications_enabled = false
"#,
    );
    let cfg = qaqh_config::Config::load().expect("load empty theme");
    assert_eq!(
        cfg.theme, None,
        "empty theme should normalize to None/system"
    );
    assert_eq!(cfg.notifications_enabled, Some(false));

    // 4) Legacy config without the new fields must still load.
    let root = setup("legacy");
    write_config(
        &root,
        r#"provider_id = "deepseek"
endpoint = "openai"
max_tokens = 16384
"#,
    );
    let cfg = qaqh_config::Config::load().expect("legacy load");
    assert_eq!(cfg.theme, None);
    assert_eq!(cfg.notifications_enabled, None);
}
