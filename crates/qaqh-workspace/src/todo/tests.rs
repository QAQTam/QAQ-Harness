use super::actions::{exec_todo_create, exec_todo_set, todo_set_for};
use super::model::{TodoStatus, TodoStore};
use super::parse::expand_todo_ids;
use super::store::{load_todo, read_store, save_todo, todo_path};
use serde_json::Value;

use super::*;
use std::ffi::OsString;

#[test]
fn todo_range_capped() {
    // 巨大范围必须报错而非物化（防 OOM/卡死 agent loop）。
    assert!(expand_todo_ids("T1-T4000000000").is_err());
    // 恰好等于上限的可接受（含端点 1000 个）。
    assert_eq!(expand_todo_ids("T1-T1000").unwrap().len(), 1000);
    // 超上限一个即拒绝。
    assert!(expand_todo_ids("T1-T1001").is_err());
    // 普通小范围不受影响（含端点）。
    assert_eq!(expand_todo_ids("T2-T4").unwrap(), vec!["T2", "T3", "T4"]);
}
/// 隔离数据目录（USERPROFILE/HOME → 临时目录）并设置会话上下文；
/// 结束恢复环境，避免污染真实 ~/.qaqh/sessions。
fn with_isolated_todo<F: FnOnce(&str)>(f: F) {
    let _guard = crate::TEST_RUNTIME_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let old_home: Option<OsString> = std::env::var_os(home_var);
    // Rust 2024: set_var/remove_var are unsafe (test-only, single-threaded via TEST_RUNTIME_SERIAL).
    unsafe { std::env::set_var(home_var, dir.path()) };
    let seed = format!("test-seed-{}", std::process::id());
    crate::runtime::set_context(&seed, 4);
    f(&seed);
    unsafe {
        match old_home {
            Some(value) => std::env::set_var(home_var, value),
            None => std::env::remove_var(home_var),
        }
    }
}

fn parse(result: &Result<String, String>) -> serde_json::Value {
    serde_json::from_str(result.as_ref().unwrap()).unwrap()
}

fn ids(store: &TodoStore) -> Vec<String> {
    store.items.iter().map(|item| item.id.clone()).collect()
}

#[test]
fn create_group_assigns_consecutive_ids_atomically() {
    with_isolated_todo(|_seed| {
        exec_todo_create(&serde_json::json!({"title": "single"}), false).unwrap();
        let result = exec_todo_create(
            &serde_json::json!({
                "items": [
                    {"title": "a"},
                    {"title": "b", "description": "desc b"},
                    {"title": "c"}
                ]
            }),
            false,
        );
        let value = parse(&result);
        let got: Vec<&str> = value["created"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect();
        assert_eq!(got, ["T2", "T3", "T4"]);
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T2", "T3", "T4"]);
    });
}

#[test]
fn create_group_is_atomic_on_validation_failure() {
    with_isolated_todo(|_seed| {
        let result = exec_todo_create(
            &serde_json::json!({
                "items": [{"title": "good"}, {"title": "   "}]
            }),
            false,
        );
        assert!(result.is_err());
        assert!(read_store().unwrap().items.is_empty());
    });
}

#[test]
fn create_rejects_empty_and_oversized_groups() {
    with_isolated_todo(|_seed| {
        assert!(exec_todo_create(&serde_json::json!({"items": []}), false).is_err());
        let items: Vec<Value> = (0..21)
            .map(|index| serde_json::json!({"title": format!("t{index}")}))
            .collect();
        assert!(exec_todo_create(&serde_json::json!({"items": items}), false).is_err());
        assert!(read_store().unwrap().items.is_empty());
    });
}

#[test]
fn insert_preserves_ids_and_changes_display_order() {
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({
                "items": [{"title": "a"}, {"title": "b"}]
            }),
            false,
        )
        .unwrap();
        let result = exec_todo_create(
            &serde_json::json!({"title": "child", "after_id": "T1"}),
            true,
        );
        assert_eq!(parse(&result)["created"][0]["id"], "T3");
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T3", "T2"]);
    });
}

