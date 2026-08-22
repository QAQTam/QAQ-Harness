//! Workspace directory resolution.
//!
//! Resolves `.deepx/` — the project-local hidden directory for PLAN.md,
//! trash, tasks, and project-scoped memory. Falls back to a subdirectory
//! of `data_dir()` when no workspace is active.

use std::path::{Path, PathBuf};

/// Return the `.deepx/` directory for the current workspace.
///
/// Priority:
/// 1. `{workspace}/.deepx/` if workspace is set and not "."
/// 2. `{data_dir}/workspace/` as fallback (headless / no workspace mode)
///
/// The fallback is intentionally NOT `home_dir()/.deepx/` to avoid
/// conflating workspace artifacts with global config/sessions data.
pub fn qaqh_dir() -> PathBuf {
    let ws = crate::current_workspace();
    if !ws.is_empty() && ws != "." {
        Path::new(&ws).join(".deepx")
    } else {
        qaqh_types::platform::data_dir().join("workspace")
    }
}

/// Bind the global session identifier used by tools and code-delta tracking.
pub fn set_current_session(seed: &str) {
    crate::set_current_session(seed);
}

/// Load and activate the workspace persisted for a session.
///
/// 统一数据源：`SessionMeta.cwd`（qaqh-session）——旧的
/// `sessions/{seed}/workspace.txt` 由读取侧惰性迁移（零停机）。
pub fn load_session_workspace(seed: &str) {
    let workspace = qaqh_session::workspace::session_workspace_cwd(seed).unwrap_or_default();
    set_process_workspace(if workspace.is_empty() {
        "."
    } else {
        &workspace
    });
}

/// Update tool path resolution and the process working directory together.
pub fn set_process_workspace(path: &str) {
    // WSL serve 侧把 Windows 路径转 /mnt 后再落盘 + cd（Linux 下才能真实 cd 成功）。
    let path = crate::wsl_path::platform_workspace_path(path);
    crate::set_workspace(&path);
    if let Err(error) = std::env::set_current_dir(&path) {
        log::warn!("set_process_workspace: cannot cd to '{}': {error}", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qaqh_dir_returns_workspace_subdir_when_set() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("/home/user/project");
        let dir = qaqh_dir();
        assert_eq!(dir, Path::new("/home/user/project/.deepx"));
    }

    #[test]
    fn qaqh_dir_falls_back_when_empty() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("");
        let dir = qaqh_dir();
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }

    #[test]
    fn qaqh_dir_falls_back_when_dot() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace(".");
        let dir = qaqh_dir();
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }
}
