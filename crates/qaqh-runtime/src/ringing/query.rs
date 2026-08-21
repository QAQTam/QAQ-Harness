//! Ringing 只读查询（`POST /ringing/v1/queries/{name}`）。
//!
//! 实现 session.list / session.meta / session.activity 三个只读 RPC 的
//! 中立 JSON 形状，复用 legacy `QaqhService::handle` 语义（查询只读，
//! 不伪装成 Command/Event）。

use serde_json::{Value, json};

use crate::QaqhService;

/// 查询分发：仅允许只读方法；未知方法返回 Err（HTTP 404）。
pub fn query(service: &QaqhService, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "daemon.version"
        | "session.list"
        | "session.meta"
        | "session.activity"
        | "session.dashboard"
        | "session.get_activity"
        | "workspace.get"
        | "workspace.status"
        | "workspace.list"
        | "fs.list"
        | "fs.read"
        | "config.load"
        | "skills.list_tools"
        | "todo.status"
        | "plan.read"
        | "plan.context_stats"
        | "stats.token_usage"
        | "git.diff"
        | "git.branch"
        | "git.branches"
        | "git.file_diff" => service.handle(method, params),
        _ => Err(format!("unknown query method {method}")),
    }
}

/// 需要 `seed` 查询参数的只读方法（HTTP 层据此返回 400）。
pub fn requires_seed(method: &str) -> bool {
    matches!(
        method,
        "session.meta"
            | "session.dashboard"
            | "session.get_activity"
            | "workspace.get"
            | "todo.status"
            | "plan.read"
            | "plan.context_stats"
            | "git.diff"
            | "git.branch"
            | "git.branches"
            | "git.file_diff"
    )
}

/// 统一查询错误响应形状（daemon HTTP 层使用）。
pub fn error_response(message: &str) -> Value {
    json!({ "code": "query_failed", "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;

    // SessionManager 是全局单例，同一测试进程只能 init 一次；
    // 用 OnceLock 共享一个 service 实例（并行测试也不会重复初始化）。
    static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

    fn service() -> &'static QaqhService {
        SERVICE.get_or_init(QaqhService::init)
    }

    #[test]
    fn session_list_returns_array() {
        let result = query(service(), "session.list", &json!({})).expect("list");
        assert!(result.is_array());
    }

    #[test]
    fn session_activity_returns_array() {
        let result = query(service(), "session.activity", &json!({})).expect("activity");
        assert!(result.is_array());
    }

    #[test]
    fn session_meta_unknown_seed_is_null() {
        let result = query(
            service(),
            "session.meta",
            &json!({ "seed": "does-not-exist" }),
        )
        .expect("meta");
        assert!(result.is_null());
    }

    #[test]
    fn session_meta_requires_seed() {
        let error = query(service(), "session.meta", &json!({})).expect_err("missing seed");
        assert!(error.contains("seed"));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let error = query(service(), "session.delete", &json!({})).expect_err("unknown");
        assert!(error.contains("unknown query method"));
    }

    #[test]
    fn read_only_allowlist_covers_router_methods() {
        for method in [
            "daemon.version",
            "session.list",
            "session.meta",
            "session.activity",
            "session.dashboard",
            "session.get_activity",
            "workspace.get",
            "workspace.status",
            "config.load",
            "skills.list_tools",
            "todo.status",
        ] {
            let result = query(service(), method, &json!({ "seed": "x" }));
            // 只验证方法在白名单内（业务错误如 "session not found" 属正常），
            // 不要求对不存在的会话返回 Ok。
            assert!(
                !result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.contains("unknown query method")),
                "{method}"
            );
        }
    }

    #[test]
    fn requires_seed_matches_seed_dependent_methods() {
        assert!(requires_seed("session.meta"));
        assert!(requires_seed("session.dashboard"));
        assert!(requires_seed("session.get_activity"));
        assert!(requires_seed("workspace.get"));
        assert!(requires_seed("todo.status"));
        assert!(!requires_seed("session.list"));
        assert!(!requires_seed("daemon.version"));
        assert!(!requires_seed("config.load"));
    }
}