#[test]
fn set_status_is_id_only_and_never_erases_metadata() {
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"title": "Keep me", "description": "Keep this too"}),
            false,
        )
        .unwrap();
        exec_todo_set(&serde_json::json!({
            "id": "T1",
            "status": "in_progress",
            "title": "",
            "description": ""
        }))
        .unwrap();
        exec_todo_set(&serde_json::json!({
            "id": "T1",
            "status": "completed",
            "evidence": "verified",
            "title": "",
            "description": ""
        }))
        .unwrap();
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "Keep me");
        assert_eq!(store.items[0].description, "Keep this too");
        assert_eq!(store.items[0].evidence.as_deref(), Some("verified"));
        assert_eq!(store.items[0].status, TodoStatus::Completed);
        assert!(store.current_id.is_none());
    });
}

#[test]
fn idle_is_public_alias_for_pending() {
    with_isolated_todo(|_seed| {
        exec_todo_create(&serde_json::json!({"title": "a"}), false).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "in_progress"})).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "idle"})).unwrap();
        assert_eq!(read_store().unwrap().items[0].status, TodoStatus::Pending);
    });
}

#[test]
fn current_id_falls_back_to_another_in_progress_task() {
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}]}),
            false,
        )
        .unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "in_progress"})).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T2", "status": "in_progress"})).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "completed"})).unwrap();
        assert_eq!(read_store().unwrap().current_id.as_deref(), Some("T2"));
    });
}

#[test]
fn insert_requires_exactly_one_existing_anchor() {
    with_isolated_todo(|_seed| {
        exec_todo_create(&serde_json::json!({"title": "parent"}), false).unwrap();

        let missing = exec_todo_create(&serde_json::json!({"title": "child"}), true);
        assert!(missing.is_err());

        let invalid = exec_todo_create(
            &serde_json::json!({"title": "child", "after_id": "not-an-id"}),
            true,
        );
        assert!(invalid.is_err());

        let absent = exec_todo_create(
            &serde_json::json!({"title": "child", "before_id": "T99"}),
            true,
        );
        assert!(absent.is_err());

        let conflicting = exec_todo_create(
            &serde_json::json!({
                "title": "child",
                "after_id": "T1",
                "before_id": "T1"
            }),
            true,
        );
        assert!(conflicting.is_err());
        assert_eq!(ids(&read_store().unwrap()), ["T1"]);
    });
}

#[test]
fn set_batch_ids_with_range_sets_same_status() {
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T1-T3"],
            "status": "completed"
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 3);
        assert_eq!(value["not_found"].as_array().unwrap().len(), 0);
        let store = read_store().unwrap();
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .count(),
            3
        );
        assert!(store.current_id.is_none());

        // 逗号列表 + 单个混合：T1,T3 + T2
        exec_todo_set(&serde_json::json!({
            "ids": ["T1,T3", "T2"],
            "status": "in_progress"
        }))
        .unwrap();
        let store = read_store().unwrap();
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
                .count(),
            3
        );
    });
}

#[test]
fn set_updates_parallel_sets_per_item_status() {
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        let result = exec_todo_set(&serde_json::json!({
            "updates": [
                {"id": "T1", "status": "completed"},
                {"id": "T2", "status": "in_progress"},
                {"id": "T3", "status": "cancelled", "evidence": "skip"}
            ]
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 3);
        let store = read_store().unwrap();
        assert_eq!(store.items[0].status, TodoStatus::Completed);
        assert_eq!(store.items[1].status, TodoStatus::InProgress);
        assert_eq!(store.items[2].status, TodoStatus::Cancelled);
        assert_eq!(store.items[2].evidence.as_deref(), Some("skip"));
        assert_eq!(store.current_id.as_deref(), Some("T2"));
    });
}

#[test]
fn set_batch_reports_not_found_without_aborting() {
    with_isolated_todo(|_seed| {
        exec_todo_create(&serde_json::json!({"title": "only"}), false).unwrap();
        // 部分命中：T1 更新，T2/T3 记入 not_found
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T1-T3"],
            "status": "completed"
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 1);
        assert_eq!(value["not_found"], serde_json::json!(["T2", "T3"]));
        assert_eq!(read_store().unwrap().items[0].status, TodoStatus::Completed);

        // 全部未命中 → 整体 NOT_FOUND
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T9-T10"],
            "status": "completed"
        }));
        assert!(result.is_err());
    });
}

#[test]
fn v2_field_guard_rejects_metadata_on_set() {
    let args = serde_json::json!({
        "action": "set",
        "id": "T1",
        "status": "completed",
        "title": ""
    });
    assert!(
        reject_fields(
            &args,
            &["title", "description", "items", "after_id", "before_id"],
            "set"
        )
        .is_err()
    );
}

