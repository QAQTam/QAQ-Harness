//! ToolManager: tool registration, lookup, routing, and cancellation.
//!
//! Since v5: per-call execution metadata (ToolExecMeta) and cumulative
//! stats (ToolStats) are returned to the caller instead of being lost
//! to stderr. The caller (agent tools.rs) acts as a forwarding layer
//! that pushes these into UI events.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::{SafetyVerdict, ToolHandler, ToolPlacement};

// ── Execution metadata ──

#[derive(Clone, Debug)]
pub struct ToolExecMeta {
    pub name: String,
    pub elapsed_ms: u64,
    pub output_size: usize,
    pub success: bool,
    pub args_summary: String,
}

#[derive(Clone, Debug)]
pub struct ToolExecReport {
    pub content: String,
    pub success: bool,
    pub meta: ToolExecMeta,
    pub files_affected: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolStats {
    pub calls_total: u32,
    pub failures: u32,
    pub files_read: Vec<String>,
    pub files_written: Vec<String>,
}

pub struct ToolManager {
    pub(crate) handlers: BTreeMap<String, ToolHandler>,
    placements: BTreeMap<String, ToolPlacement>,
    allowed: Option<Vec<String>>,
    inflight_tasks: BTreeMap<String, Arc<AtomicBool>>,
    stats_total: u32,
    stats_failures: u32,
    files_read: Vec<String>,
    files_written: Vec<String>,
}

// ── Three-phase execution for parallel tool support ──

/// Prepared tool call, ready for execution without holding the manager lock.
pub(crate) struct PreparedCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) placement: ToolPlacement,
    pub(crate) handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
    pub(crate) ctx: crate::ToolCallCtx,
    pub(crate) audit_args: serde_json::Value,
}

