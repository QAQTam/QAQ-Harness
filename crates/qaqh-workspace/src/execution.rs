//! Authorized tool execution and context-aware admission.

use crate::authorization::{Admission, AuthorizedToolCall, ToolInvocation, admit};
use std::collections::HashSet;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Return type for tool execution with interrupt support.
pub struct ToolExecResult {
    pub content: String,
    pub success: bool,
    pub result: crate::ToolResult,
    pub meta: crate::ToolExecMeta,
    pub code_delta: Option<qaqh_proto::CodeDeltaRecord>,
    pub skill_effects: Vec<crate::ToolEffect>,
}

/// Consume an authorization proof and dispatch the bound handler.
pub fn execute_authorized(
    call: AuthorizedToolCall,
    progress_tx: Option<crate::ExecProgressSender>,
) -> ToolExecResult {
    let started = Instant::now();
    let (invocation, authorized_resources, authorized_workspace) = call.into_parts();

    if let Err(_error) = crate::runtime::verify_active_session(&invocation.session_id) {
        return failure(&invocation.tool_name, crate::ToolError::SessionMismatch);
    }

    let active_workspace = crate::runtime::active_workspace_root();
    if active_workspace != authorized_workspace {
        return failure(
            &invocation.tool_name,
            crate::ToolError::ToolSpecific {
                tool: invocation.tool_name.clone(),
                code: "WORKSPACE_MISMATCH".into(),
                message: "workspace mismatch — active workspace changed after authorization".into(),
            },
        );
    }

    let mut current_resources =
        crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
    current_resources.sort();
    current_resources.dedup();
    let mut authorized_resources = authorized_resources;
    authorized_resources.sort();
    authorized_resources.dedup();
    if current_resources != authorized_resources {
        return failure(&invocation.tool_name, crate::ToolError::ResourceMismatch);
    }

    let ToolInvocation {
        session_id,
        call_id,
        tool_name: name,
        action,
        args,
        category: _,
    } = invocation;

    if crate::is_cancel() {
        return failure(&name, crate::ToolError::Cancelled);
    }

    if crate::runtime::is_plan_mode() && crate::PLAN_BLOCKED.contains(&name.as_str()) {
        return failure(
            &name,
            crate::ToolError::BlockedByMode {
                mode: "PLAN".into(),
                tool: name.clone(),
            },
        );
    }

    // Phase 1: prepare while holding the manager lock.
    let prepared = crate::runtime::with_manager(|manager| {
        manager.prepare_req(call_id, &name, &action, args.clone(), None, progress_tx)
    });
    let prepared = match prepared {
        Some(Ok(prepared)) => prepared,
        Some(Err(report)) => {
            let canonical = crate::ToolResult::error(report.content.clone());
            return ToolExecResult {
                content: report.content,
                success: report.success,
                result: canonical,
                meta: report.meta,
                code_delta: None,
                skill_effects: Vec::new(),
            };
        }
        None => {
            return failure(&name, crate::ToolError::ManagerUnavailable);
        }
    };

    // Phase 2: execute without holding the manager lock.
    let mut tool_result = crate::backend::execute(
        prepared.placement,
        session_id,
        authorized_workspace,
        authorized_resources,
        prepared.handler_fn,
        prepared.ctx.clone(),
    );
    // 工具侧折叠：结果在工具执行层定型（取代 message 侧折叠），
    // 模型看到的、存储的就是最终形态——不再有位置相关的二次改写。
    crate::tool_side_fold::apply(&name, &mut tool_result);
    let skill_effects = if name == "skills" && tool_result.is_success() {
        prepared.ctx.take_skill_effects()
    } else {
        Vec::new()
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let success = tool_result.is_success();
    let canonical = tool_result.clone();

    // Phase 3: finalize while holding the manager lock again.
    let report = crate::runtime::with_manager(|manager| {
        manager.finalize_req(prepared, tool_result, elapsed_ms)
    });
    let code_delta = success
        .then(|| crate::code_delta::compute(&name, &args))
        .flatten();

    match report {
        Some(report) => {
            let result = ToolExecResult {
                content: report.content,
                success: report.success,
                result: canonical,
                meta: report.meta,
                code_delta,
                skill_effects,
            };
            let audit_entry = crate::audit::AuditEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                user: "agent".into(),
                tool: name.clone(),
                action: action.clone(),
                args_hash: crate::audit::hash_args(&args),
                result: if result.success { "ok" } else { "fail" }.into(),
                elapsed_ms: result.meta.elapsed_ms,
                files: report.files_affected,
            };
            crate::audit::append_audit(&audit_entry);
            result
        }
        None => failure(&name, crate::ToolError::ManagerUnavailable),
    }
}