#[test]
fn v1_aliases_retired() {
    // P2-6：create_batch/update/cancel V1 别名退役。web/TUI/测试零引用、
    // 工具描述从未宣传、journal 只存结果不重放调用——分发表只认
    // create/insert/set/list，别名必须落 INVALID_INPUT（防回归守卫）。
    for alias in ["create_batch", "update", "cancel"] {
        let ctx = crate::ToolCallCtx {
            id: format!("test-{alias}"),
            name: "todo".into(),
            action: alias.into(),
            args: serde_json::json!({ "action": alias }),
            tx_progress: None,
            timeout_secs: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let out = handle_todo(ctx);
        assert!(out.error.is_some(), "别名 {alias} 必须返回错误");
    }
}

#[test]
fn seed_parameterized_set_and_list_hit_explicit_seed() {
    // HTTP service 面 / CLI 直访契约：显式 seed 读写（write_store_for /
    // read_store_for），与工具路径（runtime ctx）写同一份 todo.json。
    with_isolated_todo(|seed| {
        exec_todo_create(&serde_json::json!({"title": "工具路径建项"}), false).unwrap();
        let id = ids(&load_todo().unwrap())[0].clone();
        let set = parse(&todo_set_for(
            seed,
            &serde_json::json!({"id": id, "status": "completed", "evidence": "via CLI"}),
        ));
        assert_eq!(set["item"]["status"], "completed");
        let listed = parse(&todo_list_for(seed, &serde_json::json!({})));
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["status"], "completed");
    });
}

#[test]
fn high_water_id_survives_item_removal() {
    // 回归：max+1 推导在"删除最大项后新建"会复用 ID，破坏 IDs stable。
    // 高水位持久化后：T1-T3 建成 → next_id=4 落盘 → 移除 T3 → 新建仍得 T4。
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        assert_eq!(read_store().unwrap().next_id, 4);
        let mut store = load_todo().unwrap();
        store.items.pop(); // 模拟未来的删除/归档：移除 T3
        save_todo(&store).unwrap();
        exec_todo_create(&serde_json::json!({"items": [{"title": "d"}]}), false).unwrap();
        assert_eq!(read_store().unwrap().items.last().unwrap().id, "T4");
    });
}

#[test]
fn legacy_store_without_next_id_migrates() {
    // 旧格式文件（无 next_id 字段）→ 首次分配按现存最大号 +1 迁移。
    with_isolated_todo(|_seed| {
        let legacy = serde_json::json!({
            "items": [
                {"id": "T1", "title": "a", "description": "", "status": "completed"},
                {"id": "T2", "title": "b", "description": "", "status": "pending"}
            ],
            "mode": "manual",
            "current_id": null,
            "auto_turns": 0,
            "max_auto_turns": 24
        });
        let path = todo_path().expect("test ctx has session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, legacy.to_string()).unwrap();
        exec_todo_create(&serde_json::json!({"items": [{"title": "c"}]}), false).unwrap();
        let store = read_store().unwrap();
        assert_eq!(store.items.last().unwrap().id, "T3");
        assert_eq!(store.next_id, 4);
    });
}

#[test]
fn set_edits_title_description_without_status() {
    // 纯编辑：updates 项可省略 status，仅改 title/description。
    with_isolated_todo(|_seed| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "原始", "description": "初版描述"}]}),
            false,
        )
        .unwrap();
        let result =
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": "改名", "description": "新验收标准"}]}))
                .unwrap();
        // 单条更新走 V1 兼容路径（item+message），非批量形态。
        assert!(result.contains("Todo T1 updated."));
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "改名");
        assert_eq!(store.items[0].description, "新验收标准");
        assert_eq!(store.items[0].status, TodoStatus::Pending, "纯编辑不改状态");

        // description 传空串 = 显式清空；title 空串拒绝。
        exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "description": ""}]})).unwrap();
        assert_eq!(read_store().unwrap().items[0].description, "");
        assert!(
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": ""}]})).is_err()
        );
        // 什么都不改的条目拒绝。
        assert!(exec_todo_set(&serde_json::json!({"updates": [{"id": "T1"}]})).is_err());
    });
}

#[test]
fn set_edit_rejects_oversized_title() {
    with_isolated_todo(|_seed| {
        exec_todo_create(&serde_json::json!({"items": [{"title": "a"}]}), false).unwrap();
        let long = "x".repeat(101);
        assert!(
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": long}]})).is_err()
        );
    });
}

