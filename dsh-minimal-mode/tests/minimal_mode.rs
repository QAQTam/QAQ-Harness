//! Integration tests: register `bash_v2` and `str_replace_editor` through the
//! QAQ-Harness ToolManager and assert the model-facing returns match minimal-mode
//! verbatim.

use qaqh_workspace::runtime;
use serde_json::json;

const SESSION: &str = "dsh-minimal-mode-test";

fn run_tool(name: &str, args: serde_json::Value) -> String {
    let result = qaqh_workspace::execution::execute_with_context(
        name,
        "",
        &args.to_string(),
        "call-1",
        None,
    );
    result.result.model_text().to_string()
}

fn setup() {
    // TOOL_MANAGER is a process-global OnceLock: a single init per test binary.
    runtime::init_tools(SESSION, &[dsh_minimal_mode::register], vec![]);
    runtime::set_context(SESSION, 4);
}

/// Native display path (minimal-mode uses OS separators on Windows).
fn native(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn registry_exposes_bash_v2_and_str_replace_editor() {
    setup();
    let names: Vec<String> = runtime::all_tools()
        .into_iter()
        .map(|def| def.function.name)
        .collect();
    assert!(names.contains(&"bash_v2".to_string()), "got: {names:?}");
    assert!(
        names.contains(&"str_replace_editor".to_string()),
        "got: {names:?}"
    );
}

#[test]
fn str_replace_editor_view_verbatim() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("smoke.txt");
    std::fs::write(&path, "MINIMAL_EDITOR_OK\n").unwrap();
    let abs = native(&path);
    let out = run_tool(
        "str_replace_editor",
        json!({ "command": "view", "path": abs }),
    );
    assert_eq!(
        out,
        format!(
            "Here's the content of {abs} with line numbers (which has a total of 2 lines):\n     1  MINIMAL_EDITOR_OK\n     2  \n"
        ),
        "got: {out:?}"
    );
}

#[test]
fn str_replace_editor_lifecycle_verbatim() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edit.txt");
    let abs = native(&path);

    let created = run_tool(
        "str_replace_editor",
        json!({ "command": "create", "path": abs, "file_text": "alpha\nbeta\n" }),
    );
    assert_eq!(created, format!("New file created successfully at: {abs}"));

    let replaced = run_tool(
        "str_replace_editor",
        json!({ "command": "str_replace", "path": abs, "old_str": "beta", "new_str": "gamma" }),
    );
    assert_eq!(
        replaced,
        format!("The file {abs} has been edited successfully.")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\ngamma\n");

    let inserted = run_tool(
        "str_replace_editor",
        json!({ "command": "insert", "path": abs, "insert_line": 1, "new_str": "mid" }),
    );
    assert_eq!(
        inserted,
        format!("The file {abs} has been edited successfully.")
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "alpha\nmid\ngamma\n"
    );
}

#[test]
fn str_replace_editor_error_verbatim() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.txt");
    let abs = native(&path);
    let out = run_tool(
        "str_replace_editor",
        json!({ "command": "view", "path": abs }),
    );
    assert_eq!(
        out,
        format!("The path {abs} does not exist. Please provide a valid path.")
    );
}

#[test]
fn str_replace_editor_cjk_str_replace_verbatim() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("中文文件.txt");
    std::fs::write(&path, "第一行\n第二行 你好\n第三行\n").unwrap();
    let abs = native(&path);
    let out = run_tool(
        "str_replace_editor",
        json!({ "command": "str_replace", "path": abs, "old_str": "你好", "new_str": "世界" }),
    );
    assert_eq!(out, format!("The file {abs} has been edited successfully."));
    // 中文 old_str 被正确替换，未被字节分片破坏。
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "第一行\n第二行 世界\n第三行\n"
    );
}

#[test]
fn str_replace_editor_cjk_view_line_numbers() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("中文.txt");
    std::fs::write(&path, "你好世界\n第二行\n").unwrap();
    let abs = native(&path);
    let out = run_tool(
        "str_replace_editor",
        json!({ "command": "view", "path": abs }),
    );
    assert_eq!(
        out,
        format!(
            "Here's the content of {abs} with line numbers (which has a total of 3 lines):\n     1  你好世界\n     2  第二行\n     3  \n"
        )
    );
    // 无替换字符（没有被字节切片切坏的多字节）。
    assert!(
        !out.contains('\u{FFFD}'),
        "replacement char leaked: {out:?}"
    );
}

