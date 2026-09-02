//! Ringing 服务面方法表（`POST /ringing/v1/service/{method}` 的单一权威清单）。
//!
//! 旧 `/queries/{name}`（闭表白名单）与 `/actions/{name}`（前缀 allowlist）
//! 双端点及其 slash/dot 双别名容忍已合并于此：一个方法一个条目，
//! `Read` = 无副作用查询，`Write` = 变更操作。
//!
//! 会话生命周期（`session.new`/`session.resume`/`skills.activate`/`todo.cancel`/
//! `plan.action` 等命令语义方法）刻意不在表中——会话命令只走三频道
//! command envelope（开发标准 N5），服务面仅承载非会话 RPC。

use serde_json::Value;

use crate::QaqhService;

/// 方法类别：决定错误码形状（`query_failed` / `action_failed`），
/// 也是对调用方"只读 / 变更"的契约声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodInfo {
    pub kind: MethodKind,
    /// 要求 `seed` 参数并做 lease 归属校验（Read 中带会话作用域的子集；
    /// Write 一律在 params 携带 seed 时校验归属）。
    pub requires_seed: bool,
}

const READ: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_seed: false,
};
const READ_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_seed: true,
};
const WRITE: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_seed: false,
};

/// 方法表：未列出的名字返回 `None`（HTTP 404）。
pub fn lookup(method: &str) -> Option<MethodInfo> {
    match method {
        // daemon / 会话只读
        "daemon.version" => Some(READ),
        "session.list" => Some(READ),
        "session.meta" => Some(READ_SEEDED),
        "session.activity" => Some(READ),
        "session.dashboard" => Some(READ_SEEDED),
        "session.get_activity" => Some(READ_SEEDED),
        // workspace
        "workspace.get" => Some(READ_SEEDED),
        "workspace.status" => Some(READ),
        "workspace.list" => Some(READ),
        "workspace.diagnose" => Some(READ),
        "workspace.set" => Some(WRITE),
        "workspace.set_mode" => Some(WRITE),
        "workspace.install_wsl" => Some(WRITE),
        "workspace.create" => Some(WRITE),
        "workspace.rename" => Some(WRITE),
        "workspace.delete" => Some(WRITE),
        "workspace.move_session" => Some(WRITE),
        "workspace.detach" => Some(WRITE),
        // fs
        "fs.list" => Some(READ),
        "fs.read" => Some(READ),
        // config / profile
        "config.load" => Some(READ),
        "config.save" => Some(WRITE),
        "config.set_permission_level" => Some(WRITE),
        "profile.apply" => Some(WRITE),
        "profile.save_current" => Some(WRITE),
        "profile.delete" => Some(WRITE),
        // skills
        "skills.list_tools" => Some(READ),
        "skills.operation" => Some(WRITE),
        "skills.reload" => Some(WRITE),
        // todo / plan / stats
        "todo.status" => Some(READ_SEEDED),
        "plan.read" => Some(READ_SEEDED),
        "plan.context_stats" => Some(READ_SEEDED),
        "stats.token_usage" => Some(READ),
        // git（只读与变更分列）
        "git.diff" => Some(READ_SEEDED),
        "git.branch" => Some(READ_SEEDED),
        "git.branches" => Some(READ_SEEDED),
        "git.file_diff" => Some(READ_SEEDED),
        "git.switch_branch" => Some(WRITE),
        "git.commit" => Some(WRITE),
        // subagent / tool mode
        "subagent.spawn" => Some(WRITE),
        "session.set_tool_mode" => Some(WRITE),
        _ => None,
    }
}

/// 统一错误响应形状（daemon HTTP 层使用），按方法类别区分 code。
pub fn error_response(kind: MethodKind, message: &str) -> Value {
    let code = match kind {
        MethodKind::Read => "query_failed",
        MethodKind::Write => "action_failed",
    };
    serde_json::json!({ "code": code, "message": message })
}

/// 服务分发：方法表校验后的唯一入口。
pub fn dispatch(service: &QaqhService, method: &str, params: &Value) -> Result<Value, String> {
    service.handle(method, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    // SessionManager 是全局单例，同一测试进程只能 init 一次；
    // 用 OnceLock 共享一个 service 实例（并行测试也不会重复初始化）。
    static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

    fn service() -> &'static QaqhService {
        SERVICE.get_or_init(|| {
            qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
            QaqhService::init(qaqh_session::SessionManager::global())
        })
    }

    #[test]
    fn session_list_returns_array() {
        let result = dispatch(service(), "session.list", &serde_json::json!({})).expect("list");
        assert!(result.is_array());
    }

    #[test]
    fn read_methods_carry_read_kind() {
        let info = lookup("session.list").expect("listed");
        assert_eq!(info.kind, MethodKind::Read);
        assert!(!info.requires_seed);
        let info = lookup("session.meta").expect("listed");
        assert!(info.requires_seed);
    }

    #[test]
    fn lifecycle_methods_are_deliberately_absent() {
        // 会话生命周期只走 command envelope（N5），服务面不收。
        assert!(lookup("session.new").is_none());
        assert!(lookup("session.resume").is_none());
        assert!(lookup("skills.activate").is_none());
        assert!(lookup("todo.cancel").is_none());
        assert!(lookup("plan.action").is_none());
    }

    #[test]
    fn slash_alias_is_no_longer_accepted() {
        // 旧双别名（"session/list"）已拆除：单一规范形态 `module.method`。
        assert!(lookup("session/list").is_none());
    }

    #[test]
    fn error_response_shape_differs_by_kind() {
        assert_eq!(
            error_response(MethodKind::Read, "x")["code"],
            serde_json::json!("query_failed")
        );
        assert_eq!(
            error_response(MethodKind::Write, "x")["code"],
            serde_json::json!("action_failed")
        );
    }
}
