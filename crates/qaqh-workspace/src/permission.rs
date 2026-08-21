//! Permission engine: tool categories, permission levels, and trusted folder management.
//!
//! ## Architecture
//! - `ToolCategory` classifies every tool by risk profile (Read/Write/Exec/Net).
//! - `PermissionLevel` defines the default policy (1–4).
//! - `needs_permission()` evaluates whether a tool call requires user confirmation.
//! - `TrustedFolderSet` persists cross-workspace folder trust decisions.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

// ──────────────────────────────────────
// Tool category taxonomy
// ──────────────────────────────────────

/// Risk profile for each tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    /// No side effects: read, search, skills, image, ask, process(check/wait),
    /// and read-only git queries.
    Read,
    /// Mutates files or session state: edit, task, and write-oriented git
    /// operations.
    Write,
    /// Executes arbitrary code or controls a running process: exec, process(kill/write).
    Exec,
    /// Outbound network: web_fetch.
    Net,
}

/// Intrinsic impact of the requested action, independent of the configured
/// permission policy level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

impl PermissionRisk {
    pub fn consequence(self) -> &'static str {
        match self {
            Self::Low => "Reads data without changing it.",
            Self::Medium => "Changes files inside the current workspace.",
            Self::High => "May affect external resources or execute arbitrary actions.",
        }
    }
}

/// Classify action impact from authoritative category and normalized resources.
pub fn classify_risk(
    category: ToolCategory,
    paths: &[PathBuf],
    workspace: &Path,
) -> PermissionRisk {
    if matches!(category, ToolCategory::Exec | ToolCategory::Net) {
        return PermissionRisk::High;
    }

    let workspace = resolve_target_path(workspace.to_path_buf());
    if paths
        .iter()
        .map(|path| resolve_target_path(path.clone()))
        .any(|path| !path.starts_with(&workspace))
    {
        return PermissionRisk::High;
    }

    match category {
        ToolCategory::Read => PermissionRisk::Low,
        ToolCategory::Write => PermissionRisk::Medium,
        ToolCategory::Exec | ToolCategory::Net => PermissionRisk::High,
    }
}

// ──────────────────────────────────────
// Permission level
// ──────────────────────────────────────

/// Agent operating permission level (1–4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PermissionLevel {
    /// Level 1: Every tool call requires user confirmation.
    MaxLockdown = 1,
    /// Level 2: Workspace reads auto-approve; writes, exec, net require confirmation.
    ReadFree = 2,
    /// Level 3: Workspace all auto-approve; cross-workspace writes require one-time folder trust.
    WorkspaceFree = 3,
    /// Level 4: No permission checks (current default behavior).
    Unrestricted = 4,
}

impl PermissionLevel {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::MaxLockdown,
            2 => Self::ReadFree,
            3 => Self::WorkspaceFree,
            _ => Self::Unrestricted,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MaxLockdown => "Level 1 — Maximum Lockdown",
            Self::ReadFree => "Level 2 — Read Free",
            Self::WorkspaceFree => "Level 3 — Workspace Free",
            Self::Unrestricted => "Level 4 — Unrestricted",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::MaxLockdown => "All operations require confirmation. No automatic trust.",
            Self::ReadFree => {
                "Reads auto-approve. Writes, execution, and network require confirmation."
            }
            Self::WorkspaceFree => {
                "Auto-approve within workspace. Cross-workspace writes are trusted once per folder."
            }
            Self::Unrestricted => "No permission checks. All tools execute immediately.",
        }
    }
}

// ──────────────────────────────────────
// Path helpers
// ──────────────────────────────────────

/// Extract file/directory paths from tool arguments that the tool will read or write.
pub fn extract_target_paths(tool_name: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if tool_name == "read"
        && let Some(requests) = args.get("requests").and_then(|value| value.as_array()) {
            paths.extend(requests.iter().filter_map(|request| {
                request
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(PathBuf::from)
            }));
        }
    // Direct path argument
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(p));
    }
    // Multiple paths
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                paths.push(PathBuf::from(s));
            }
        }
    }
    // source / dest pairs (copy, move)
    if let Some(s) = args.get("source").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(s));
    }
    if let Some(d) = args.get("dest").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(d));
    }
    // copy_range: source (read) + target (write) — both workspace-bounded
    if tool_name == "copy_range" {
        if let Some(s) = args.get("source_path").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(s));
        }
        if let Some(t) = args.get("target_path").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(t));
        }
    }
    // journal: replay target/out may write outside the workspace; keep it
    // authorization-bounded like other write tools.
    if tool_name == "journal" {
        if let Some(f) = args.get("file").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(f));
        }
        if let Some(o) = args.get("out").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(o));
        }
    }
    // exec: extract cwd
    if tool_name == "exec"
        && let Some(cwd) = args.get("cwd").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(cwd));
        }

    paths.into_iter().map(resolve_target_path).collect()
}

