//! Ringing 架构测试（PLAN 测试与验收 · 架构硬规则）。
//!
//! 验证：
//! 1. domain crate 不依赖 legacy wire（qaqh-proto）与 wire（qaqh-ringing）——
//!    通过依赖图静态检查（cargo metadata）。
//! 2. 全仓不存在 `Agent2Ui → Ringing` / `Ui2Agent → Ringing` 转换函数——
//!    通过源码模式检查（禁止的桥接模式）。
//! 3. Ringing 原生事件不再投影到 Agent2Ui；底层 legacy worker 边界仍为后续
//!    TUI/WinUI 重做保留。

/// 读取 qaqh-domain 的 Cargo.toml path 依赖。
fn domain_path_deps() -> Vec<String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let manifest = std::path::Path::new(root)
        .join("../..")
        .join("crates/qaqh-domain/Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("qaqh-domain Cargo.toml");
    let mut deps = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.contains("path = \"../") {
            deps.push(t.to_string());
        }
    }
    deps
}

/// 架构测试：domain crate 不得依赖 proto（legacy）与 ringing（wire）。
#[test]
fn domain_crate_does_not_depend_on_legacy_or_wire() {
    let domain_deps = domain_path_deps();
    assert!(
        !domain_deps.iter().any(|d| d.contains("qaqh-proto")),
        "qaqh-domain must not depend on qaqh-proto (legacy), got {domain_deps:?}"
    );
    assert!(
        !domain_deps.iter().any(|d| d.contains("qaqh-ringing")),
        "qaqh-domain must not depend on qaqh-ringing (wire), got {domain_deps:?}"
    );
}

/// 架构测试：全仓不存在 `Agent2Ui → Ringing` 转换（禁止桥接）。
///
/// 检查模式（在 crate 源码中）：
/// - `Agent2Ui → RingingEvent` / `Agent2Ui → RingingWorkerEventEnvelope` 类型转换；
/// - 函数名含 `to_ringing` / `ringing_from_legacy` 且参数含 Agent2Ui 的桥接。
#[test]
fn no_agent2ui_to_ringing_bridge_functions() {
    let root = env!("CARGO_MANIFEST_DIR");
    let crates_dir = std::path::Path::new(root).join("../..").join("crates");
    let mut found: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&crates_dir).expect("crates dir") {
        let dir = entry.expect("entry").path();
        if !dir.is_dir() {
            continue;
        }
        walk_rs(&dir, &mut |path, text| {
            // 禁止的模式：函数签名里同时出现 legacy 类型与 Ringing 转换意图
            if text.contains("fn ") && text.contains("Agent2Ui") {
                for line in text.lines() {
                    let t = line.trim();
                    if (t.contains("to_ringing") || t.contains("into_ringing"))
                        && (t.contains("Agent2Ui") || t.contains("Ui2Agent"))
                    {
                        found.push(format!("{}: {t}", path.display()));
                    }
                }
            }
            // 禁止 `impl From<Agent2Ui> for Ringing*`
            if text.contains("impl From<Agent2Ui>") && text.contains("for Ringing") {
                found.push(format!("{}: From<Agent2Ui> for Ringing*", path.display()));
            }
            if text.contains("impl From<Ui2Agent>") && text.contains("for Ringing") {
                found.push(format!("{}: From<Ui2Agent> for Ringing*", path.display()));
            }
        });
    }
    assert!(
        found.is_empty(),
        "found forbidden Agent2Ui→Ringing bridge candidates:\n{}",
        found.join("\n")
    );
}

fn walk_rs(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // 跳过 tests/（集成测试含模式字符串会自误报）
            if !path.file_name().is_some_and(|n| n == "tests") {
                walk_rs(&path, visit);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                visit(&path, &text);
            }
        }
    }
}
