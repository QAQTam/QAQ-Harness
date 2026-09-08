//! exec::handler — 工具 handler 层（normalize_rg/handle_run_with_shell/strip_ansi/exec_schema）。

use crate::{ToolCallCtx, ToolResult};

use super::direct::direct_exec;
use super::shell::{Shell, executable_on_path};

// ── Tool handler ──

/// ripgrep `-rn` 习惯陷阱防御（grep 迁移）。
///
/// `grep -rn`（`-r` 递归 + `-n` 行号）是 POSIX 经典组合；ripgrep 中 `-r` 被
/// 定义为 `--replace`，`rg -rn "pat"` 会被解析为 `-r n`（把匹配替换成字面
/// `n`），输出被污染、搜索不到预期内容。exec 层在调用前把误用的紧贴组合
/// 改写为 rg 的正确写法（递归默认开启，行号用 `-n`）：
///   `rg -rn`  → `rg -n`
///   `rg -rni` → `rg -ni`   （+ 忽略大小写）
///   `rg -rnl` → `rg -nl`   （+ 仅列文件名）
/// 仅改写紧贴组合；`-r` 单独出现（`--replace` 的合法用法，如 `rg -r x pat`）
/// 与长选项 `--replace` 不受影响。grep 本身（`grep -rn` 合法）不处理。
pub(crate) fn normalize_rg_argv(argv: &mut [String]) {
    if !matches!(
        argv.first().map(|p| p.to_lowercase()).as_deref(),
        Some("rg") | Some("rg.exe")
    ) {
        return;
    }
    for arg in argv.iter_mut().skip(1) {
        if let Some(rest) = arg.strip_prefix("-rn") {
            let cleaned = format!("-n{rest}");
            log::info!("[exec] rg habit fix (argv): '{arg}' -> '{cleaned}'");
            *arg = cleaned;
        }
    }
}

/// command 模式版本：在 shell 命令字符串里改写 `rg -rn...` → `rg -n...`。
/// 匹配 `rg` / `rg.exe` 后紧跟的 `-rn` 前缀组合（大小写不敏感，适配
/// Windows `RG.EXE`）；管道/多命令场景同样覆盖。引号内出现的字面文本
/// 也会被改写——罕见且语义无害，接受。
pub(crate) fn normalize_command_rg(command: &str) -> String {
    use std::sync::OnceLock;
    static RG_HABIT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RG_HABIT_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(rg(\.exe)?)\s+-rn([a-z]*)").expect("rg habit regex")
    });
    re.replace_all(command, |caps: &regex::Captures| {
        // caps[1] 为完整程序名（含 .exe，原始大小写），caps[3] 为组合尾缀
        let prog = &caps[1];
        let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let cleaned = format!("{prog} -n{rest}");
        log::info!("[exec] rg habit fix (command): 'rg -rn{rest}' -> '{cleaned}'");
        cleaned
    })
    .into_owned()
}

/// exec 通用入口：`shell` 参数显式选壳（默认平台自动检测），`command` 经
/// 选定 shell 包装执行，`argv` 直调（无 shell）。pwsh 特判收敛在 Shell 枚举
/// 内（-EncodedCommand/-CommandWithArgs 降级链），此层不分支。
pub(crate) fn handle_run_exec(ctx: ToolCallCtx) -> ToolResult {
    handle_run_with_shell(ctx, None)
}
/// shell 可用性软检测：注册不拒绝，调用时解析路径不可用才报错。
/// shell 可用性软检测：注册不拒绝，调用时解析路径不可用才报错。
pub(crate) fn shell_available(shell: Shell) -> bool {
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    let path = shell.path();
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.is_file()
    } else {
        executable_on_path(path)
    }
}

/// 本机可用 shell 清单（软检测报错的引导信息）。
pub(crate) fn available_shells() -> String {
    let mut list = Vec::new();
    for (name, shell) in [
        ("bash", Shell::Bash),
        ("pwsh", Shell::PowerShell),
        ("cmd", Shell::Cmd),
    ] {
        if shell_available(shell) {
            list.push(name);
        }
    }
    if list.is_empty() {
        "none detected".to_string()
    } else {
        list.join(", ")
    }
}