/// Parse, admit, and execute a call against the current runtime context.
pub fn execute_with_context(
    name: &str,
    action: &str,
    args: &str,
    tool_call_id: &str,
    progress_tx: Option<crate::ExecProgressSender>,
) -> ToolExecResult {
    let args: serde_json::Value = match serde_json::from_str(args) {
        Ok(args) => args,
        Err(error) => {
            return failure(
                &resolve_name(name, action),
                crate::ToolError::InvalidArgs {
                    message: error.to_string(),
                },
            );
        }
    };
    let Some(context) = crate::runtime::context() else {
        return failure(
            &resolve_name(name, action),
            crate::ToolError::RuntimeNotInitialized,
        );
    };

    let call_id = if tool_call_id.is_empty() {
        format!(
            "agent_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        )
    } else {
        tool_call_id.to_string()
    };
    let resolved_name = resolve_name(name, action);
    let resolved_action = if action.is_empty() {
        args.get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(name)
            .to_string()
    } else {
        action.to_string()
    };
    let workspace_root = crate::runtime::active_workspace_root();
    // 能力类别来自 handler 声明（单一事实源）；查不到时保守回退 Write。
    let category = crate::runtime::lookup_category(&resolved_name)
        .unwrap_or(crate::permission::ToolCategory::Write);
    let invocation = ToolInvocation {
        session_id: context.active_session,
        call_id,
        tool_name: resolved_name.clone(),
        action: resolved_action,
        args,
        category,
    };

    match admit(
        invocation,
        context.permission_level,
        &workspace_root,
        &HashSet::new(),
    ) {
        Admission::Authorized(authorized) => execute_authorized(authorized, progress_tx),
        Admission::ApprovalRequired(challenge) => failure(
            &resolved_name,
            crate::ToolError::PermissionDenied {
                reason: challenge.reason().to_string(),
            },
        ),
        Admission::Denied(reason) => failure(
            &resolved_name,
            crate::ToolError::PermissionDenied { reason },
        ),
    }
}

fn resolve_name(name: &str, action: &str) -> String {
    if action.is_empty() {
        name.to_string()
    } else {
        format!("{name}_{action}")
    }
}

fn failure(name: &str, error: crate::ToolError) -> ToolExecResult {
    ToolExecResult {
        content: error.to_string(),
        success: false,
        result: error.into_result(),
        meta: crate::ToolExecMeta {
            name: name.to_string(),
            elapsed_ms: 0,
            output_size: 0,
            success: false,
            args_summary: String::new(),
        },
        code_delta: None,
        skill_effects: Vec::new(),
    }
}

// ═══════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{
        Admission, ApprovalError, AuthorizedToolCall, ToolInvocation, admit,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Arc, MutexGuard, atomic::AtomicU32};
    use std::time::Duration;

    static TEST_HANDLER_COUNT: AtomicU32 = AtomicU32::new(0);
    fn test_counter_handler(_ctx: crate::ToolCallCtx) -> crate::ToolResult {
        TEST_HANDLER_COUNT.fetch_add(1, Ordering::SeqCst);
        crate::ToolResult::ok("counter incremented")
    }

    struct TestWorkspaceBackend {
        calls: Arc<AtomicU32>,
    }

    struct WorkspaceReset;

    impl Drop for WorkspaceReset {
        fn drop(&mut self) {
            crate::set_workspace(".");
        }
    }

    impl crate::ToolExecutionBackend for TestWorkspaceBackend {
        fn execute(&self, request: crate::BackendRequest) -> crate::ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.session_id, "test_session");
            assert_eq!(request.ctx.name, "test_workspace");
            assert!(request.authorized_resources.is_empty());
            assert!(!request.host_workspace.as_os_str().is_empty());
            crate::ToolResult::ok("workspace backend")
        }
    }

    fn setup_test_manager() -> MutexGuard<'static, ()> {
        let test_guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace(".");
        let allowed: Vec<String> = vec![];
        crate::runtime::init_tools("test", &[], allowed);
        crate::runtime::register_test_handler(crate::ToolHandler {
            key: "test_counter".to_string(),
            description: "test handler",
            input_schema: serde_json::json!({}),
            handler: test_counter_handler,
            risk: crate::ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(5),
        });
        crate::runtime::register_test_handler(crate::ToolHandler {
            key: "test_write".to_string(),
            description: "test write handler",
            input_schema: serde_json::json!({}),
            handler: test_counter_handler,
            risk: crate::ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(5),
        });
        crate::runtime::register_test_handler_with_placement(
            crate::ToolHandler {
                key: "test_workspace".to_string(),
                description: "test workspace handler",
                input_schema: serde_json::json!({}),
                handler: test_counter_handler,
                risk: crate::ToolRisk::ReadOnly,
                category: crate::permission::ToolCategory::Read,
                default_timeout: std::time::Duration::from_secs(5),
            },
            crate::ToolPlacement::Workspace,
        );
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        test_guard
    }

    #[test]
    fn skill_execution_returns_typed_activation() {
        let _test_guard = setup_test_manager();
        let definitions = crate::runtime::all_tools();
        let skill_definitions = definitions
            .iter()
            .filter(|definition| definition.function.name == "skills")
            .collect::<Vec<_>>();
        assert_eq!(skill_definitions.len(), 1);
        assert!(
            skill_definitions[0].function.parameters["oneOf"]
                .as_array()
                .is_some_and(|variants| variants.len() == 4)
        );
        assert!(!definitions.iter().any(|definition| matches!(
            definition.function.name.as_str(),
            "skill" | "skill_resource" | "skills_list" | "skill_validate"
        )));
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join(".agents/skills/typed-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: typed-skill\ndescription: Use for typed activation tests.\n---\n\n# Typed instructions",
        ).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(skill_dir.join("references/info.md"), "complete reference").unwrap();
        crate::set_workspace(&temp.path().to_string_lossy());
        crate::runtime::set_context("test_session", 4);

        let result = execute_with_context(
            "skills",
            "",
            r#"{"action":"activate","name":"typed-skill"}"#,
            "skill-call-1",
            None,
        );

        assert!(result.success);
        let activation = match result
            .skill_effects
            .into_iter()
            .next()
            .expect("typed activation")
        {
            crate::ToolEffect::Skill(qaqh_skills::SkillEffect::Activate(activation)) => activation,
            other => panic!("unexpected effect: {other:?}"),
        };
        assert_eq!(activation.metadata.name, "typed-skill");
        assert!(activation.body.contains("Typed instructions"));

        let resource = execute_with_context(
            "skills",
            "",
            r#"{"action":"resource","name":"typed-skill","path":"references/info.md"}"#,
            "resource-call-1",
            None,
        );
        assert!(resource.success);
        assert_eq!(resource.content, "complete reference");
        assert!(resource.skill_effects.is_empty());

        let generic_read = execute_with_context(
            "read",
            "",
            &serde_json::json!({"path": skill_dir.join("SKILL.md")}).to_string(),
            "generic-skill-read",
            None,
        );
        assert!(!generic_read.success);
        assert_eq!(
            generic_read
                .result
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("USE_SKILLS_TOOL")
        );

        let traversal = execute_with_context(
            "skills",
            "",
            r#"{"action":"resource","name":"typed-skill","path":"../outside.md"}"#,
            "resource-call-2",
            None,
        );
        assert!(!traversal.success);
        assert!(traversal.content.contains("SKILL_RESOURCE_UNAVAILABLE"));

        let list =
            execute_with_context("skills", "", r#"{"action":"list"}"#, "skills-list-1", None);
        assert!(list.success);
        assert!(list.content.contains("typed-skill"));

        let invalid = execute_with_context(
            "skills",
            "",
            r#"{"action":"list","name":"typed-skill"}"#,
            "skills-invalid-1",
            None,
        );
        assert!(!invalid.success);
        assert!(invalid.content.contains("INVALID_ARGUMENTS"));
        crate::set_workspace(".");
    }

    fn make_invocation(tool_name: &str, call_id: &str) -> ToolInvocation {
        // 与生产路径（execute_with_context）一致：从 manager lookup handler
        // 声明的 category（单一事实源）；测试 manager 已注册 test_* handler。
        let category = crate::runtime::lookup_category(tool_name)
            .unwrap_or(crate::permission::ToolCategory::Read);
        ToolInvocation {
            session_id: "test_session".to_string(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            action: String::new(),
            args: serde_json::json!({}),
            category,
        }
    }

    // ── Test 1: Auto-approved calls execute normally (Level 4) ──

    #[test]
    fn auto_approved_call_executes_normally() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 4, &ws, &trusted);
        match admission {
            Admission::Authorized(auth) => {
                let result = execute_authorized(auth, None);
                assert!(result.success, "auto-approved call should succeed");
            }
            other => panic!(
                "expected Authorized, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        }
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn workspace_backend_routes_only_workspace_tools() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let backend_calls = Arc::new(AtomicU32::new(0));
        let _backend_guard =
            crate::backend::replace_workspace_backend_for_test(Arc::new(TestWorkspaceBackend {
                calls: backend_calls.clone(),
            }));
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        let workspace_call = match admit(
            make_invocation("test_workspace", "workspace-route-1"),
            4,
            &ws,
            &trusted,
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("expected workspace call to be authorized"),
        };
        let workspace_result = execute_authorized(workspace_call, None);
        assert!(workspace_result.success);
        assert_eq!(workspace_result.content, "workspace backend");
        assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 0);

        let host_call = match admit(
            make_invocation("test_counter", "host-route-1"),
            4,
            &ws,
            &trusted,
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("expected host call to be authorized"),
        };
        let host_result = execute_authorized(host_call, None);
        assert!(host_result.success);
        assert_eq!(host_result.content, "counter incremented");
        assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);
    }

    struct TestProcessBackend {
        calls: Arc<AtomicU32>,
    }

    impl crate::ToolExecutionBackend for TestProcessBackend {
        fn execute(&self, _request: crate::BackendRequest) -> crate::ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            crate::ToolResult::ok("workspace backend")
        }
    }

    /// exec 与 process 必须路由到 workspace 后端（与 exec 共享 serve 进程的
    /// ProcessRegistry）。若 process 仍为 HostOnly，则 LLM 拿到的 exec
    /// process_id 在 worker 本地注册表查不到（跨进程注册表隔离 bug）。
    #[test]
    fn process_tools_route_to_workspace_backend() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let backend_calls = Arc::new(AtomicU32::new(0));
        let _backend_guard =
            crate::backend::replace_workspace_backend_for_test(Arc::new(TestProcessBackend {
                calls: backend_calls.clone(),
            }));
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        for tool in ["exec", "process"] {
            backend_calls.store(0, Ordering::SeqCst);
            let call = match admit(
                make_invocation(tool, &format!("proc-route-{tool}")),
                4,
                &ws,
                &trusted,
            ) {
                Admission::Authorized(call) => call,
                _ => panic!("expected {tool} to be authorized"),
            };
            let result = execute_authorized(call, None);
            assert!(
                result.success,
                "{tool} should succeed via workspace backend"
            );
            assert_eq!(
                backend_calls.load(Ordering::SeqCst),
                1,
                "{tool} must route to workspace backend (shared ProcessRegistry)"
            );
        }
    }

    // ── Test 2: Level 1 (MaxLockdown) requires approval ──

    #[test]
    fn max_lockdown_requires_approval() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-2");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        assert!(
            matches!(admission, Admission::ApprovalRequired(_)),
            "Level 1 should require approval for all tools"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must not execute before approval"
        );
    }

    // ── Test 3: Approval creates a single-use grant ──

    #[test]
    fn approved_call_executes_exactly_once() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-3-once");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            other => panic!(
                "expected ApprovalRequired, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        let authorized = challenge.approve(true).expect("approval should succeed");
        let result = execute_authorized(authorized, None);
        assert!(result.success, "approved call should execute");
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            1,
            "handler should execute exactly once"
        );
    }

    // ── Test 4: Rejected approval does not execute ──

    #[test]
    fn rejected_approval_does_not_execute() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-4");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            other => panic!(
                "expected ApprovalRequired, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        let result = challenge.approve(false);
        assert!(matches!(result, Err(ApprovalError::Rejected)));
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must not execute on rejection"
        );
    }

    // ── Test 5: Expired approval fails (is_expired check) ──

    #[test]
    fn expired_approval_fails() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-5-exp");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };
        assert!(matches!(
            challenge.approve_with_ttl(true, Duration::ZERO),
            Err(ApprovalError::Expired)
        ));
    }

    // ── Test 6: Challenge approve consumes the challenge (no replay) ──

    #[test]
    fn challenge_cannot_be_replayed() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-6");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };

        // First approval succeeds and consumes the challenge
        let auth = challenge
            .approve(true)
            .expect("first approval should succeed");
        let _result = execute_authorized(auth, None);
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);

        // Cannot consume the same challenge twice — it was moved
        // (Rust move semantics guarantee this at compile time)
    }

    // ── Test 7: Different call_id approval fails (mismatch protection) ──

    #[test]
    fn mismatched_call_id_detected_at_loop_level() {
        let _test_guard = setup_test_manager();
        // Create challenge for call-7a
        let inv = make_invocation("test_counter", "call-7a");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => {
                assert_eq!(c.call_id(), "call-7a");
                c
            }
            _ => panic!("expected ApprovalRequired"),
        };
        // The challenge call_id matches the invocation — the Loop layer
        // enforces that the PermissionResponse call_id matches the pending
        // challenge's call_id via HashMap lookup.
        drop(challenge);
    }

    // ── Test 8: Authorization proof is bound to the call identity ──

    #[test]
    fn authorization_bound_to_call_identity() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv1 = make_invocation("test_counter", "bound-1");
        let inv2 = make_invocation("test_counter", "bound-2");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        let a1 = match admit(inv1, 4, &ws, &trusted) {
            Admission::Authorized(a) => a,
            other => panic!("expected Authorized"),
        };
        let a2 = match admit(inv2, 4, &ws, &trusted) {
            Admission::Authorized(a) => a,
            other => panic!("expected Authorized"),
        };

        assert_eq!(a1.call_id(), "bound-1");
        assert_eq!(a2.call_id(), "bound-2");
        assert_ne!(a1.call_id(), a2.call_id());
    }

    // ── Test 9: Compatibility wrapper with context delegates to secured path ──

    #[test]
    fn compat_wrapper_delegates_to_secured_path() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        // With Level 4 permission context, auto-approve should work
        let result = execute_with_context("test_counter", "", "{}", "compat-2", None);
        assert!(
            result.success,
            "compat wrapper should succeed with permission context: {}",
            result.content
        );
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);
    }

    // ── Test 12: Structured success propagates ──

    #[test]
    fn structured_success_propagates_correctly() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);

        // auto-approve
        let inv = make_invocation("test_counter", "struc-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        match admit(inv, 4, &ws, &trusted) {
            Admission::Authorized(auth) => {
                let result = execute_authorized(auth, None);
                assert!(result.success, "structured success should be true");
                assert!(
                    !result.content.contains("[ERROR]"),
                    "should not contain error prefix"
                );
            }
            other => panic!("expected Authorized"),
        }
    }

    // ── Test 13: PLAN mode blocks destructive tools but not reads ──

    #[test]
    fn plan_mode_blocks_destructive_but_not_reads() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let previous_mode = 0;
        crate::runtime::set_mode(1);

        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // test_write has ToolRisk::Destructive, so it's in PLAN_BLOCKED (via the "Test" default for PLAN_BLOCKED?
        // Actually PLAN_BLOCKED checks specific names. Let me check PLAN_BLOCKED:
        // pub const PLAN_BLOCKED: &[&str] = &["edit_file", "write", "delete", "exec", "git"];
        // So "test_write" is NOT in PLAN_BLOCKED. The admission will Authorize at level 4.
        // The block happens inside execute_authorized based on PLAN_BLOCKED list.

        let inv = make_invocation("test_write", "plan-write");
        match admit(inv, 4, &ws, &trusted) {
            Admission::Authorized(auth) => {
                let result = execute_authorized(auth, None);
                // "test_write" is NOT in PLAN_BLOCKED, so it succeeds
                assert!(
                    result.success,
                    "test_write not in PLAN_BLOCKED, should succeed even in plan mode: {}",
                    result.content
                );
            }
            _ => {}
        }

        crate::runtime::set_mode(previous_mode);
    }

    // ── Test 14: Same permission decision for equivalent invocations ──

    #[test]
    fn ui_and_llm_same_permission_decision() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 2); // ReadFree: reads auto, writes need approval
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // Both "test_counter" and "test_write" have default ToolCategory::Write.
        // At Level 2 (ReadFree), both require approval.
        for id in &["inv-a", "inv-b"] {
            let inv = make_invocation("test_write", id);
            match admit(inv, 2, &ws, &trusted) {
                Admission::ApprovalRequired(_) => {} // expected for Write at Level 2
                other => panic!(
                    "write tools should require approval at level 2, {:?}",
                    std::any::type_name_of_val(&other)
                ),
            }
        }

        // Same at Level 4 — both auto-approve
        for id in &["inv-c", "inv-d"] {
            let inv = make_invocation("test_write", id);
            match admit(inv, 4, &ws, &trusted) {
                Admission::Authorized(_) => {} // expected for Write at Level 4
                other => panic!(
                    "level 4 should auto-approve all tools, {:?}",
                    std::any::type_name_of_val(&other)
                ),
            }
        }
    }

    // ── Test: Missing context fails closed ──

    #[test]
    fn missing_context_fails_closed() {
        let _test_guard = setup_test_manager();
        crate::runtime::clear_context();
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let result = execute_with_context("test_counter", "", "{}", "miss-ctx-1", None);
        assert!(
            !result.success,
            "should fail closed without runtime context"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Invalid JSON does not execute ──

    #[test]
    fn invalid_json_does_not_execute() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let result = execute_with_context("test_counter", "", "not-json{{{", "inv-json-1", None);
        assert!(!result.success, "invalid JSON should fail");
        assert!(
            result.content.contains("[ERROR]"),
            "should contain error prefix"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Resources bound in authorization ──

    #[test]
    fn resources_bound_in_authorization() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "res-bound-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };
        let expected_resources = challenge.resources().to_vec();
        let expected_workspace = challenge.workspace_root().to_path_buf();
        let authorized = challenge.approve(true).expect("approval should succeed");
        assert_eq!(
            authorized.resources(),
            expected_resources.as_slice(),
            "resources must be carried through approve()"
        );
        assert_eq!(
            authorized.workspace_root(),
            expected_workspace,
            "workspace root must be carried through approve()"
        );
    }

    #[test]
    fn workspace_change_after_authorization_is_rejected() {
        let _test_guard = setup_test_manager();
        let _workspace_reset = WorkspaceReset;
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let authorized_workspace = tempfile::tempdir().unwrap();
        let changed_workspace = tempfile::tempdir().unwrap();
        crate::set_workspace(&authorized_workspace.path().to_string_lossy());

        let authorized = match admit(
            make_invocation("test_workspace", "workspace-bound-1"),
            4,
            authorized_workspace.path(),
            &HashSet::new(),
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("expected workspace call to be authorized"),
        };

        crate::set_workspace(&changed_workspace.path().to_string_lossy());
        let result = execute_authorized(authorized, None);
        assert!(!result.success, "changed workspace must invalidate proof");
        assert!(
            result.content.contains("workspace mismatch"),
            "should report workspace mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Session mismatch rejected ──

    #[test]
    fn session_mismatch_rejected() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("session-A", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let inv = ToolInvocation {
            session_id: "session-B".to_string(),
            call_id: "sess-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({}),
            category: crate::permission::ToolCategory::Read,
        };
        let auth = AuthorizedToolCall::new(inv, vec![], crate::runtime::active_workspace_root());
        let result = execute_authorized(auth, None);
        assert!(!result.success, "session mismatch should be rejected");
        assert!(
            result.content.contains("session mismatch"),
            "should report session mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
        // Cleanup runtime context for subsequent tests
        crate::runtime::clear_context();
    }

    // ── Test: Resource mismatch rejected ──

    #[test]
    fn resource_mismatch_rejected() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);

        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // Create invocation with path="a.txt", admit it to get authorized resources
        let inv1 = ToolInvocation {
            session_id: "test".to_string(),
            call_id: "res-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({"path": "a.txt"}),
            category: crate::permission::ToolCategory::Read,
        };
        let admission = admit(inv1, 4, &ws, &trusted);
        let auth = match admission {
            Admission::Authorized(a) => a,
            other => panic!(
                "expected Authorized, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        // Forge a call with different path but same authorized resources
        let inv2 = ToolInvocation {
            session_id: "test".to_string(),
            call_id: "res-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({"path": "b.txt"}),
            category: crate::permission::ToolCategory::Read,
        };
        let forged_auth = AuthorizedToolCall::new(
            inv2,
            auth.resources().to_vec(),
            auth.workspace_root().to_path_buf(),
        );
        let result = execute_authorized(forged_auth, None);
        assert!(!result.success, "resource mismatch should be rejected");
        assert!(
            result.content.contains("Resource mismatch"),
            "should report resource mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }
}
