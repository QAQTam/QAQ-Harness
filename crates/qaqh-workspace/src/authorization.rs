//! Permission admission and single-use authorization proofs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// 子代理沙箱标志（per-actor）：`run_actor` subagent 分支在 actor 线程上设置。
//
// 沙箱语义（方案 B）：子代理**没有用户审批通道**——
// - 文件级操作（Read/Write）且全部路径在 workspace 内 → 自动批准
//   （等价 Level 3 的 workspace 内语义，子代理可以正常干活）；
// - 其余（Exec / Net / 跨 workspace 路径）→ **自动拒绝**（返回
//   `PermissionDenied` 失败，不产生 `ToolPermissionRequested` 弹窗事件，
//   不挂起回合）。
//
// 同时解决"卡死"（L1-L3 下子代理审批无人响应）与"越狱"（子代理
// 跨 workspace / exec / 网络访问）两个问题。
//
// thread-local：主代理与子代理并发时，子代理的沙箱不会误伤主代理的
// 工具准入（`admit` 只在同一 actor 线程上执行）。
thread_local! {
    static SUBAGENT_SANDBOX: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 启用/关闭子代理沙箱（仅 actor 线程内调用）。
pub fn set_subagent_sandbox(on: bool) {
    SUBAGENT_SANDBOX.with(|slot| slot.set(on));
}

/// 当前线程是否处于子代理沙箱模式。
pub fn is_subagent_sandbox() -> bool {
    SUBAGENT_SANDBOX.with(|slot| slot.get())
}

/// Identity of a single tool invocation destined for a handler.
pub struct ToolInvocation {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub action: String,
    pub args: serde_json::Value,
    /// 能力类别（handler 声明）：权限决策的单一事实源，取代名字表。
    pub category: crate::permission::ToolCategory,
}

/// Authorization proof required to dispatch a handler.
///
/// Fields and construction stay private to this crate. External callers can
/// only obtain a proof through [`admit`] or [`PermissionChallenge::approve`].
pub struct AuthorizedToolCall {
    invocation: ToolInvocation,
    resources: Vec<PathBuf>,
    workspace_root: PathBuf,
    _sealed: (),
}

impl AuthorizedToolCall {
    pub(crate) fn new(
        invocation: ToolInvocation,
        resources: Vec<PathBuf>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            invocation,
            resources,
            workspace_root,
            _sealed: (),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.invocation.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.invocation.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.invocation.tool_name
    }

    pub fn action(&self) -> &str {
        &self.invocation.action
    }

    pub fn args(&self) -> &serde_json::Value {
        &self.invocation.args
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn into_parts(self) -> (ToolInvocation, Vec<PathBuf>, PathBuf) {
        (self.invocation, self.resources, self.workspace_root)
    }
}

/// Result of the admission gate.
pub enum Admission {
    Authorized(AuthorizedToolCall),
    ApprovalRequired(PermissionChallenge),
    Denied(String),
}

/// Immutable snapshot of a call that requires user approval.
///
/// Approval consumes the challenge, making the grant single-use by type.
pub struct PermissionChallenge {
    session_id: String,
    call_id: String,
    tool_name: String,
    action: String,
    normalized_args: serde_json::Value,
    resources: Vec<PathBuf>,
    workspace_root: PathBuf,
    reason: String,
    category: crate::permission::ToolCategory,
    risk: crate::permission::PermissionRisk,
    consequence: String,
    created_at: Instant,
    _sealed: (),
}

impl PermissionChallenge {
    fn new(
        invocation: ToolInvocation,
        reason: String,
        resources: Vec<PathBuf>,
        workspace_root: PathBuf,
        category: crate::permission::ToolCategory,
        risk: crate::permission::PermissionRisk,
        consequence: String,
    ) -> Self {
        Self {
            session_id: invocation.session_id,
            call_id: invocation.call_id,
            tool_name: invocation.tool_name,
            action: invocation.action,
            normalized_args: invocation.args,
            resources,
            workspace_root,
            reason,
            category,
            risk,
            consequence,
            created_at: Instant::now(),
            _sealed: (),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn normalized_args(&self) -> &serde_json::Value {
        &self.normalized_args
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn category(&self) -> &crate::permission::ToolCategory {
        &self.category
    }

    pub fn risk(&self) -> crate::permission::PermissionRisk {
        self.risk
    }

    pub fn consequence(&self) -> &str {
        &self.consequence
    }

    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.created_at.elapsed() > ttl
    }

    pub fn approve(self, approved: bool) -> Result<AuthorizedToolCall, ApprovalError> {
        self.approve_with_ttl(approved, Duration::from_secs(120))
    }

    pub(crate) fn approve_with_ttl(
        self,
        approved: bool,
        ttl: Duration,
    ) -> Result<AuthorizedToolCall, ApprovalError> {
        if !approved {
            return Err(ApprovalError::Rejected);
        }
        if self.is_expired(ttl) {
            return Err(ApprovalError::Expired);
        }
        let invocation = ToolInvocation {
            session_id: self.session_id,
            call_id: self.call_id,
            tool_name: self.tool_name,
            action: self.action,
            args: self.normalized_args,
            category: self.category,
        };
        Ok(AuthorizedToolCall::new(
            invocation,
            self.resources,
            self.workspace_root,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    Rejected,
    Expired,
    MissingOrReplayed,
}

/// Evaluate permission policy and bind the resulting proof to normalized resources.
pub fn admit(
    invocation: ToolInvocation,
    permission_level: u8,
    workspace_root: &Path,
    trusted_dirs: &HashSet<PathBuf>,
) -> Admission {
    let workspace_root = crate::permission::resolve_target_path(workspace_root.to_path_buf());
    let level = crate::permission::PermissionLevel::from_u8(permission_level);
    match crate::permission::needs_permission(
        level,
        &invocation.tool_name,
        &invocation.args,
        &workspace_root,
        trusted_dirs,
        invocation.category,
    ) {
        crate::permission::PermissionDecision::AutoApprove => {
            let mut resources =
                crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
            resources.sort();
            resources.dedup();
            Admission::Authorized(AuthorizedToolCall::new(
                invocation,
                resources,
                workspace_root,
            ))
        }
        crate::permission::PermissionDecision::AskUser {
            reason,
            paths,
            category,
            risk,
            consequence,
        } => {
            if is_subagent_sandbox() {
                // 子代理沙箱：无审批通道，不产生弹窗事件。
                let file_ops = matches!(
                    category,
                    crate::permission::ToolCategory::Read | crate::permission::ToolCategory::Write
                );
                if file_ops && crate::permission::all_within_workspace(&paths, &workspace_root) {
                    // workspace 内文件操作：自动批准（等价 Level 3 语义）。
                    let mut resources = crate::permission::extract_target_paths(
                        &invocation.tool_name,
                        &invocation.args,
                    );
                    resources.sort();
                    resources.dedup();
                    Admission::Authorized(AuthorizedToolCall::new(
                        invocation,
                        resources,
                        workspace_root,
                    ))
                } else {
                    // Exec / Net / 跨 workspace：自动拒绝，防止越狱。
                    Admission::Denied(format!(
                        "subagent sandbox denied '{}': {reason}",
                        invocation.tool_name
                    ))
                }
            } else {
                Admission::ApprovalRequired(PermissionChallenge::new(
                    invocation,
                    reason,
                    paths,
                    workspace_root,
                    category,
                    risk,
                    consequence,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionRisk;

    #[test]
    fn approval_challenge_preserves_backend_risk_and_consequence() {
        // 与 sandbox 测试共享全局 SUBAGENT_SANDBOX：并行时沙箱置位会使本测试的
        // "level 1 write 必须要求审批"断言失败（沙箱下写自动批准）——串行化。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-risk");
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-a".into(),
            tool_name: "write".into(),
            action: String::new(),
            args: serde_json::json!({ "path": workspace.join("src/lib.rs") }),
            category: crate::permission::ToolCategory::Write,
        };

        let Admission::ApprovalRequired(challenge) =
            admit(invocation, 1, &workspace, &HashSet::new())
        else {
            panic!("level 1 write must require approval");
        };

        assert_eq!(challenge.risk(), PermissionRisk::Medium);
        assert_eq!(
            challenge.consequence(),
            "Changes files inside the current workspace."
        );
    }

    // ── 子代理沙箱（方案 B）──

    /// 持锁 + 置位沙箱；Drop 时复位。全局 AtomicBool 会被并行测试干扰，
    /// 必须经 `TEST_RUNTIME_SERIAL` 串行化（同 crate 其他全局状态测试）。
    fn sandbox_guard() -> impl Drop {
        struct Guard {
            _serial: std::sync::MutexGuard<'static, ()>,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                set_subagent_sandbox(false);
            }
        }
        let serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(true);
        Guard { _serial: serial }
    }

    fn invoke(
        tool: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        category: crate::permission::ToolCategory,
    ) -> Admission {
        admit(
            ToolInvocation {
                session_id: "sub-a".into(),
                call_id: "call-s".into(),
                tool_name: tool.into(),
                action: String::new(),
                args,
                category,
            },
            1, // 即使主代理是 MaxLockdown，沙箱下 workspace 内文件操作也自动批准
            ws,
            &HashSet::new(),
        )
    }

    #[test]
    fn sandbox_auto_approves_workspace_file_ops_even_at_level_1() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-a");
        // workspace 内写：Level 1 本应 AskUser，沙箱下自动批准。
        let admission = invoke(
            "write",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "expected authorized"
        );
        // workspace 内读：同样自动。
        let admission = invoke(
            "read",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Read,
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "expected authorized"
        );
    }

    #[test]
    fn sandbox_denies_cross_workspace_access() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-b");
        let outside = std::env::temp_dir().join("qaqh-sandbox-b-outside");
        let admission = invoke(
            "write",
            serde_json::json!({ "path": outside.join("secret.txt") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        match admission {
            Admission::Denied(reason) => {
                assert!(reason.contains("sandbox"), "{reason}");
            }
            other => panic!("cross-workspace write must be denied, got non-denied"),
        }
    }

    #[test]
    fn sandbox_denies_exec_and_net() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-c");
        for (tool, args) in [
            ("exec", serde_json::json!({ "command": "whoami" })),
            (
                "web_fetch",
                serde_json::json!({ "url": "https://example.com" }),
            ),
        ] {
            let category = if tool == "exec" {
                crate::permission::ToolCategory::Exec
            } else {
                crate::permission::ToolCategory::Net
            };
            let admission = invoke(tool, args, &ws, category);
            assert!(
                matches!(admission, Admission::Denied(_)),
                "{tool} must be denied in sandbox, got non-denied"
            );
        }
    }

    #[test]
    fn non_sandbox_still_requires_approval() {
        // 未启用沙箱：Level 1 写仍需审批（主代理行为不受影响）。
        // 全局 AtomicBool 需串行（与 sandbox_guard 同锁）。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-sandbox-d");
        let admission = invoke(
            "write",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        assert!(
            matches!(admission, Admission::ApprovalRequired(_)),
            "non-sandbox must keep approval path"
        );
    }
}
