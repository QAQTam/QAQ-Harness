//! exec::register — bash/pwsh 工具注册（register_shell_tool + register）。

use crate::{ToolHandler, ToolPlacement, ToolRisk};
use std::time::Duration;

use super::handler::{exec_schema, handle_run_bash, handle_run_pwsh};
use super::shell::Shell;

/// 独立 shell 工具注册（bash / pwsh）：共享 exec 引擎 + 固定 Shell，
/// schema 无 `shell` 参数；description 注入本机解析路径（软检测）。
pub(crate) fn register_shell_tool(
    mgr: &mut crate::ToolManager,
    key: &str,
    shell: Shell,
    handler: fn(crate::ToolCallCtx) -> crate::ToolResult,
) {
    // 触发探测缓存，让 description 里的解析路径真实（Windows git-bash / pwsh 降级链）。
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    let resolved = shell.path();
    let description = if key == "pwsh" {
        format!(
            "Run command via {key} ({resolved}). Modes: argv|[exe,args] or command|[shell string] (+args for -CommandWithArgs). Returns status/exit_code/output; backgrounded+process_id if timeout."
        )
    } else {
        format!(
            "Run command via {key} ({resolved}). Modes: argv|[exe,args] or command|[shell string]. Returns status/exit_code/output; backgrounded+process_id if timeout."
        )
    };
    // ToolHandler.description 是 &'static str：注册仅进程启动一次，leak 即静态。
    let description: &'static str = Box::leak(description.into_boxed_str());
    mgr.register_with_placement(
        ToolHandler {
            key: key.to_string(),
            description,
            input_schema: exec_schema(false),
            handler,
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Exec,
            default_timeout: Duration::from_secs(30),
        },
        ToolPlacement::Workspace,
    );
}

pub fn register(mgr: &mut crate::ToolManager) {
    // exec 已拆分至 bash/pwsh，不再暴露给模型（避免三者鼎立）。
    // bash/pwsh 各自支持 argv 直调（无 shell）与 command(+args) 包装，语义清晰。
    // 内部 handle_run 保留供单测/兼容，但不注册为模型工具。
    register_shell_tool(mgr, "bash", Shell::Bash, handle_run_bash);
    register_shell_tool(mgr, "pwsh", Shell::PowerShell, handle_run_pwsh);
}
