//! exec::register — exec 通用入口注册（方案 A 独占）。
//!
//! 0946afe 曾将 exec 拆分为 bash/pwsh 双工具（避免三者鼎立）；回 exec 后
//! 语义收敛为单一 `exec` + `shell` 参数（bash/zsh/sh/pwsh/powershell/cmd，
//! 缺省平台自动检测）。pwsh 特判收敛在 [`Shell`] 枚举内（路径降级链 +
//! -EncodedCommand/-CommandWithArgs），注册层不分支。

use crate::{ToolHandler, ToolPlacement, ToolRisk};
use std::time::Duration;

use super::handler::{exec_schema, handle_run_exec};
use super::shell::Shell;

pub fn register(mgr: &mut crate::ToolManager) {
    // 触发探测缓存（Windows git-bash / pwsh 降级链），description 不再为
    // 特定 shell 背书——选壳是每次调用的运行时决策。
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    let description: &'static str = Box::leak(
        "Run command: argv|[exe,args] direct exec without a shell, or command|[shell string] via shell (default auto-detected: pwsh on Windows, bash elsewhere; explicit shell: bash/zsh/sh/pwsh/cmd). POSIX args fill $1/$2/$@, pwsh args fill $args. Returns status/exit_code/output; backgrounded+process_id if timeout."
            .to_string()
            .into_boxed_str(),
    );
    mgr.register_with_placement(
        ToolHandler {
            key: "exec".to_string(),
            description,
            input_schema: exec_schema(true),
            handler: handle_run_exec,
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Exec,
            default_timeout: Duration::from_secs(30),
        },
        ToolPlacement::Workspace,
    );
}
