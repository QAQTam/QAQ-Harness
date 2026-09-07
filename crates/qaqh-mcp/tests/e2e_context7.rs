//! PR-M3-3 验收（PLAN §4 出口）：`QAQH_MCP_E2E=1 cargo test -p qaqh-mcp
//! --test e2e_context7` 通过（需网络 + node/npx；默认 skip——环境门控惯例，
//! 与 `QAQH_TEST_*` 家族一致：缺 env 时打印原因并返回，不算失败）。
//!
//! 验证对象：真实外部 MCP server（`npx -y @upstash/context7-mcp`）——stdio
//! 全链路（spawn → Auto 握手 → tools/list 缓存 → 投影批次含聚合工具 →
//! dispatch 往返）。context7 的工具集随上游变化，断言只锁定**结构不变量**
//! （≥1 个工具、名字以 mcp__e2e__ 前缀、echo 级别的 dummy 调用不做——
//! context7 的工具需要真实 API key，调用语义不属传输层验收）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::BTreeMap;
use std::time::Duration;

use qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind};
use qaqh_mcp::McpManager;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_context7_real_server() {
    if std::env::var("QAQH_MCP_E2E").as_deref() != Ok("1") {
        eprintln!(
            "skip: QAQH_MCP_E2E=1 required (downloads @upstash/context7-mcp via npx; needs network)"
        );
        return;
    }

    let mut cfg = McpConfig {
        enabled: true,
        idle_shutdown_secs: 0,
        servers: BTreeMap::new(),
        import_external: false,
    };
    cfg.servers.insert(
        "e2e".to_owned(),
        McpServerConfig {
            transport: McpTransportKind::Stdio,
            command: "npx".to_owned(),
            args: vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()],
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            tools: None,
            resources_enabled: true,
            default_timeout_secs: 120,
            max_concurrent_calls: 2,
        },
    );
    let settings = qaqh_mcp::connection::LifecycleSettings {
        connect_timeout: Duration::from_secs(120), // npx 首次下载可能很慢
        reconnect_cooldown: Duration::from_secs(5),
        close_timeout: Duration::from_secs(5),
        idle_tick: Duration::from_millis(100),
    };
    let manager = McpManager::with_settings(cfg, settings);

    // 连接 + tools/list 缓存（生产 adapter 全链路；无 factory override）。
    manager
        .get_or_connect("e2e")
        .await
        .expect("npx @upstash/context7-mcp must connect");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if manager
            .connection("e2e")
            .is_some_and(|conn| conn.cached_tools().is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let conn = manager.connection("e2e").expect("connected");
    let tools = conn.cached_tools().expect("tools cached");
    assert!(!tools.is_empty(), "context7 must expose at least one tool");

    // 投影批次：聚合工具 + e2e 工具（结构不变量，不锁上游工具名清单）。
    let batch = qaqh_mcp::bridge_for_tests::projection_batch_with(&manager)
        .expect("cache populated — batch must exist");
    let names: Vec<&str> = batch.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names[0], "mcp", "聚合工具钉底");
    assert!(
        names
            .iter()
            .skip(1)
            .all(|name| name.starts_with("mcp__e2e__")),
        "server 工具全部带 mcp__e2e__ 前缀: {names:?}"
    );
    assert!(names.len() >= 2, "聚合工具 + ≥1 context7 工具: {names:?}");

    // 优雅收尾：manager 离开作用域 → Drop 链（cancel → transport close →
    // 组杀兜底）即真实 RAII 验证；孤儿检查由 orphan_reap 模式覆盖。
    drop(batch);
    drop(manager);
}
