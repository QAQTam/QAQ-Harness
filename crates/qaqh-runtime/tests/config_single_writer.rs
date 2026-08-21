//! BUG-001/008 regression: every daemon config action must go through the
//! same `Config::update` write port, so later actions cannot overwrite fields
//! written by earlier actions.

use std::path::PathBuf;

use qaqh_runtime::QaqhService;
use serde_json::json;

fn temp_root() -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("qaqh-config-single-writer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

#[test]
fn daemon_config_actions_share_one_write_port() {
    let root = temp_root();
    // SAFETY: integration-test process is single-purpose; the env is read by
    // ConfigStore::default_location during the test and reset by process exit.
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };

    let service = QaqhService::init();

    service
        .handle(
            "config.save",
            &json!({
                "base_url": "https://custom.example/v1",
                "max_tokens": 123456,
            }),
        )
        .expect("config.save");

    service
        .handle("config.set_permission_level", &json!({ "level": 2 }))
        .expect("permission update");

    let cfg = qaqh_config::Config::load().expect("reload config");
    assert_eq!(cfg.permission_level, 2);
    assert_eq!(
        cfg.max_tokens, 123456,
        "permission write must not drop max_tokens"
    );
    assert_eq!(
        cfg.base_url, "https://custom.example/v1",
        "permission write must not drop base_url"
    );

    service
        .handle("workspace.set_mode", &json!({ "mode": "local" }))
        .expect("workspace mode");
    let cfg = qaqh_config::Config::load().expect("reload config");
    assert_eq!(cfg.workspace.mode, "local");
    assert_eq!(
        cfg.permission_level, 2,
        "workspace write must not drop permission"
    );
    assert_eq!(
        cfg.max_tokens, 123456,
        "workspace write must not drop max_tokens"
    );

    let _ = std::fs::remove_dir_all(root);
}