// ═══════════════════════════════════════════════════════
// W1 拆分（PR-DT-1）：todo_create / todo_insert / todo_set / todo_list
// ═══════════════════════════════════════════════════════

use super::split::{handle_create, handle_insert, handle_list, handle_set};

fn parse_tool_result(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

fn split_ctx(name: &str, args: Value) -> crate::ToolCallCtx {
    crate::ToolCallCtx {
        id: format!("test-{name}"),
        name: name.into(),
        action: name.into(),
        args,
        tx_progress: None,
        timeout_secs: None,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

#[test]
fn split_handlers_reject_cross_fields_like_aggregate() {
    // 拆分薄壳的字段校验表必须与聚合 dispatch 各 action 同表：
    // 跨形态字段一律 INVALID_INPUT（聚合行为回归守卫）。
    assert!(
        handle_create(split_ctx(
            "todo_create",
            serde_json::json!({"title": "t", "id": "T1"})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_set(split_ctx(
            "todo_set",
            serde_json::json!({"items": [{"title": "t"}]})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_list(split_ctx("todo_list", serde_json::json!({"ids": ["T1"]})))
            .error
            .is_some()
    );
    assert!(
        handle_insert(split_ctx(
            "todo_insert",
            serde_json::json!({"title": "t", "status": "idle"})
        ))
        .error
        .is_some()
    );
}

#[test]
fn split_roundtrip_via_handlers() {
    with_isolated_todo(|_seed| {
        // create 单条
        let created = parse_tool_result(
            handle_create(split_ctx(
                "todo_create",
                serde_json::json!({"title": "first"}),
            ))
            .model_text(),
        );
        assert_eq!(created["created"][0]["id"], "T1");
        // insert 定位（after_id 必选其一）
        let inserted = parse_tool_result(
            handle_insert(split_ctx(
                "todo_insert",
                serde_json::json!({"title": "sub", "after_id": "T1"}),
            ))
            .model_text(),
        );
        assert_eq!(inserted["created"][0]["id"], "T2");
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T2"]);
        // set 批量同状态
        let set = parse_tool_result(
            handle_set(split_ctx(
                "todo_set",
                serde_json::json!({"ids": ["T1", "T2"], "status": "completed"}),
            ))
            .model_text(),
        );
        assert_eq!(set["updated"].as_array().unwrap().len(), 2);
        // list 过滤
        let listed = parse_tool_result(
            handle_list(split_ctx(
                "todo_list",
                serde_json::json!({"status": "completed"}),
            ))
            .model_text(),
        );
        assert_eq!(listed["items"].as_array().unwrap().len(), 2);
        let idle = parse_tool_result(
            handle_list(split_ctx("todo_list", serde_json::json!({}))).model_text(),
        );
        assert_eq!(idle["counts"]["total"], 2);
    });
}

#[test]
fn split_plan_blocked_and_conflict_semantics() {
    // 写类拆分工具受 plan 阻断；todo_list 是读，plan 模式放行。
    for blocked in ["todo_create", "todo_insert", "todo_set"] {
        assert!(
            crate::PLAN_BLOCKED.contains(&blocked),
            "{blocked} 应在 PLAN_BLOCKED"
        );
    }
    assert!(!crate::PLAN_BLOCKED.contains(&"todo_list"));
    // 冲突键：拆分工具与聚合同一合成键（store 是单一资源，保持同轮
    // 读写顺序约束与聚合行为一致）。
    for name in [
        "todo",
        "todo_create",
        "todo_insert",
        "todo_set",
        "todo_list",
    ] {
        let paths = crate::conflict::file_write_paths(name, &serde_json::json!({}));
        assert_eq!(paths, vec!["__qaqh_todo__".to_string()], "{name} 冲突键");
    }
}

#[test]
fn aggregate_todo_marked_deprecated_and_split_registered() {
    let mgr = crate::registration::build_tool_manager(&[]);
    let todo_desc = mgr
        .all_defs()
        .into_iter()
        .find(|def| def.function.name == "todo")
        .expect("聚合 todo 仍在场")
        .function
        .description;
    assert!(
        todo_desc.contains("Deprecated"),
        "聚合 description 应带 deprecated 标注"
    );
    for name in ["todo_create", "todo_insert", "todo_set", "todo_list"] {
        assert!(
            mgr.all_defs().iter().any(|def| def.function.name == name),
            "{name} 应已注册"
        );
    }
}