/// 共享执行引擎。`fixed` = 强制指定壳（仅单测）；
/// None = exec 通用入口（`shell` 参数显式选壳，缺省平台自动检测）。
/// `argv` 直调无 shell：`shell`/`args` 与 `argv` 同传时显式拒绝（静默忽略
/// 比报错更贵——模型会误以为参数生效）。
pub(crate) fn handle_run_with_shell(ctx: ToolCallCtx, fixed: Option<Shell>) -> ToolResult {
    // ── Resolve argv ──
    // Two modes: `command` (auto-wrapped in platform shell) or `argv` (direct exec).
    let shell_command: Option<String> = ctx.get_str("command").map(String::from);
    let argv: Vec<String> = if let Some(command) = shell_command.as_deref() {
        if command.is_empty() {
            return crate::json_err(
                "EMPTY_COMMAND",
                "command string is empty",
                "Provide a shell command string.",
            );
        }
        // ── rg 习惯陷阱防御（grep 迁移）：`rg -rn` → `rg -n` ──
        // `grep -rn`（-r 递归 + -n 行号）是 POSIX 经典组合；ripgrep 中
        // `-r` 是 --replace，`rg -rn "pat"` 会被解析成 `-r n`（把匹配替换成
        // 字面 `n`），输出被污染。在此把 command 字符串中的紧贴组合改写为
        // rg 正确写法（递归默认开启，行号用 -n）。
        let command = normalize_command_rg(command);
        // 固定 shell（bash/pwsh 独立工具）或 exec 的 `shell` 参数/平台默认。
        let shell = match fixed {
            Some(shell) => {
                if !shell_available(shell) {
                    return crate::json_err(
                        "SHELL_NOT_FOUND",
                        format!("{} not found on this machine", shell.path()),
                        format!("available shells: {}", available_shells()),
                    );
                }
                shell
            }
            None => match ctx.args.get("shell").and_then(|v| v.as_str()) {
                Some(name) if !name.is_empty() => match Shell::from_name(name) {
                    Some(shell) => shell,
                    None => {
                        return crate::json_err(
                            "UNKNOWN_SHELL",
                            format!("unknown shell '{name}'"),
                            "Use one of: bash, zsh, sh, pwsh, powershell, cmd. The default is auto-detected (pwsh on Windows, bash elsewhere).",
                        );
                    }
                },
                _ => Shell::detect(),
            },
        };
        // `args: string[]` 透传（模板与数据分离，避免在脚本字符串内拼接引号）：
        // - PowerShell 7.6 LTS：-CommandWithArgs 把额外参数原样填入 $args；
        // - POSIX（bash/zsh/sh）：`sh -c 'script' _ arg...`，参数进 $1/$2/$@（$0 固定占位 `_`）。
        let extra_args: Option<Vec<String>> = ctx
            .args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        if extra_args.is_some() && shell == Shell::Cmd {
            return crate::json_err(
                "ARGS_NOT_SUPPORTED",
                "args is only supported for bash/zsh/sh ($1/$@) and pwsh -CommandWithArgs ($args)",
                "Use exec with shell bash/zsh/sh and args as string array (positional $1...), or shell pwsh.",
            );
        }
        shell.derive_exec_args_with(&command, extra_args.as_deref())
    } else {
        // argv 直调无 shell：shell/args 与 argv 同传是模型误解（以为参数
        // 会生效），显式拒绝比静默忽略便宜。
        if let Some(name) = ctx.args.get("shell").and_then(|v| v.as_str())
            && !name.is_empty()
        {
            return crate::json_err(
                "ARGV_IGNORES_SHELL",
                format!("argv mode runs direct exec without a shell; 'shell: {name}' is ignored"),
                "Use command (not argv) to run through a shell, or drop the shell parameter.",
            );
        }
        if ctx
            .args
            .get("args")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
        {
            return crate::json_err(
                "ARGV_IGNORES_ARGS",
                "argv mode runs direct exec without a shell; 'args' only applies to command mode",
                "Use command + args (positional $1... / $args), or drop args.",
            );
        }
        match ctx.args.get("argv").and_then(|v| v.as_array()) {
            Some(arr) => {
                let mut argv: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                // ── rg 习惯陷阱防御（argv 模式）：`rg -rn...` → `rg -n...` ──
                normalize_rg_argv(&mut argv);
                argv
            }
            None => {
                return crate::json_err(
                    "MISSING_ARGV",
                    "exec requires argv or command",
                    "Example: {\"argv\": [\"cargo\", \"check\"]} or {\"command\": \"cargo check\"}",
                );
            }
        }
    };
    if argv.is_empty() {
        return crate::json_err(
            "EMPTY_ARGV",
            "argv array is empty",
            "Provide at least one element.",
        );
    }
    // 默认 token 上限跟随折叠策略：StandardPolicy=10K；NoFoldPolicy（极限模式）
    // = 不截断（u32::MAX，模型显式传 max_output_tokens 时以模型参数为准）。
    let policy_default = crate::tool_side_fold::policy()
        .exec_max_output_tokens()
        .unwrap_or(u32::MAX);
    let max_output_tokens = ctx
        .get_u64("max_output_tokens")
        .filter(|&n| (100..=50000).contains(&n))
        .map(|n| n as u32)
        .unwrap_or(policy_default);
    let timeout_secs = ctx
        .get_u64("timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or_else(|| ctx.timeout_secs.unwrap_or(30).clamp(1, 3600));
    // 快速后台移交窗口：进程存活超过该时长（秒）即返回 backgrounded，
    // 不等 timeout_secs。用于拉起长驻服务（serve/daemon/watch）。
    let background_after_secs = ctx
        .get_u64("background_after_secs")
        .filter(|&n| n > 0 && n <= 3600);
    // Fall back to workspace root when the caller doesn't supply cwd.
    // A relative cwd resolves against the workspace root (or the process
    // directory when no workspace is set) — same semantics as file tools.
    let cwd: Option<String> = ctx
        .get_str("cwd")
        .map(String::from)
        .map(|cwd| {
            let resolved = crate::resolve_workspace_path(&cwd);
            if resolved.is_empty() { cwd } else { resolved }
        })
        .or_else(|| {
            let ws = crate::current_workspace();
            if ws.is_empty() || ws == "." {
                None
            } else {
                Some(ws)
            }
        });
    // 可选环境变量覆盖（传入完整 env 供子进程使用）。
    let env: Option<Vec<(String, String)>> = ctx
        .args
        .get("env")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .filter(|pairs: &Vec<(String, String)>| !pairs.is_empty());
    let cwd_ref: Option<&str> = cwd.as_deref();
    let mut result = direct_exec(
        &argv,
        env.as_deref(),
        cwd_ref,
        max_output_tokens,
        timeout_secs,
        background_after_secs,
        Some(ctx.cancel.as_ref()),
        ctx.tx_progress.clone(),
        &ctx.id,
    );
    // 观测线纪律（事故 2026-09-02 预防）：检测 shell 命令中的后台派生 `&`，
    // 以强提示引导走 background_after_secs + process 工具的受控路径。
    if let Some(command) = shell_command.as_deref()
        && result.status == "completed"
        && detect_background_derivation(command)
    {
        result.output.push_str(BACKGROUND_DERIVATION_HINT);
    }
    let success = match result.exit_code {
        Some(0) => true,
        Some(_) => false,
        None => !result.timed_out && !result.cancelled,
    };
    let json = result.to_json();
    if success {
        // 极限模式（NoFoldPolicy）：exec/bash/pwsh 输出完全透传，
        // 连 qaqh-types 的 24K 字符硬顶也放开（仍保留 read_stream 字节保护）。
        if crate::tool_side_fold::policy()
            .exec_max_output_tokens()
            .is_none()
        {
            ToolResult::ok_with_limit(json, None)
        } else {
            ToolResult::ok(json)
        }
    } else {
        ToolResult::error(json)
    }
}

/// 后台派生强提示（观测线纪律，fd 持有复现实验 2026-09-06）：裸 `cmd &` 的
/// 孙进程持 fd1/fd2 = 管道写端，孤儿化 reparent 后长期滞留——阶段 2 的
/// settle 机制保证读线程有界退出，但输出完整性仍以重定向到文件 + 受控移交
/// （background_after_secs + process 工具）为正道。
pub(crate) const BACKGROUND_DERIVATION_HINT: &str = "\n[!] 后台派生检测：命令包含 `&`（后台任务）。后台/孙进程可能持有输出管道，导致本结果遗漏其后继输出。长驻服务请改用 background_after_secs 参数移交后台，并以 process 工具（check/wait/kill）接管；后台命令应将 stdout/stderr 重定向到文件（证据：docs/incidents/2026-09-06-fd-hold-repro.md）。\n";

/// 检测 shell 命令中的后台派生操作符。
/// 剥除逻辑与（`&&`）与重定向组合（`>&`/`&>`，覆盖 `2>&1`）后残留的
/// `&` 才是后台派生；引号内的 `&`（sed/awk 等）会误报——提示为建议性
/// 输出，宁滥勿缺。
pub(crate) fn detect_background_derivation(command: &str) -> bool {
    command
        .replace("&&", "")
        .replace(">&", "")
        .replace("&>", "")
        .contains('&')
}

// ── Registration ──

/// exec / bash / pwsh 共享的 input schema 模板。
/// `with_shell` 控制是否暴露 `shell` 参数（仅 exec 通用入口）。
pub(crate) fn exec_schema(with_shell: bool) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "argv".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "Direct exec without a shell (e.g. [\"cargo\", \"check\"]); must not combine with shell/args" }),
    );
    props.insert(
        "command".into(),
        serde_json::json!({ "type": "string", "description": "Shell command string (runs via `shell`, default auto-detected)" }),
    );
    props.insert(
        "args".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "Extra args for command: bash/zsh/sh fills $1/$2/$@ ($0 is placeholder `_`); pwsh fills $args (-CommandWithArgs). cmd does not support args." }),
    );
    if with_shell {
        props.insert(
            "shell".into(),
            serde_json::json!({ "type": "string", "enum": ["bash", "zsh", "sh", "pwsh", "powershell", "cmd"], "description": "Shell for command (default auto-detected: pwsh on Windows, bash elsewhere)" }),
        );
    }
    props.insert(
        "cwd".into(),
        serde_json::json!({"type": "string", "description": "Workdir (default workspace root)"}),
    );
    props.insert(
        "env".into(),
        serde_json::json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Env overrides"}),
    );
    props.insert(
        "timeout_secs".into(),
        serde_json::json!({"type": "integer", "description": "Timeout secs (1-3600, default 30)"}),
    );
    props.insert(
        "background_after_secs".into(),
        serde_json::json!({"type": "integer", "description": "Background after secs -> backgrounded+process_id"}),
    );
    props.insert(
        "max_output_tokens".into(),
        serde_json::json!({ "type": "integer", "description": "Max output tokens (10000, 100-50000)" }),
    );
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [],
        "additionalProperties": false,
        "oneOf": [
            {"required": ["argv"]},
            {"required": ["command"]}
        ]
    })
}
