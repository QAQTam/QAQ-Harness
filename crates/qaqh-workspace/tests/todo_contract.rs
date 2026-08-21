#[test]
fn public_schema_exposes_one_todo_tool_with_no_alias_and_no_goal_entrypoint() {
    let manager = qaqh_workspace::registration::build_tool_manager(&[]);
    let definitions = manager.all_defs();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect();

    // 主工具：todo（prompt/文档统一命名）。
    assert!(names.contains(&"todo"), "missing todo tool");
    // 旧名别名 task 已移除：公开 schema 不得再暴露，避免模型调用失效工具。
    assert!(
        !names.contains(&"task"),
        "removed task alias must not be exposed"
    );
    assert!(
        !names.contains(&"todo_") || !names.iter().any(|name| name.starts_with("todo_")),
        "split todo tools must stay hidden"
    );
    let todo = definitions
        .iter()
        .find(|definition| definition.function.name == "todo")
        .expect("todo definition");
    assert!(
        !todo.function.description.contains("Goal"),
        "the frozen Goal workflow must not be advertised to the model"
    );
    assert_eq!(
        todo.function.parameters["properties"]["action"]["enum"],
        serde_json::json!(["create", "insert", "set", "list"])
    );
    assert_eq!(
        todo.function.parameters["properties"]["status"]["enum"],
        serde_json::json!(["idle", "in_progress", "completed", "cancelled"])
    );
    let branches = todo.function.parameters["oneOf"]
        .as_array()
        .expect("action-specific schema branches");
    assert_eq!(
        branches.len(),
        10,
        "anchored insert variants + three set shapes are public"
    );
    let set_branches: Vec<_> = branches
        .iter()
        .filter(|branch| branch["properties"]["action"]["const"] == "set")
        .collect();
    assert_eq!(
        set_branches.len(),
        3,
        "set must expose single/batch/parallel shapes"
    );
    let set_branch = set_branches
        .iter()
        .find(|branch| {
            branch["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("Set task state (single)"))
        })
        .expect("single set branch");
    assert_eq!(
        set_branch["required"],
        serde_json::json!(["action", "id", "status"])
    );
    // The oneOf branch no longer carries `not` anti-constraints (schema
    // simplification): required + action.const are the sole discriminator.
    // Batch/parallel shapes are discriminated by their own required fields
    // (ids / updates), so no cross-branch `not` is needed.
    assert!(
        set_branches
            .iter()
            .all(|branch| branch.get("not").is_none()),
        "set branches must not carry redundant not anti-constraints"
    );
    let insert_branches: Vec<_> = branches
        .iter()
        .filter(|branch| branch["properties"]["action"]["const"] == "insert")
        .collect();
    assert_eq!(insert_branches.len(), 4);
    assert!(insert_branches.iter().all(|branch| {
        let required = branch["required"].as_array().expect("required array");
        required
            .iter()
            .any(|field| field == "before_id" || field == "after_id")
    }));
}

#[test]
fn manual_status_transitions_round_trip_to_the_frontend_contract() {
    let temp_home = std::env::temp_dir().join(format!(
        "qaqh-todo-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_home).expect("create isolated home");

    // This integration test binary owns an isolated process home.
    // Linux 上 todo 数据目录解析优先 HOME：两个变量都必须钉住，
    // 否则会读进真实用户 home 的历史 todo（并行测试全局态污染）。
    unsafe {
        std::env::set_var("USERPROFILE", &temp_home);
        std::env::set_var("HOME", &temp_home);
    }
    qaqh_workspace::runtime::init_tools("todo-contract", &[], vec![]);
    qaqh_workspace::runtime::set_context("todo-contract", 1);

    for (index, title) in ["Working", "Done", "Cancelled", "Waiting"]
        .into_iter()
        .enumerate()
    {
        let create = qaqh_workspace::execution::execute_with_context(
            "todo",
            "",
            &serde_json::json!({"action":"create", "title": title, "description": format!("item {index}")})
                .to_string(),
            &format!("todo-create-{index}"),
            None,
        );
        assert!(create.success, "create failed: {}", create.content);
    }

    let working = qaqh_workspace::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"set","id":1,"status":"in_progress"}"#,
        "todo-working",
        None,
    );
    assert!(
        working.success,
        "working update failed: {}",
        working.content
    );

    let completed = qaqh_workspace::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"set","id":"T2","status":"completed","evidence":"verified"}"#,
        "todo-completed",
        None,
    );
    assert!(
        completed.success,
        "completed update failed: {}",
        completed.content
    );

    let cancelled = qaqh_workspace::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"set","id":"3","status":"cancelled"}"#,
        "todo-cancelled",
        None,
    );
    assert!(
        cancelled.success,
        "cancel operation failed: {}",
        cancelled.content
    );

    let list = qaqh_workspace::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"list"}"#,
        "todo-list",
        None,
    );
    assert!(list.success, "list failed: {}", list.content);
    let list_json: serde_json::Value =
        serde_json::from_str(&list.content).expect("structured list response");
    assert_eq!(list_json["counts"]["in_progress"], 1);
    assert_eq!(list_json["counts"]["completed"], 1);
    assert_eq!(list_json["counts"]["cancelled"], 1);
    assert_eq!(list_json["counts"]["idle"], 1);
    assert_eq!(list_json["items"][0]["id"], "T1");
    assert_eq!(list_json["items"][1]["status"], "completed");
    assert_eq!(list_json["items"][1]["evidence"], "verified");
    assert_eq!(list_json["items"][3]["status"], "idle");

    let status: serde_json::Value = serde_json::from_str(
        &qaqh_workspace::todo::todo_status_json("todo-contract").expect("status JSON"),
    )
    .expect("parse status JSON");
    assert_eq!(status["mode"], "manual");
    assert_eq!(status["current_id"], "T1");
    assert_eq!(status["current_title"], "Working");
    assert_eq!(status["idle"], 1);
    assert_eq!(status["pending"], 1);
    assert_eq!(status["in_progress"], 1);
    assert_eq!(status["completed"], 1);
    assert_eq!(status["cancelled"], 1);
    assert_eq!(status["total"], 4);
    assert_eq!(status["items"][2]["status"], "cancelled");

    // Verify that the todo.json format is clean (no legacy Goal-enforced normalization).
    let store = qaqh_workspace::todo::load_todo().expect("load todo");
    assert_eq!(store.mode, qaqh_workspace::todo::TodoMode::Manual);

    std::fs::remove_dir_all(&temp_home).expect("remove isolated home");
}