/// Resolve symlinks/junctions in the nearest existing ancestor, then append
/// any missing suffix. This keeps authorization checks correct for new files.
pub(crate) fn resolve_target_path(path: PathBuf) -> PathBuf {
    // WSL serve 侧：worker 下发的 Windows 绝对路径转 /mnt，使授权资源路径与
    // workspace_root（同样已被归一化为 /mnt）一致，避免被误判为跨 workspace。
    let path = PathBuf::from(crate::wsl_path::platform_workspace_path(
        path.to_string_lossy().as_ref(),
    ));
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    let normalized = normalize_lexically(&absolute);
    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();

    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return normalized;
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return normalized;
        };
        ancestor = parent;
    }

    let Ok(mut resolved) = std::fs::canonicalize(ancestor) else {
        return normalized;
    };
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    resolved
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

/// Check if ALL target paths are inside the workspace root.
pub(crate) fn all_within_workspace(paths: &[PathBuf], workspace: &Path) -> bool {
    if paths.is_empty() {
        return true;
    } // tools without paths (e.g. ask) are considered safe
    paths.iter().all(|p| p.starts_with(workspace))
}

/// Find the first path (if any) that is outside the workspace.
fn first_outside_workspace<'a>(paths: &'a [PathBuf], workspace: &Path) -> Option<&'a PathBuf> {
    paths.iter().find(|p| !p.starts_with(workspace))
}

// ──────────────────────────────────────
// Permission decision
// ──────────────────────────────────────

/// Result of `needs_permission()`: either auto-approve or request confirmation.
#[derive(Debug)]
pub enum PermissionDecision {
    /// No confirmation needed — execute immediately.
    AutoApprove,
    /// Confirmation required. Contains the reason and target paths for the dialog.
    AskUser {
        /// Human-readable reason for the dialog (e.g. "Write to external path").
        reason: String,
        /// Paths to display in the dialog.
        paths: Vec<PathBuf>,
        /// Whether the tool is Read/Write/Exec/Net.
        category: ToolCategory,
        /// Intrinsic impact of this action, independent of policy level.
        risk: PermissionRisk,
        /// User-facing description of the effect of approving the action.
        consequence: String,
    },
}

