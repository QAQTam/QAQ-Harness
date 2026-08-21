//! Typed auxiliary HTTP endpoints used beside the Ringing event/command plane.
//!
//! These enums keep legacy service method names and JSON assembly inside the
//! transport crate. Native shells choose a closed Rust variant; they cannot
//! mistype a method name, send a mutation through the query route, or invent a
//! second renderer-facing protocol.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryRequest {
    SessionList,
    SessionActivity,
    ConfigLoad,
    WorkspaceStatus,
    WorkspaceList,
    SkillsListTools,
    WorkspaceDiagnose,
    /// 列出 daemon 侧目录内容（远端文件选择器数据源）。
    FsList {
        path: String,
    },
    /// 读取 daemon 侧文件内容（文本预览，最多 `max_bytes`）。
    FsRead {
        path: String,
        max_bytes: Option<u64>,
    },
}

impl QueryRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SessionList => ("session.list", json!({})),
            Self::SessionActivity => ("session.activity", json!({})),
            Self::ConfigLoad => ("config.load", json!({})),
            Self::WorkspaceStatus => ("workspace.status", json!({})),
            Self::WorkspaceList => ("workspace.list", json!({})),
            Self::SkillsListTools => ("skills.list_tools", json!({})),
            Self::WorkspaceDiagnose => ("workspace.diagnose", json!({})),
            Self::FsList { path } => ("fs.list", json!({ "path": path })),
            Self::FsRead { path, max_bytes } => {
                let mut params = json!({ "path": path });
                if let Some(max_bytes) = max_bytes {
                    params["max_bytes"] = json!(max_bytes);
                }
                ("fs.read", params)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionRequest {
    SkillsOperation {
        seed: String,
        operation_id: String,
        action: String,
        name: String,
        expected_revision: u64,
    },
    SkillsReload {
        seed: String,
    },
    ConfigSave {
        fields: Value,
    },
    ConfigSetPermissionLevel {
        level: u64,
    },
    ProfileApply {
        name: String,
    },
    ProfileSaveCurrent {
        name: String,
    },
    ProfileDelete {
        name: String,
    },
    WorkspaceSet {
        seed: String,
        path: String,
    },
    WorkspaceSetMode {
        mode: String,
    },
    WorkspaceInstallWsl,
    /// 注册一个目录为 UI 工作区（组织语义；daemon `workspace.create`）。
    WorkspaceCreate {
        path: String,
    },
    /// 重命名工作区（daemon `workspace.rename`）。
    WorkspaceRename {
        id: String,
        title: String,
    },
    /// 删除工作区注册（不删会话；daemon `workspace.delete`）。
    WorkspaceDelete {
        id: String,
    },
    /// 把会话移入指定工作区（daemon `workspace.move_session`）。
    WorkspaceMoveSession {
        seed: String,
        workspace_id: String,
    },
    /// 把会话移出工作区 → 未分组（daemon `workspace.detach`）。
    WorkspaceDetach {
        seed: String,
    },
    /// 切换会话工具模式（standard/minimal/custom，PLAN-TOOL-MODES.md）。
    /// daemon 侧先持久化 meta.json（persist_tool_mode）再经 Control 频道
    /// 下发 worker 应用（set_allowed_tools + tool_defs 刷新）。
    SessionSetToolMode {
        seed: String,
        tool_mode: String,
        custom_tools: Vec<String>,
    },
    /// Spawn an isolated subagent worker (daemon `subagent.spawn`). Returns
    /// `{ "seed": "<8-hex>" }`; the caller then attaches the seed and drives
    /// it with ordinary Ringing commands/events.
    SubagentSpawn {
        /// Tool allowlist (empty = all tools available).
        tools: Vec<String>,
        /// Model override; `None` = inherit parent config.
        model: Option<String>,
        /// API base URL override; `None` = inherit parent config.
        base_url: Option<String>,
        /// Max output tokens override.
        max_tokens: Option<u32>,
        /// Workspace the subagent inherits from the parent agent. Persisted to
        /// the subagent's `SessionMeta.cwd` before the worker starts, so the
        /// subagent resolves relative paths and enforces its permission
        /// boundary against the *parent's* workspace.
        workspace: Option<String>,
    },
}

impl ActionRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SkillsOperation {
                seed,
                operation_id,
                action,
                name,
                expected_revision,
            } => (
                "skills.operation",
                json!({
                    "seed": seed,
                    "operationId": operation_id,
                    "action": action,
                    "name": name,
                    "expectedRevision": expected_revision,
                }),
            ),
            Self::SkillsReload { seed } => ("skills.reload", json!({ "seed": seed })),
            Self::ConfigSave { fields } => ("config.save", fields),
            Self::ConfigSetPermissionLevel { level } => {
                ("config.set_permission_level", json!({ "level": level }))
            }
            Self::ProfileApply { name } => ("profile.apply", json!({ "name": name })),
            Self::ProfileSaveCurrent { name } => ("profile.save_current", json!({ "name": name })),
            Self::ProfileDelete { name } => ("profile.delete", json!({ "name": name })),
            Self::WorkspaceSet { seed, path } => {
                ("workspace.set", json!({ "seed": seed, "path": path }))
            }
            Self::WorkspaceSetMode { mode } => ("workspace.set_mode", json!({ "mode": mode })),
            Self::WorkspaceInstallWsl => ("workspace.install_wsl", json!({})),
            Self::WorkspaceCreate { path } => ("workspace.create", json!({ "path": path })),
            Self::WorkspaceRename { id, title } => {
                ("workspace.rename", json!({ "id": id, "title": title }))
            }
            Self::WorkspaceDelete { id } => ("workspace.delete", json!({ "id": id })),
            Self::WorkspaceMoveSession { seed, workspace_id } => (
                "workspace.move_session",
                json!({ "seed": seed, "workspace_id": workspace_id }),
            ),
            Self::WorkspaceDetach { seed } => ("workspace.detach", json!({ "seed": seed })),
            Self::SessionSetToolMode {
                seed,
                tool_mode,
                custom_tools,
            } => (
                "session.set_tool_mode",
                json!({
                    "seed": seed,
                    "tool_mode": tool_mode,
                    "custom_tools": custom_tools,
                }),
            ),
            Self::SubagentSpawn {
                tools,
                model,
                base_url,
                max_tokens,
                workspace,
            } => {
                let mut params = json!({ "tools": tools });
                if let Some(model) = model {
                    params["model"] = json!(model);
                }
                if let Some(base_url) = base_url {
                    params["base_url"] = json!(base_url);
                }
                if let Some(max_tokens) = max_tokens {
                    params["max_tokens"] = json!(max_tokens);
                }
                if let Some(workspace) = workspace {
                    params["workspace"] = json!(workspace);
                }
                ("subagent.spawn", params)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_set_tool_mode_uses_action_route() {
        let (name, params) = ActionRequest::SessionSetToolMode {
            seed: "s1".into(),
            tool_mode: "minimal".into(),
            custom_tools: vec!["bash".into(), "edit".into()],
        }
        .into_parts();
        assert_eq!(name, "session.set_tool_mode");
        assert_eq!(params["seed"], "s1");
        assert_eq!(params["tool_mode"], "minimal");
        assert_eq!(params["custom_tools"][0], "bash");
    }

    #[test]
    fn workspace_set_is_an_action_not_a_query() {
        let (name, params) = ActionRequest::WorkspaceSet {
            seed: "s1".into(),
            path: "C:/work".into(),
        }
        .into_parts();
        assert_eq!(name, "workspace.set");
        assert_eq!(params["seed"], "s1");
    }

    #[test]
    fn query_variants_have_no_call_site_method_strings() {
        let (name, params) = QueryRequest::SessionList.into_parts();
        assert_eq!(name, "session.list");
        assert_eq!(params, json!({}));
    }
}
