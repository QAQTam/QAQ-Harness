//! Regression: config.save with an empty/masked apiKey must NOT delete the
//! stored key (Bug 根因 1).
//!
//! Previously the winui frontend always sent `apiKey:""` on save (the daemon
//! masks a configured key as "****", which the frontend parses back to ""),
//! and the daemon treated an empty string as explicit deletion — so ANY save
//! wiped the credentials.
//!
//! New contract: only a non-empty, non-"****" apiKey updates the secret;
//! empty/masked keeps the existing value. Explicit deletion needs a dedicated
//! interface (not yet implemented).

use std::path::PathBuf;

use qaqh_runtime::QaqhService;
use serde_json::json;

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-config-api-key-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

#[test]
fn empty_or_masked_api_key_keeps_existing_secret() {
    let root = temp_root("keep");
    // SAFETY: integration-test process is single-purpose; env is reset by exit.
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };

    let service = QaqhService::init();

    // 1) initial save with a real key
    service
        .handle("config.save", &json!({ "apiKey": "sk-1", "model": "m1" }))
        .expect("save with key");

    let cfg = qaqh_config::Config::load().expect("reload");
    assert_eq!(cfg.api_key, "sk-1");
    assert_eq!(cfg.model, "m1");

    // 2) empty apiKey must preserve the existing key
    service
        .handle("config.save", &json!({ "apiKey": "" }))
        .expect("save with empty apiKey");
    let cfg = qaqh_config::Config::load().expect("reload");
    assert_eq!(cfg.api_key, "sk-1", "empty apiKey must not delete the key");
    assert_eq!(cfg.model, "m1", "unrelated fields must survive");

    // 3) masked placeholder must preserve the existing key
    service
        .handle("config.save", &json!({ "apiKey": "****" }))
        .expect("save with masked apiKey");
    let cfg = qaqh_config::Config::load().expect("reload");
    assert_eq!(cfg.api_key, "sk-1", "masked apiKey must not delete the key");

    // 4) a real new key still updates
    service
        .handle("config.save", &json!({ "apiKey": "sk-2" }))
        .expect("save with new key");
    let cfg = qaqh_config::Config::load().expect("reload");
    assert_eq!(cfg.api_key, "sk-2", "new apiKey must replace");

    let _ = std::fs::remove_dir_all(root);
}
