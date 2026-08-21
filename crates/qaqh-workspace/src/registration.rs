//! ToolManager 初始化构造器。
//!
//! 各模块的 `register()` 在此组装。外部注册器通过 `extra_registrars` 注入。

use super::ToolManager;
use super::exec;
use super::image_query;
use super::journal;
use super::web;

use super::apply_patch;
use super::confirm_apply;
use super::copy_range;
use super::file_edit_v2;
use super::file_glob;
use super::file_mutate;
use super::file_query;
use super::grep_tool;

use super::ask_user;
use super::process_inspect;
use super::todo;

use super::skill;

/// 工具注册器函数签名。
pub type ToolRegistrar = fn(&mut ToolManager);

/// 构造并注册全部工具 handler，返回初始化后的 ToolManager。
/// `extra_registrars` 允许外部 crate（如 qaqh-subagent）注入工具。
pub fn build_tool_manager(extra_registrars: &[ToolRegistrar]) -> ToolManager {
    let mut mgr = ToolManager::new();

    // ── 系统工具 ──
    exec::register(&mut mgr);
    web::register(&mut mgr);

    // ── 文件操作 ──
    file_edit_v2::register(&mut mgr);
    file_mutate::register(&mut mgr);
    file_query::register(&mut mgr);
    file_glob::register(&mut mgr);
    apply_patch::register(&mut mgr);
    copy_range::register(&mut mgr);
    grep_tool::register(&mut mgr);

    // ── dry-run 确认（内存直提）──
    confirm_apply::register(&mut mgr);

    // ── Todo（直接、会话内状态工具）──
    todo::register(&mut mgr);

    // ── 交互 ──
    ask_user::register(&mut mgr);

    // ── 多模态图像理解 ──
    image_query::register(&mut mgr);

    journal::register(&mut mgr);

    // ── 进程巡查 ──
    process_inspect::register(&mut mgr);

    // ── Agent Skills ──
    skill::register(&mut mgr);

    // ── 外部注册器 ──
    for reg in extra_registrars {
        reg(&mut mgr);
    }

    mgr
}

#[cfg(test)]
mod tests {
    use super::build_tool_manager;

    #[test]
    fn default_registry_exposes_the_formal_tool_vocabulary() {
        let names: Vec<String> = build_tool_manager(&[])
            .all_defs()
            .into_iter()
            .map(|def| def.function.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "apply_patch",
                "ask",
                "bash",
                "confirm_apply",
                "copy_range",
                "delete",
                "edit",
                "exec",
                "glob",
                "grep",
                "image",
                "journal",
                "process",
                "pwsh",
                "read",
                "skills",
                "todo",
                "web_fetch",
                "write",
            ]
        );
    }
}
