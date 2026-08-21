//! 工具侧折叠（tool-side folding）——策略接口化。
//!
//! 折叠/截断在工具执行层完成：`execution.rs::execute_authorized` 在结果回传
//! LLM 之前统一调用 [`apply`]；`qaqh-message` 的 message 层不再改写工具结果
//! （原 message 侧折叠已取消）。
//!
//! 折叠策略通过 [`ToolResultFoldPolicy`] 接口注入，运行时全局切换：
//! - [`StandardPolicy`]（默认）：统一折叠/截断（见 [`StandardPolicy::limit_for`]）；
//! - [`NoFoldPolicy`]（极限模式）：完全透传，连 exec/bash 内部的 token 截断
//!   也关闭——上下文大小完全由模型自己控制。
//!
//! 原则：模型必须“看得到、看得清”。
//! - 清单类 / 自限工具（`glob` / `grep` / `read` / `skills` / 收据类…）结果
//!   **原样透传**——产物就是模型要继续工作的输入，折叠成首行会剪断反馈闭环；
//! - 大内容工具（`exec` / `bash` / `pwsh` / `web_fetch` / `image` / `process`）
//!   保留**头部内容 + 明确截断标记**，不再折叠成“首行 + [details folded]”这类
//!   无信息形态（StandardPolicy 行为；NoFoldPolicy 下全部透传）；
//! - 失败/部分结果一律透传——模型需要失败原因、候选与详情才能修正重试。
//!
//! 工具内已有的“资源保护”不属于折叠策略，保持各工具自身语义：
//! `read_stream` 字节上限、`web_fetch` body 上限、`grep`/`glob` 的 `max_results`
//! 熔断等——它们防止内存/IO 爆炸，与“给模型看多少”无关。
//!
//! `read` / `edit` 按约定暂不在此施加策略（更名完成后随工具一起优化），
//! StandardPolicy 下与其它透传工具一样原样返回。

use std::sync::{Arc, LazyLock, RwLock};

use qaqh_types::ToolResult;

/// 命令输出（exec/bash/pwsh）的默认字符上限（StandardPolicy）。
const EXEC_CHAR_LIMIT: usize = 8_000;
/// 大内容工具（网络/图像/进程输出）的默认字符上限。
const CONTENT_BEARING_CHAR_LIMIT: usize = 16_000;
/// exec 内部 token 截断的默认上限（StandardPolicy；模型可显式传参覆盖）。
const EXEC_DEFAULT_MAX_OUTPUT_TOKENS: u32 = 10_000;

/// 工具结果折叠策略。
///
/// 实现方决定：哪些工具截断（`limit_for`）、命令输出内部 token 上限
/// （`exec_max_output_tokens`）以及截断标记文案（`truncation_marker`，可覆盖）。
pub trait ToolResultFoldPolicy: Send + Sync + std::fmt::Debug {
    /// 该工具结果模型可见文本的字符上限；`None` = 完全透传（不截断）。
    fn limit_for(&self, tool_name: &str) -> Option<usize>;

    /// exec/bash/pwsh 内部 token 截断上限；`None` = 不截断（极限模式）。
    /// 模型显式传入 `max_output_tokens` 参数时以模型参数为准。
    fn exec_max_output_tokens(&self) -> Option<u32>;

    /// 截断标记文案（默认实现足够通用，策略可按需覆盖）。
    fn truncation_marker(&self, tool_name: &str, total_chars: usize) -> String {
        format!(
            "[truncated: {total_chars} chars — call {tool_name} again with narrower args/filters to see more]"
        )
    }
}

/// 标准策略：统一折叠/截断（默认）。
#[derive(Debug, Default)]
pub struct StandardPolicy;

impl ToolResultFoldPolicy for StandardPolicy {
    fn limit_for(&self, tool_name: &str) -> Option<usize> {
        match tool_name {
            // 透传白名单：清单 / 收据 / 激活说明——模型必须看到全文
            // （read/edit 更名后再随工具优化，此处同样透传）
            "apply_patch" | "ask" | "confirm_apply" | "copy_range" | "delete" | "edit" | "glob"
            | "grep" | "read" | "skills" | "todo" | "write" => None,
            // 命令输出
            "bash" | "exec" | "pwsh" => Some(EXEC_CHAR_LIMIT),
            // 大内容
            "image" | "web_fetch" | "process" => Some(CONTENT_BEARING_CHAR_LIMIT),
            // 未知工具：透传（ToolResult 构造时默认 24K 硬顶兜底）
            _ => None,
        }
    }