#[test]
fn schemas_match_minimal_mode() {
    setup();
    let defs: Vec<qaqh_types::ToolDef> = runtime::all_tools();
    let def = |name: &str| defs.iter().find(|d| d.function.name == name).unwrap();

    // bash schema (minimal-mode-extraction README §4.2)
    let bash = def("bash_v2");
    assert_eq!(
        bash.function.description,
        "Run commands in a bash shell\n* When invoking this tool, the contents of the \"command\" parameter does NOT need to be XML-escaped.\n* You don't have access to the internet via this tool.\n* You do have access to a mirror of common linux and python packages via apt and pip.\n* State is persistent across command calls and discussions with the user.\n* To inspect a particular line range of a file, e.g. lines 10-25, try 'sed -n 10,25p /path/to/the/file'.\n* Please avoid commands that may produce a very large amount of output.\n* Please run long lived commands in the background, e.g. 'sleep 10 &' or start a server in the background."
    );
    assert_eq!(
        bash.function.parameters,
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to run. Relative path is preferred in the command."
                }
            },
            "required": ["command"]
        })
    );

    // str_replace_editor schema (minimal-mode-extraction README §4.3) — 全量逐字
    let editor = def("str_replace_editor");
    assert_eq!(
        editor.function.description,
        "Custom editing tool for viewing, creating and editing files\n* State is persistent across command calls and discussions with the user\n* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep\n* The `create` command cannot be used if the specified `path` already exists as a file\n* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`\n\nNotes for using the `str_replace` command:\n* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!\n* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique\n* The `new_str` parameter should contain the edited lines that should replace the `old_str`"
    );
    assert_eq!(
        editor.function.parameters,
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The commands to run. Allowed options are: `view`, `create`, `str_replace`, `insert`.",
                    "enum": ["view", "create", "str_replace", "insert"]
                },
                "path": {
                    "type": "string",
                    "description": "Absolute path to file or directory, e.g. `/repo/file.py` or `/repo`."
                },
                "file_text": {
                    "type": "string",
                    "description": "Required parameter of `create` command, with the content of the file to be created."
                },
                "insert_line": {
                    "type": "integer",
                    "description": "Required parameter of `insert` command. The `new_str` will be inserted AFTER the line `insert_line` of `path`."
                },
                "new_str": {
                    "type": "string",
                    "description": "Optional parameter of `str_replace` command containing the new string (if not given, no string will be added). Required parameter of `insert` command containing the string to insert."
                },
                "old_str": {
                    "type": "string",
                    "description": "Required parameter of `str_replace` command containing the string in `path` to replace."
                },
                "view_range": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Optional parameter of `view` command when `path` points to a file. If none is given, the full file is shown. If provided, the file will be shown in the indicated line number range, e.g. [11, 12] will show lines 11 and 12. Indexing at 1 to start. Setting `[start_line, -1]` shows all lines from `start_line` to the end of the file."
                }
            },
            "required": ["command", "path"]
        })
    );
}

#[test]
fn bash_v2_persistent_state_verbatim() {
    let _guard = BASH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    let dir = tempfile::tempdir().unwrap();
    let abs = dir.path().to_string_lossy().into_owned();
    // Call 1: cd + export. Call 2: read back. State persists across calls.
    let _ = run_tool(
        "bash_v2",
        json!({ "command": format!("cd {} && export DSH_MINIMAL_STATE=PERSISTED", shell_quote(&abs)) }),
    );
    let out = run_tool(
        "bash_v2",
        json!({ "command": "printf '%s:%s\\n' \"$DSH_MINIMAL_STATE\" \"$PWD\"" }),
    );
    assert!(
        out.contains("PERSISTED:"),
        "expected PERSISTED state, got: {out:?}"
    );
}

/// bash 集成测试共享进程级 TOOL_MANAGER + 持久 shell，必须串行执行，
/// 否则并行会互相干扰（session / owner / shell 状态竞争）。
static BASH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn bash_v2_nonzero_exit_code_verbatim() {
    let _guard = BASH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    // `sh -c 'exit 3'`：子进程退出 3，bash 会话不退出 → 命令级 `[exit code: N]`。
    let out = run_tool("bash_v2", json!({ "command": "sh -c 'exit 3'" }));
    assert!(out.contains("[exit code: 3]"), "got: {out:?}");
    // marker 绝不能泄漏到模型面。
    assert!(
        !out.contains("__DSH_PERSISTENT_BASH_"),
        "marker leaked: {out:?}"
    );
}

#[test]
fn bash_v2_success_output_verbatim_no_marker() {
    let _guard = BASH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    let out = run_tool("bash_v2", json!({ "command": "printf 'hello dsh\\n'" }));
    assert_eq!(out.trim_end(), "hello dsh", "got: {out:?}");
    // 成功（exit 0）不追加 `[exit code: 0]`，也不泄漏 marker。
    assert!(!out.contains("[exit code:"), "got: {out:?}");
    assert!(
        !out.contains("__DSH_PERSISTENT_BASH_"),
        "marker leaked: {out:?}"
    );
}

#[test]
fn bash_v2_long_output_truncated_verbatim() {
    let _guard = BASH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    // ~20K 字符 > 16K cap → `<response clipped>` + TRUNCATED_MESSAGE。
    let out = run_tool("bash_v2", json!({ "command": "yes x | head -c 20000" }));
    assert!(out.contains("<response clipped>"), "got: {out:?}");
    assert!(
        out.contains("To save on context only part of this file"),
        "got: {out:?}"
    );
}

#[test]
fn bash_v2_cjk_output_verbatim() {
    let _guard = BASH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    // 中文输出跨 UTF-8 多字节，不得被字节分片切坏 / panic。
    let out = run_tool(
        "bash_v2",
        json!({ "command": "printf '你好世界\\n中文测试\\n'" }),
    );
    assert_eq!(out.trim_end(), "你好世界\n中文测试", "got: {out:?}");
    assert!(
        !out.contains('\u{FFFD}'),
        "replacement char leaked: {out:?}"
    );
}

// 说明：PTY 的 shell-exit（`exit 0`）与 reset 路径在 CI/本机测试环境下
// 检测不稳定（portable-pty 的 is_exited 轮询会超时 300s），故不做集成断言；
// 其渲染逻辑（render_shell_exit_status / timeout 消息 / reset 文案）已由
// bash.rs 单元测试锁定。

/// Minimal single-quote shell quoting for the bash smoke test path.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
