//! PR-M1-4 验收（PLAN §4 出口）：`cargo test -p qaqh-workspace --test
//! dynamic_registration` 全绿（前缀/碰撞/白名单过滤/截断）。
//!
//! 覆盖面（设计 §5.3/S2/E-5）：
//! 1. **前缀**：`build_dynamic_tool` 产出 `mcp__{server}__{tool}` 命名；
//! 2. **碰撞拒绝**：动态↔内置、动态↔动态重名 → Err 且不写入；
//! 3. **白名单过滤**：tool_mode `custom` 的 allowlist 用完整前缀名过滤
//!    （`set_allowed` 的 known 语义覆盖动态层；`filtered_defs` 按完整名）；
//! 4. **截断**：description 2KB 上限（字符边界 + `[truncated]` 标记）；
//! 5. 模型面合并（all_defs = 内置 + 动态）、category 两层查询、
//!    clear_dynamic 全量重建语义。
//!
//! prepare 路由（pub(crate)）由 `src/manager.rs` 内联测试
//! `prepare_routes_dynamic_tool_to_injected_fn` 覆盖（同 crate 可达）。
//! 全部状态测试私有，无需 TEST_RUNTIME_SERIAL。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::time::Duration;

use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{
    DYNAMIC_DESCRIPTION_LIMIT, MCP_DYNAMIC_PREFIX, ToolCallCtx, ToolHandler, ToolManager,
    ToolPlacement, ToolResult, ToolRisk, build_dynamic_tool,
};

fn noop(_ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok("noop")
}

fn builtin(key: &str) -> ToolHandler {
    ToolHandler {
        key: key.to_owned(),
        description: "builtin",
        input_schema: serde_json::json!({ "type": "object" }),
        handler: noop,
        risk: ToolRisk::ReadOnly,
        category: ToolCategory::Read,
        default_timeout: Duration::from_secs(5),
    }
}

fn echo_entry(
    server: &str,
    tool: &str,
    description: &str,
) -> (String, qaqh_workspace::DynamicTool) {
    build_dynamic_tool(
        server,
        tool,
        description,
        serde_json::json!({ "type": "object", "properties": {} }),
        noop,
        ToolPlacement::HostOnly,
        ToolCategory::Exec,
        Duration::from_secs(30),
    )
}

// ── 1. 前缀命名 + 模型面合并 ──

#[test]
fn mcp_prefix_and_model_face_merge() {
    let mut mgr = ToolManager::new();
    let (name, tool) = echo_entry("demo", "echo", "echo back");
    assert_eq!(name, "mcp__demo__echo", "S2 前缀命名");
    mgr.register_dynamic(name, tool).expect("register");

    let defs = mgr.all_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].function.name, "mcp__demo__echo");
    assert_eq!(
        defs[0].function.description, "echo back",
        "未超限描述直通（schema 同为直通，见 parameters 断言）"
    );
    assert!(defs[0].function.parameters.is_object());

    // 全量重建：clear 后动态层清空（M2 tools/list_changed 的入口语义）。
    mgr.clear_dynamic();
    assert!(mgr.all_defs().is_empty());
    assert!(mgr.dynamic_names().is_empty());
}

// ── 2. 碰撞拒绝 ──

#[test]
fn collision_rejected_against_dynamic_and_builtin() {
    let mut mgr = ToolManager::new();
    mgr.register(builtin("read"));

    // 动态↔内置：与真实注册的内置名撞 → Err。
    let (_, clashing) = echo_entry("read", "x", "");
    let err = mgr
        .register_dynamic("read".to_owned(), clashing)
        .expect_err("与内置撞名应拒绝");
    assert!(err.contains("collides"), "{err}");

    // 动态↔动态：同名两次注册，第二次 Err 且第一次仍在。
    let (name, tool) = echo_entry("demo", "echo", "first");
    mgr.register_dynamic(name.clone(), tool).expect("first ok");
    let (_, dup) = echo_entry("demo", "echo", "second");
    let err = mgr.register_dynamic(name, dup).expect_err("重复注册应拒绝");
    assert!(err.contains("collides"));
    assert_eq!(
        mgr.all_defs()
            .last()
            .map(|d| d.function.description.as_str()),
        Some("first"),
        "拒绝不得写入"
    );

    // S2 前缀保证零碰撞：注册表里的动态名全部带前缀。
    assert!(
        !mgr.dynamic_names()
            .iter()
            .any(|n| !n.starts_with(MCP_DYNAMIC_PREFIX))
    );
}

// ── 3. 白名单过滤（tool_mode custom 用完整前缀名）──