    fn exec_max_output_tokens(&self) -> Option<u32> {
        Some(EXEC_DEFAULT_MAX_OUTPUT_TOKENS)
    }
}

/// 极限模式策略：完全不折叠任何工具结果。
///
/// 模型自己控制上下文——exec/bash 输出全量透传（连 24K 字符硬顶也放开，
/// 仅保留工具内部的 IO 资源保护如 `read_stream` 字节上限）。
#[derive(Debug, Default)]
pub struct NoFoldPolicy;

impl ToolResultFoldPolicy for NoFoldPolicy {
    fn limit_for(&self, _tool_name: &str) -> Option<usize> {
        None
    }

    fn exec_max_output_tokens(&self) -> Option<u32> {
        None
    }
}

/// 当前生效的全局折叠策略（默认 StandardPolicy）。
static CURRENT_POLICY: LazyLock<RwLock<Arc<dyn ToolResultFoldPolicy>>> =
    LazyLock::new(|| RwLock::new(Arc::new(StandardPolicy)));

/// 切换全局折叠策略（如进入/退出极限模式）。
pub fn set_policy(policy: Arc<dyn ToolResultFoldPolicy>) {
    let mut guard = CURRENT_POLICY.write().unwrap_or_else(|e| e.into_inner());
    *guard = policy;
}

