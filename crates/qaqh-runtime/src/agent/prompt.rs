//! System prompt — compiled from embedded markdown.
//!
//! `backend_prompt.md`  defines the agent identity and rules.
//! `os_env.md`           carries runtime environment info (OS, shells, toolchains).

use std::sync::OnceLock;

const DEFAULT_PROMPT: &str = include_str!("prompts/backend_prompt.md");
const OS_ENV_TEMPLATE: &str = include_str!("prompts/os_env.md");

/// Cached OS info string. Set at startup.
pub static OS_INFO: OnceLock<String> = OnceLock::new();

/// Cached toolchain versions. Set at startup.
pub static TOOLS_INFO: OnceLock<String> = OnceLock::new();

/// Cached shell inventory. Discovery must remain side-effect free because this
/// code runs synchronously before a newly spawned agent enters its input loop.
static SHELLS_INFO: OnceLock<String> = OnceLock::new();

/// Full system prompt from embedded backend_prompt.md (identity + rules only).
pub fn full_system_prompt() -> String {
    DEFAULT_PROMPT.to_string()
}

/// Full system prompt with runtime environment injected from os_env.md.
///
/// Placeholders in os_env.md:
///   {{OS}}     → OS_INFO (probed once at daemon startup, see detect_os_info)
///   {{SHELLS}} → auto-detected shells available on this machine
///   {{TOOLS}}  → TOOLS_INFO (toolchain versions detected at startup)
///
/// The date is intentionally NOT part of the system prompt: it would break
/// the provider prefix cache once per day. It is delivered instead via the
/// frozen [Environment] annotation on the first user message (see
/// AgentState::build_context), which regenerates per session without
/// touching the cache prefix.
pub fn full_system_prompt_with_env(os_info: &str) -> String {
    let shells = detect_shells();
    let tools = TOOLS_INFO
        .get()
        .map(|s| s.as_str())
        .unwrap_or("(not detected)");
    let os = if os_info.is_empty() {
        std::env::consts::OS
    } else {
        os_info
    };
    let env_block = OS_ENV_TEMPLATE
        .replace("{{OS}}", os)
        .replace("{{SHELLS}}", shells)
        .replace("{{TOOLS}}", tools);
    format!("{}\n\n{}", DEFAULT_PROMPT, env_block)
}

/// 按工具模式选择系统提示。minimal:dsh 已下线，一切模式（含已废弃名）
/// 都走完整 prompt；模式判定使用 qaqh-types 的单一工具模式契约（BUG-013）。
pub fn system_prompt_for_mode(_tool_mode: &str) -> String {
    full_system_prompt_with_env(OS_INFO.get().map(|s| s.as_str()).unwrap_or(""))
}

/// Detect available shells on this machine.
fn detect_shells() -> &'static str {
    SHELLS_INFO.get_or_init(|| {
        let mut shells: Vec<&str> = Vec::new();
        if cfg!(windows) {
            // Never spawn a shell as a capability probe here. Git Bash startup
            // can block for tens of seconds under concurrent agent creation.
            // 顺序与默认 shell 一致：pwsh 优先。
            if executable_on_path("pwsh") {
                shells.push("pwsh (PowerShell 7)");
            }
            if executable_on_path("bash") {
                shells.push("bash (Git for Windows)");
            }
            shells.push("cmd");
        } else {
            shells.push("bash");
            shells.push("sh");
            if std::path::Path::new("/bin/zsh").exists() {
                shells.push("zsh");
            }
        }
        shells.join(", ")
    })
}

fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    executable_in_dirs(name, std::env::split_paths(&path))
}

fn executable_in_dirs(name: &str, dirs: impl IntoIterator<Item = std::path::PathBuf>) -> bool {
    #[cfg(windows)]
    let candidates = if std::path::Path::new(name).extension().is_some() {
        vec![name.to_string()]
    } else {
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    };
    #[cfg(not(windows))]
    let candidates = [name.to_string()];

    dirs.into_iter().any(|dir| {
        candidates
            .iter()
            .any(|candidate| is_executable_file(&dir.join(candidate)))
    })
}

fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_not_empty() {
        assert!(!full_system_prompt().is_empty());
    }

    #[test]
    fn system_prompt_for_mode_minimal_dsh_falls_back_to_full() {
        // minimal:dsh 已随 bash/pwsh 拆分下线（is_minimal_dsh 恒 false）：
        // 废弃模式必须走与 standard 相同的完整 prompt，极简特例不得复活。
        assert_eq!(
            system_prompt_for_mode("minimal:dsh"),
            system_prompt_for_mode("standard")
        );
        // 完整 prompt 显著长于已退役的极简句（长度守卫双保险）。
        assert!(
            system_prompt_for_mode("standard").len()
                > "You are a helpful software engineer assistant.".len()
        );
        assert!(
            system_prompt_for_mode("").len()
                > "You are a helpful software engineer assistant.".len()
        );
    }

    #[test]
    fn backend_prompt_must_not_hardcode_host_env() {
        // 回归守卫：执行环境只能来自 os_env.md 动态注入（{{OS}}/{{SHELLS}}/
        // {{TOOLS}}）。曾因在 backend_prompt.md 硬编码 "Windows11 26H2；
        // Pwsh7.6" 导致 Linux 会话被告知运行在 Windows 11。
        let base = full_system_prompt();
        assert!(!base.contains("Windows11"));
        assert!(!base.contains("Pwsh7.6"));
        assert!(!base.contains("# 执行环境"));
    }

    #[test]
    fn env_template_renders_all_placeholders() {
        let prompt = full_system_prompt_with_env("probe-os-debian-linux");
        // 环境块必须存在，且 OS 探测值被注入。
        assert!(prompt.contains("执行环境"));
        assert!(prompt.contains("probe-os-debian-linux"));
        // 占位符禁止原样漏出（模板与渲染必须一一对应）。
        assert!(!prompt.contains("{{"));
        // OS_INFO 未初始化时降级为 std::env::consts::OS，而非留空。
        assert!(full_system_prompt_with_env("").contains(std::env::consts::OS));
    }

    #[test]
    fn executable_discovery_reads_directories_without_starting_the_candidate() {
        let root = std::env::temp_dir().join(format!("qaqh-shell-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(windows)]
        let candidate = root.join("probe-shell.exe");
        #[cfg(not(windows))]
        let candidate = root.join("probe-shell");
        #[cfg(windows)]
        std::fs::write(&candidate, b"not an executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&candidate, b"#!/bin/sh\n: > \"$0.ran\"\n").unwrap();
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(executable_in_dirs(
            "probe-shell",
            std::iter::once(root.clone())
        ));
        assert!(!root.join("probe-shell.ran").exists());

        let _ = std::fs::remove_file(candidate);
        let _ = std::fs::remove_dir(root);
    }
}