#[test]
fn allowed_list_filters_by_full_prefixed_name() {
    let mut mgr = ToolManager::new();
    for (server, tool) in [("demo", "echo"), ("demo", "slow"), ("other", "ping")] {
        let (name, entry) = echo_entry(server, tool, "t");
        mgr.register_dynamic(name, entry).expect("register");
    }

    // custom 模式：只放行完整前缀名；未知名被剔除。
    mgr.set_allowed(vec!["mcp__demo__echo".to_owned(), "ghost".to_owned()]);
    let names: Vec<String> = mgr
        .filtered_defs()
        .into_iter()
        .map(|d| d.function.name)
        .collect();
    assert_eq!(names, vec!["mcp__demo__echo"]);

    // 全部未知 → 回退全量（宁全开不瘫痪，既有语义）。
    mgr.set_allowed(vec!["nope".to_owned()]);
    assert_eq!(mgr.filtered_defs().len(), 3);

    // 标准模式（空列表）→ 全量。
    mgr.set_allowed(vec![]);
    assert_eq!(mgr.filtered_defs().len(), 3);
}

// ── 4. 描述截断（2KB 上限，字符边界 + 标记）──

#[test]
fn description_truncates_at_char_boundary_with_marker() {
    let mut mgr = ToolManager::new();

    // 多字节字符超限：'汉' = 3 bytes × 1000 = 3000 bytes > 2048。
    let (name, tool) = echo_entry("demo", "big", &"汉".repeat(1000));
    mgr.register_dynamic(name, tool).expect("register");

    // ASCII 超限。
    let (name2, tool2) = echo_entry("demo", "ascii", &"a".repeat(3000));
    mgr.register_dynamic(name2, tool2).expect("register");

    // 未超限（直通对照组）。
    let (name3, tool3) = echo_entry("demo", "small", "short");
    mgr.register_dynamic(name3, tool3).expect("register");

    let defs = mgr.all_defs();
    let big = defs
        .iter()
        .find(|d| d.function.name == "mcp__demo__big")
        .unwrap();
    let ascii = defs
        .iter()
        .find(|d| d.function.name == "mcp__demo__ascii")
        .unwrap();
    let small = defs
        .iter()
        .find(|d| d.function.name == "mcp__demo__small")
        .unwrap();

    for (label, def) in [("cjk", big), ("ascii", ascii)] {
        assert!(
            def.function.description.len() <= DYNAMIC_DESCRIPTION_LIMIT,
            "{label} 截断后不超过 2KB：{}",
            def.function.description.len()
        );
        assert!(
            def.function.description.ends_with(" [truncated]"),
            "{label} 应带截断标记"
        );
    }
    // 字符边界：'汉' 3 bytes，头部应为完整字符序列。
    let head_len = big.function.description.len() - " [truncated]".len();
    assert_eq!(head_len % 3, 0);
    // 未超限原样。
    assert_eq!(small.function.description, "short");
}

// ── 5. category 两层查询（S3 沙箱读取路径）──

#[test]
fn category_of_covers_builtin_and_dynamic() {
    let mut mgr = ToolManager::new();
    mgr.register(builtin("read"));
    let (name, tool) = echo_entry("demo", "echo", "d");
    mgr.register_dynamic(name, tool).expect("register");

    assert_eq!(mgr.category_of("read"), Some(ToolCategory::Read));
    assert_eq!(mgr.category_of("mcp__demo__echo"), Some(ToolCategory::Exec));
    assert_eq!(mgr.category_of("unknown"), None);
}

// ── 6. 回合边界 refresh（replace_dynamic_tools：clear + 全量重建，M1-5）──

#[test]
fn replace_dynamic_tools_clears_and_rebuilds() {
    // runtime::replace_dynamic_tools 操作 thread-local actor manager——
    // 测试线程安装私有 manager，用完 clear 回退进程 manager（actor 语义一致）。
    qaqh_workspace::runtime::install_actor_tool_manager(ToolManager::new());

    let batch1 = vec![
        echo_entry("demo", "echo", "v1"),
        echo_entry("demo", "slow", "v1"),
    ];
    assert_eq!(qaqh_workspace::runtime::replace_dynamic_tools(batch1), 0);
    let dynamic_names = |expected: &[&str]| {
        let names: Vec<String> = qaqh_workspace::runtime::all_tools()
            .into_iter()
            .filter(|def| def.function.name.starts_with(MCP_DYNAMIC_PREFIX))
            .map(|def| def.function.name)
            .collect();
        assert_eq!(names, expected);
    };
    dynamic_names(&["mcp__demo__echo", "mcp__demo__slow"]);

    // 第二代批次（server 缩到只剩 echo）：clear + 重建后动态层只剩新集合。
    let batch2 = vec![echo_entry("demo", "echo", "v2")];
    assert_eq!(qaqh_workspace::runtime::replace_dynamic_tools(batch2), 0);
    dynamic_names(&["mcp__demo__echo"]);

    // 完整名不同的两条（demo/echo vs other/echo）互不冲突，零拒绝。
    let batch3 = vec![
        echo_entry("demo", "echo", "v3"),
        echo_entry("other", "echo", "v3"),
    ];
    assert_eq!(qaqh_workspace::runtime::replace_dynamic_tools(batch3), 0);
    dynamic_names(&["mcp__demo__echo", "mcp__other__echo"]);

    qaqh_workspace::runtime::clear_actor_tool_manager();
}