/// 读取当前全局折叠策略。
pub fn policy() -> Arc<dyn ToolResultFoldPolicy> {
    CURRENT_POLICY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 对工具结果应用当前策略。仅在成功/后台结果上截断；错误/部分结果原样透传。
pub fn apply(tool_name: &str, result: &mut ToolResult) {
    if result.status.is_failure() {
        return; // 失败/部分：模型必须看到完整原因与候选
    }
    let policy = policy();
    let Some(limit) = policy.limit_for(tool_name) else {
        return; // 透传：工具自身自限 / 24K 硬顶 / 极限模式全透传
    };
    let text = &result.model.text;
    if text.chars().count() <= limit {
        return;
    }
    result.model.text = truncate_with_marker(&*policy, tool_name, text, limit);
    result.model.truncated = true;
}

/// 保留头部（按行对齐到 limit 内最后一个换行），追加策略的截断标记。
fn truncate_with_marker(
    policy: &dyn ToolResultFoldPolicy,
    tool_name: &str,
    text: &str,
    limit: usize,
) -> String {
    let cut = text.floor_char_boundary(limit);
    // 行对齐：宁可少给一点，也不让模型看到半行代码/半条日志。
    let cut = text[..cut].rfind('\n').map(|n| n + 1).unwrap_or(cut);
    let total = text.chars().count();
    format!(
        "{}…\n{}",
        &text[..cut],
        policy.truncation_marker(tool_name, total)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::{ToolResult, ToolStatus};

    fn ok(text: &str) -> ToolResult {
        ToolResult::ok(text)
    }

    #[test]
    fn standard_policy_limits_match_documented_contract() {
        let p = StandardPolicy;
        assert_eq!(p.limit_for("grep"), None);
        assert_eq!(p.limit_for("read"), None);
        assert_eq!(p.limit_for("edit"), None);
        assert_eq!(p.limit_for("skills"), None);
        assert_eq!(p.limit_for("exec"), Some(EXEC_CHAR_LIMIT));
        assert_eq!(p.limit_for("bash"), Some(EXEC_CHAR_LIMIT));
        assert_eq!(p.limit_for("pwsh"), Some(EXEC_CHAR_LIMIT));
        assert_eq!(p.limit_for("web_fetch"), Some(CONTENT_BEARING_CHAR_LIMIT));
        assert_eq!(p.limit_for("image"), Some(CONTENT_BEARING_CHAR_LIMIT));
        assert_eq!(p.limit_for("unknown_tool"), None);
        assert_eq!(p.exec_max_output_tokens(), Some(10_000));
    }

    #[test]
    fn no_fold_policy_passes_everything_through() {
        let p = NoFoldPolicy;
        for name in ["exec", "bash", "pwsh", "web_fetch", "grep", "read", "edit"] {
            assert_eq!(p.limit_for(name), None, "{name} must not be limited");
        }
        assert_eq!(p.exec_max_output_tokens(), None);
    }

    #[test]
    fn grep_result_passes_through_verbatim() {
        // grep 的产物就是 path:line:content 清单——折叠会让模型拿不到清单。
        let listing = "src/a.rs:10:fn alpha() {}\nsrc/b.rs:20:fn beta() {}\n";
        let mut result = ok(listing);
        apply("grep", &mut result);
        assert_eq!(result.model.text, listing);
        assert!(!result.model.truncated);
    }

    #[test]
    fn read_and_edit_pass_through() {
        // 更名前约定：read/edit 不在工具侧施加策略。
        let read_body = "L1: line one\nL2: line two\n";
        let mut result = ok(read_body);
        apply("read", &mut result);
        assert_eq!(result.model.text, read_body);

        let edit_receipt = "[OK] edit src/lib.rs\n  2/2 hunks applied (new_hash a1b2c3d4)\n";
        let mut result = ok(edit_receipt);
        apply("edit", &mut result);
        assert_eq!(result.model.text, edit_receipt);
    }

    #[test]
    fn skills_and_glob_and_todo_pass_through() {
        for (name, body) in [
            ("skills", "[QAQH_SKILL_V1]\ninstructions…\n"),
            ("glob", "src/a.rs\nsrc/b.rs\n"),
            ("todo", "[OK] todo\n  2 items\n"),
        ] {
            let mut result = ok(body);
            apply(name, &mut result);
            assert_eq!(result.model.text, body, "{name} must pass through");
        }
    }

    #[test]
    fn exec_result_is_truncated_with_head_and_marker() {
        let body = "line of output\n".repeat(2_000); // ~34K chars > 8K cap
        let mut result = ok(&body);
        apply("exec", &mut result);
        assert!(result.model.truncated);
        assert!(result.model.text.len() < body.len());
        assert!(result.model.text.starts_with("line of output\n"));
        assert!(result.model.text.contains("[truncated:"));
        assert!(result.model.text.contains("call exec again"));
        // 行对齐：头部到截断标记之间没有半行。
        assert!(result.model.text.contains("…\n[truncated:"));
    }

    #[test]
    fn bash_and_pwsh_are_truncated_like_exec() {
        for name in ["bash", "pwsh"] {
            let body = "x\n".repeat(6_000); // 12K chars > 8K cap
            let mut result = ok(&body);
            apply(name, &mut result);
            assert!(result.model.truncated, "{name} must be truncated");
            assert!(result.model.text.contains("[truncated:"));
        }
    }

    #[test]
    fn short_exec_result_passes_through() {
        let body = "done in 12ms\n";
        let mut result = ok(body);
        apply("exec", &mut result);
        assert_eq!(result.model.text, body);
        assert!(!result.model.truncated);
    }

    #[test]
    fn web_fetch_and_image_use_content_bearing_limit() {
        for name in ["web_fetch", "image"] {
            let body = "content\n".repeat(5_000); // 40K chars > 16K cap
            let mut result = ok(&body);
            apply(name, &mut result);
            assert!(result.model.truncated, "{name} must be truncated");
            assert!(result.model.text.len() <= 16_000 + 128);
        }
    }

    #[test]
    fn failure_result_always_passes_through() {
        // 失败/部分结果不截断：模型需要完整错误原因、候选与详情。
        let error = "[ERROR] NO_MATCH\n  detail: old matches nothing\n  candidates: [L12, L40]\n"
            .repeat(200); // ~19K chars < qaqh-types 24K 硬顶
        let mut result =
            ToolResult::error_with("NO_MATCH", error.clone(), true, Some("refine old".into()));
        apply("exec", &mut result);
        assert_eq!(result.model.text, error);

        let mut partial = ToolResult::partial("x\n".repeat(5_000));
        apply("web_fetch", &mut partial);
        assert_eq!(partial.status, ToolStatus::Partial);
        assert_eq!(partial.model.text, "x\n".repeat(5_000));
    }

    #[test]
    fn unknown_tool_passes_through() {
        let body = "custom tool output\n";
        let mut result = ok(body);
        apply("custom_tool", &mut result);
        assert_eq!(result.model.text, body);
    }
}
