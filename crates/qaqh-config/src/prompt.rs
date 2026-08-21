//! System prompt — compiled from embedded markdown.
//!
//! `backend_prompt.md`  defines the agent identity and rules.
//! `os_env.md`           carries runtime environment info (OS, shells, date).

use std::sync::OnceLock;

const DEFAULT_PROMPT: &str = include_str!("../prompts/backend_prompt.md");
const OS_ENV_TEMPLATE: &str = include_str!("../prompts/os_env.md");

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
///   {{OS}}     → OS_INFO (set at startup via agent_bridge)
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

/// 极简模式（minimal:dsh）的系统提示——逐字对齐 deepseek-harness minimal
/// preset（source of truth：minimal-mode-extraction README §2）。只有这一句，
/// 不含 OS 环境 / skills / runtime context；配合逐字精确的工具 schema
/// （bash + str_replace_editor）才能触发模型的“最大化思考”模式。
pub const MINIMAL_DSH_PROMPT: &str = "You are a helpful software engineer assistant.";

/// 按工具模式选择系统提示。`minimal:dsh` 用极简那一句，其余用完整 prompt。
/// 模式判定使用 qaqh-types 的单一工具模式契约（BUG-013）。
pub fn system_prompt_for_mode(tool_mode: &str) -> String {
    if qaqh_types::is_minimal_dsh(tool_mode) {
        MINIMAL_DSH_PROMPT.to_string()
    } else {
        full_system_prompt_with_env(OS_INFO.get().map(|s| s.as_str()).unwrap_or(""))
    }
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
    let candidates = vec![name.to_string()];

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
        return path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
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
    fn system_prompt_for_mode_minimal_dsh_is_verbatim() {
        // 极简模式必须逐字等于 minimal preset 的那一句。
        assert_eq!(
            system_prompt_for_mode("minimal:dsh"),
            "You are a helpful software engineer assistant."
        );
        // 非极简模式仍走完整 prompt（更长）。
        assert!(system_prompt_for_mode("standard").len() > MINIMAL_DSH_PROMPT.len());
        assert!(system_prompt_for_mode("").len() > MINIMAL_DSH_PROMPT.len());
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