/// Determine whether a tool call requires user permission.
///
/// - `level`: current permission level
/// - `tool_name`: registered tool name
/// - `args`: tool arguments (JSON)
/// - `workspace_root`: workspace root directory (used for boundary checks)
/// - `trusted_dirs`: set of previously trusted directories
/// - `declared_category`: capability category from the handler declaration
///   （单一事实源；`process` 等按 action 细分的工具在内部覆盖）
pub fn needs_permission(
    level: PermissionLevel,
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    trusted_dirs: &HashSet<PathBuf>,
    declared_category: ToolCategory,
) -> PermissionDecision {
    // Task only mutates the active session's own todo.json. It does not
    // touch workspace files, run code, or access external resources. Requiring
    // approval for each model-authored status transition creates recursive,
    // repeated prompts without protecting a user-controlled resource.
    if matches!(tool_name, "todo") {
        return PermissionDecision::AutoApprove;
    }

    // Level 4: everything auto-approved
    if level == PermissionLevel::Unrestricted {
        return PermissionDecision::AutoApprove;
    }

    // ask is itself the user-interaction boundary. Opening a permission
    // dialog for it creates a recursive prompt and prevents the Ring from
    // delivering the actual model question.
    if tool_name == "ask" {
        return PermissionDecision::AutoApprove;
    }

    // `process` / `edit` 按调用形态细分（per-action 授权颗粒度的扩展点）：
    // - process: check/wait 只读、kill/write 控制进程；
    // - edit: hunks 缺失/空 = 读路径（Read，不触发写确认；整文件 /
    //   行号范围 / 锚定读三种形态均只读），非空 = 编辑/创建（Write）。未来对
    //   read/edit/create/overwrite 各自粒度授权时，在本 match 内扩展。
    let category = match tool_name {
        "process" => match args.get("action").and_then(|value| value.as_str()) {
            Some("check" | "wait") => ToolCategory::Read,
            Some("write" | "kill") => ToolCategory::Exec,
            _ => ToolCategory::Write,
        },
        "edit" => {
            let has_hunks = args
                .get("hunks")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty());
            if has_hunks {
                ToolCategory::Write
            } else {
                ToolCategory::Read
            }
        }
        _ => declared_category,
    };
    let paths = extract_target_paths(tool_name, args);
    let workspace_root = resolve_target_path(workspace_root.to_path_buf());
    let risk = classify_risk(category, &paths, &workspace_root);
    let consequence = risk.consequence().to_string();

    // Level 1: everything requires confirmation
    if level == PermissionLevel::MaxLockdown {
        return PermissionDecision::AskUser {
            reason: format!("Level 1: '{}' requires confirmation.", tool_name),
            paths,
            category,
            risk,
            consequence,
        };
    }

    // Level 2+: Reads auto-approve
    if category == ToolCategory::Read {
        return PermissionDecision::AutoApprove;
    }

    // Level 3: only workspace writes auto-approve. Exec and Net still require confirmation.
    if level >= PermissionLevel::WorkspaceFree && category == ToolCategory::Write {
        // If no paths or all paths are within the workspace, auto-approve the write.
        if all_within_workspace(&paths, &workspace_root) {
            return PermissionDecision::AutoApprove;
        }

        // Cross-workspace: check trusted folders
        if let Some(outside) = first_outside_workspace(&paths, &workspace_root) {
            let dir: &Path = outside.parent().unwrap_or(outside);
            if trusted_dirs
                .iter()
                .any(|trusted| resolve_target_path(trusted.clone()) == dir)
            {
                return PermissionDecision::AutoApprove;
            }
        }
    }

    // Otherwise: ask user
    let reason = if level == PermissionLevel::ReadFree {
        format!(
            "Level 2: '{}' (write/exec/net) requires confirmation.",
            tool_name
        )
    } else if matches!(category, ToolCategory::Exec | ToolCategory::Net) {
        format!(
            "Level 3: '{}' requires execution or network confirmation.",
            tool_name
        )
    } else {
        format!(
            "Level 3: '{}' accesses a path outside the workspace.",
            tool_name
        )
    };

    PermissionDecision::AskUser {
        reason,
        paths,
        category,
        risk,
        consequence,
    }
}

// ──────────────────────────────────────
// Trusted folder set
// ──────────────────────────────────────

/// Persistent set of trusted directories for cross-workspace access.
/// Stored as `{sessions_dir}/{seed}/trusted_folders.json`.
pub struct TrustedFolderSet {
    seed: String,
    dirs: HashSet<PathBuf>,
}

impl TrustedFolderSet {
    /// Load the trusted folders file for a session, or create an empty set.
    pub fn load(seed: &str) -> Self {
        let path = trusted_folders_path(seed);
        let dirs = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                .map(|v| v.into_iter().map(PathBuf::from).collect())
                .unwrap_or_default()
        } else {
            HashSet::new()
        };
        Self {
            seed: seed.to_string(),
            dirs,
        }
    }

    /// Add a directory to the trusted set and persist.
    pub fn trust(&mut self, dir: &Path) {
        self.dirs.insert(dir.to_path_buf());
        self.save();
    }

    /// Check if a directory is trusted.
    pub fn contains(&self, dir: &Path) -> bool {
        self.dirs.contains(dir)
    }

    /// Expose the underlying set for permission checks.
    pub fn set(&self) -> &HashSet<PathBuf> {
        &self.dirs
    }

    fn save(&self) {
        let path = trusted_folders_path(&self.seed);
        let dir = path.parent().unwrap();
        let _ = std::fs::create_dir_all(dir);
        let list: Vec<String> = self
            .dirs
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let _ = std::fs::write(&path, serde_json::to_string(&list).unwrap_or_default());
    }
}