impl Default for ToolManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolManager {
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
            placements: BTreeMap::new(),
            allowed: None,
            inflight_tasks: BTreeMap::new(),
            stats_total: 0,
            stats_failures: 0,
            files_read: Vec::new(),
            files_written: Vec::new(),
        }
    }

    pub fn register(&mut self, handler: ToolHandler) {
        self.register_with_placement(handler, ToolPlacement::HostOnly);
    }

    pub fn register_with_placement(&mut self, handler: ToolHandler, placement: ToolPlacement) {
        let key = handler.key.clone();
        self.handlers.insert(key.clone(), handler);
        self.placements.insert(key, placement);
    }

    pub fn lookup(&self, name: &str) -> Option<&ToolHandler> {
        self.handlers.get(name)
    }

    /// 运行时重设工具白名单（工具模式切换的入口）：空列表 = 全量（标准模式）。
    /// 与 `apply_init` 共享 known 过滤语义；**不**改 session（区别于 apply_init）。
    pub fn set_allowed(&mut self, allowed_tools: Vec<String>) {
        // 防御：剔除不在注册表中的工具名。工具冻结期间改名/删除后（如
        // read_file→read、edit_file_v2→edit、web→web_fetch、search 移除），
        // 旧配置的 allowlist 会指向不存在的工具——保留则执行期报
        // "Unknown tool"，剔除则按当前正式词汇表生效。全部无效时回退
        // 到"全部工具"（空 allowlist 语义），宁全开不瘫痪。
        let total = allowed_tools.len();
        let known: Vec<String> = allowed_tools
            .into_iter()
            .filter(|name| self.handlers.contains_key(name))
            .collect();
        if known.len() != total {
            log::warn!(
                "[TOOLS] allowlist filtered: {} of {total} tool names unknown (renamed/removed); kept: {}",
                total - known.len(),
                if known.is_empty() {
                    "<all tools>".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
        self.allowed = if known.is_empty() { None } else { Some(known) };
    }

    pub fn apply_init(&mut self, allowed_tools: Vec<String>, session_seed: &str) {
        self.set_allowed(allowed_tools);
        crate::set_current_session(session_seed);
    }

    pub fn all_defs(&self) -> Vec<qaqh_types::ToolDef> {
        self.handlers.values().map(|h| h.to_tool_def()).collect()
    }

    pub fn filtered_defs(&self) -> Vec<qaqh_types::ToolDef> {
        match &self.allowed {
            Some(allowed) => self
                .all_defs()
                .into_iter()
                .filter(|d| allowed.contains(&d.function.name))
                .collect(),
            None => self.all_defs(),
        }
    }

    // ── Three-phase execution for parallel tool support ──

    /// Phase 1: validate, safety-check, register inflight. Returns a [`PreparedCall`]
    /// that can be executed without the manager lock.
    pub(crate) fn prepare_req(
        &mut self,
        id: String,
        name: &str,
        action: &str,
        args: serde_json::Value,
        timeout_secs: Option<u64>,
        progress_tx: Option<crate::ExecProgressSender>,
    ) -> Result<PreparedCall, ToolExecReport> {
        if let Some(ref allowed) = self.allowed
            && !allowed.contains(&name.to_string()) {
                let msg = format!(
                    "[ERROR] Tool '{}' is not in the allowed list for this subagent. Allowed tools: [{}]",
                    name,
                    allowed.join(", ")
                );
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }

        let (handler, placement) = match self.handlers.get(name) {
            Some(handler) => (
                handler.clone(),
                self.placements.get(name).copied().unwrap_or_default(),
            ),
            None => {
                let msg = format!("[ERROR] Unknown tool: {}", name);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
        };

        let timeout_secs = timeout_secs.unwrap_or(handler.default_timeout.as_secs());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let skill_effects = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args: args.clone(),
            tx_progress: progress_tx.clone(),
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag.clone(),
            skill_effects: skill_effects.clone(),
        };
        let in_workspace = is_path_in_workspace(&ctx);
        match crate::safety::SafetyPolicy::evaluate(handler.risk.clone(), in_workspace) {
            SafetyVerdict::Block(reason) => {
                let msg = format!("[ERROR] {}", reason);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
            SafetyVerdict::Allow => {}
        }

        self.inflight_tasks.insert(id.clone(), cancel_flag.clone());

        let audit_args = args.clone();
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: progress_tx,
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag,
            skill_effects,
        };

        Ok(PreparedCall {
            id,
            name: name.to_string(),
            placement,
            handler_fn: handler.handler,
            ctx,
            audit_args,
        })
    }

    /// Phase 3: deregister inflight, accumulate stats, build report.
    pub(crate) fn finalize_req(
        &mut self,
        prepared: PreparedCall,
        result: crate::ToolResult,
        elapsed_ms: u64,
    ) -> ToolExecReport {
        self.inflight_tasks.remove(&prepared.id);

        let output_size = result.model_text().len();
        let success = result.is_success();

        self.stats_total += 1;
        if !success {
            self.stats_failures += 1;
        }
        let args_summary = audit_args_summary(&prepared.name, &prepared.audit_args);
        let files_affected = extract_files_affected(&prepared.name, &prepared.audit_args);
        if success {
            match prepared.name.as_str() {
                "read" | "skills" => {
                    for f in &files_affected {
                        if !self.files_read.contains(f) {
                            self.files_read.push(f.clone());
                        }
                    }
                }
                "edit" | "todo" => {
                    for f in &files_affected {
                        if !self.files_written.contains(f) {
                            self.files_written.push(f.clone());
                        }
                    }
                }
                "exec" | "git_commit" | "git_add" => {
                    // These mutate the workspace but don't have a single 'path' argument
                }
                _ => {}
            }
        }
        let meta = ToolExecMeta {
            name: prepared.name,
            elapsed_ms,
            output_size,
            success,
            args_summary,
        };
        ToolExecReport {
            success,
            content: result.model_text().to_string(),
            meta,
            files_affected,
        }
    }

    pub fn stats(&self) -> ToolStats {
        ToolStats {
            calls_total: self.stats_total,
            failures: self.stats_failures,
            files_read: self.files_read.clone(),
            files_written: self.files_written.clone(),
        }
    }

    pub fn cancel_tool(&mut self, id: Option<&str>) {
        match id {
            Some(specific) => {
                if let Some(flag) = self.inflight_tasks.get(specific) {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            None => {
                crate::set_cancel(true);
                for flag in self.inflight_tasks.values() {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

/// Extract file paths from tool args.
fn extract_files_affected(_tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut files = Vec::new();
    if let Some(v) = obj.get("path").and_then(|v| v.as_str()) {
        files.push(v.to_string());
    }
    if let Some(arr) = obj.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                files.push(s.to_string());
            }
        }
    }
    for key in ["file_a", "file_b", "dest", "target"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            files.push(v.to_string());
        }
    }
    files
}

/// Determine whether the tool call is operating within the current workspace.
fn is_path_in_workspace(ctx: &crate::ToolCallCtx) -> bool {
    if let Some(path) = ctx.args.get("path").and_then(|v| v.as_str()) {
        if path.is_empty() || path == "." {
            return true;
        }
        let ws = crate::current_workspace();
        if ws.is_empty() || ws == "." {
            return true;
        }
        let abs_path = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            std::path::Path::new(&ws)
                .join(path)
                .to_string_lossy()
                .to_string()
        };
        abs_path.starts_with(&ws)
    } else {
        // No path arg — assume workspace operation (e.g. task, skills, ask)
        true
    }
}

/// Compact args summary for audit log — path and key values only.
fn audit_args_summary(_tool: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    // Show path-like args first, then command, then truncate to 80 chars
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "path", "file_a", "file_b", "dest", "target", "command", "pattern", "query", "question",
    ] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            let short = if v.len() > 50 {
                // Show last path segment
                let seg = v.rsplit(&['/', '\\']).next().unwrap_or(v);
                format!("{key}=\"{seg}\"")
            } else {
                format!("{key}=\"{v}\"")
            };
            parts.push(short);
        }
    }
    let s = parts.join(", ");
    if s.len() > 80 {
        let end = s.floor_char_boundary(77);
        format!("{}…", &s[..end])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCallCtx, ToolResult, ToolRisk};

    fn noop(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok("noop")
    }

    fn handler(key: &str) -> ToolHandler {
        ToolHandler {
            key: key.to_string(),
            description: "test handler",
            input_schema: serde_json::json!({ "type": "object" }),
            handler: noop,
            risk: ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(10),
        }
    }

    fn names(mgr: &ToolManager) -> Vec<String> {
        let mut v: Vec<String> = mgr
            .filtered_defs()
            .into_iter()
            .map(|d| d.function.name)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn apply_init_filters_unknown_renamed_tool_names() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("read"));
        mgr.register(handler("exec"));
        // 冻结期间改名/删除的旧名（read/edit/web/search）必须被剔除。
        mgr.apply_init(
            vec![
                "read".to_string(),
                "edit".to_string(),
                "read".to_string(),
                "exec".to_string(),
            ],
            "s1",
        );
        assert_eq!(names(&mgr), vec!["exec", "read"]);
    }

    #[test]
    fn apply_init_all_unknown_falls_back_to_all_tools() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        // 旧配置全是已移除的工具名：回退到"全部工具"（空 allowlist 语义），
        // 而不是把子代理锁死成零工具。
        mgr.apply_init(vec!["read".to_string(), "edit".to_string()], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn apply_init_empty_stays_all_tools() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.apply_init(vec![], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    // ── set_allowed（4.1：工具模式运行时切换入口）──

    #[test]
    fn set_allowed_restricts_and_restores() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.register(handler("read"));
        mgr.register(handler("write"));
        // 切到极限白名单
        mgr.set_allowed(vec!["exec".to_string(), "read".to_string()]);
        assert_eq!(names(&mgr), vec!["exec", "read"]);
        // 切回全量（标准模式）
        mgr.set_allowed(vec![]);
        assert_eq!(names(&mgr), vec!["exec", "read", "write"]);
    }

    #[test]
    fn set_allowed_filters_unknown_names() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        // 未知名剔除（不静默吞掉，log warn）；全无效 → 全量
        mgr.set_allowed(vec!["exec".to_string(), "ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
        mgr.set_allowed(vec!["ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn set_allowed_does_not_touch_session() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.apply_init(vec![], "seed-A");
        // set_allowed 只改工具集，不动 session（区别于 apply_init）
        mgr.set_allowed(vec!["exec".to_string()]);
        assert_eq!(
            *crate::CURRENT_SESSION.lock().unwrap(),
            Some("seed-A".to_string())
        );
    }

    #[test]
    fn set_allowed_gates_prepare_req() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.register(handler("read"));
        mgr.set_allowed(vec!["read".to_string()]);
        // 白名单外工具在执行层被拦截（纵深防御）
        let err = mgr.prepare_req(
            "c1".to_string(),
            "exec",
            "exec",
            serde_json::json!({"argv": ["echo", "hi"]}),
            None,
            None,
        );
        assert!(err.is_err());
        let ok = mgr.prepare_req(
            "c2".to_string(),
            "read",
            "read",
            serde_json::json!({"path": "x"}),
            None,
            None,
        );
        assert!(ok.is_ok());
    }
}