fn trusted_folders_path(seed: &str) -> PathBuf {
    crate::workspace::qaqh_dir()
        .join("sessions")
        .join(seed)
        .join("trusted_folders.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LinkedTempTree {
        root: PathBuf,
        link: PathBuf,
    }

    impl Drop for LinkedTempTree {
        fn drop(&mut self) {
            #[cfg(windows)]
            let _ = std::fs::remove_dir(&self.link);
            #[cfg(unix)]
            let _ = std::fs::remove_file(&self.link);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn linked_temp_tree() -> (LinkedTempTree, PathBuf, PathBuf) {
        let unique = format!(
            "qaqh-permission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        let link = workspace.join("external-link");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&outside).expect("create outside directory");

        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .status()
                .expect("create directory junction");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("create directory symlink");

        (
            LinkedTempTree {
                root,
                link: link.clone(),
            },
            workspace,
            link.join("new.txt"),
        )
    }

    #[test]
    fn ask_user_does_not_open_a_second_permission_dialog() {
        let decision = needs_permission(
            PermissionLevel::MaxLockdown,
            "ask",
            &serde_json::json!({"question":"Continue?"}),
            Path::new("."),
            &HashSet::new(),
            ToolCategory::Read,
        );

        assert!(matches!(decision, PermissionDecision::AutoApprove));
    }

    #[test]
    fn session_todo_operations_never_open_permission_dialogs() {
        for level in [
            PermissionLevel::MaxLockdown,
            PermissionLevel::ReadFree,
            PermissionLevel::WorkspaceFree,
            PermissionLevel::Unrestricted,
        ] {
            for tool_name in ["todo"] {
                let decision = needs_permission(
                    level,
                    tool_name,
                    &serde_json::json!({"id": "T1", "status": "completed"}),
                    Path::new("."),
                    &HashSet::new(),
                    ToolCategory::Write,
                );
                assert!(
                    matches!(decision, PermissionDecision::AutoApprove),
                    "{tool_name} should be auto-approved at level {}",
                    level.to_u8()
                );
            }
        }
    }

    #[test]
    fn edit_empty_hunks_classified_as_read() {
        // 空 hunks = 读路径 → 动态分类 Read（handler 声明仍为 Write，由
        // admit() 按调用形态覆盖）→ ReadFree 下自动放行。
        let read = needs_permission(
            PermissionLevel::ReadFree,
            "edit",
            &serde_json::json!({"path": "a.txt", "hunks": []}),
            Path::new("."),
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(read, PermissionDecision::AutoApprove),
            "read path should auto-approve at ReadFree, got: {read:?}"
        );

        // 非空 hunks = 编辑 → Write → ReadFree 下需确认。
        let edit = needs_permission(
            PermissionLevel::ReadFree,
            "edit",
            &serde_json::json!({"path": "a.txt", "hunks": [{"kind": "replace", "old": "a", "new": "b"}]}),
            Path::new("."),
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(edit, PermissionDecision::AskUser { .. }),
            "edit path should ask at ReadFree, got: {edit:?}"
        );
    }

    #[test]
    fn permission_risk_distinguishes_read_workspace_write_and_exec() {
        let workspace = resolve_target_path(PathBuf::from("C:/repo"));

        assert_eq!(
            classify_risk(ToolCategory::Read, &[], &workspace),
            PermissionRisk::Low
        );
        assert_eq!(
            classify_risk(
                ToolCategory::Write,
                &[workspace.join("src/lib.rs")],
                &workspace,
            ),
            PermissionRisk::Medium
        );
        assert_eq!(
            classify_risk(ToolCategory::Exec, &[], &workspace),
            PermissionRisk::High
        );
        assert_eq!(
            classify_risk(
                ToolCategory::Write,
                &[resolve_target_path(PathBuf::from("C:/outside/file"))],
                &workspace,
            ),
            PermissionRisk::High
        );
    }

    #[test]
    fn workspace_free_requires_approval_for_missing_file_beneath_external_link() {
        let (_temp, workspace, target) = linked_temp_tree();

        let decision = needs_permission(
            PermissionLevel::WorkspaceFree,
            "write",
            &serde_json::json!({"path": target}),
            &workspace,
            &HashSet::new(),
            ToolCategory::Write,
        );

        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "a missing file beneath an external directory link must not auto-approve"
        );
    }

    #[test]
    fn workspace_free_requires_approval_for_missing_file_after_parent_traversal() {
        let (_temp, workspace, _) = linked_temp_tree();
        let target = workspace
            .join("missing")
            .join("..")
            .join("..")
            .join("outside")
            .join("new.txt");

        let decision = needs_permission(
            PermissionLevel::WorkspaceFree,
            "write",
            &serde_json::json!({"path": target}),
            &workspace,
            &HashSet::new(),
            ToolCategory::Write,
        );

        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "parent traversal to a missing file outside the workspace must not auto-approve"
        );
    }

    #[test]
    fn workspace_free_still_requires_approval_for_exec_and_network() {
        for (tool, category) in [
            ("exec", ToolCategory::Exec),
            ("spawn_subagent", ToolCategory::Exec),
            ("web_fetch", ToolCategory::Net),
        ] {
            let decision = needs_permission(
                PermissionLevel::WorkspaceFree,
                tool,
                &serde_json::json!({}),
                Path::new("."),
                &HashSet::new(),
                category,
            );
            assert!(
                matches!(decision, PermissionDecision::AskUser { .. }),
                "Level 3 must ask before {tool}"
            );
        }
    }
}
