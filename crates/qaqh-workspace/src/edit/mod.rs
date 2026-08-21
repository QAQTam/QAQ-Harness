#![allow(unused_imports)]
//! edit — 差分实现的第二代编辑工具。
//!
//! 定位（见 docs/nextdev/PLAN-EDIT-FILE-V2.md）：**只实现 v1 没有的部分**。
//! v1（edit_file：行号定位、replace_all、regex、每 op 独立事务与诊断体系）
//! 已下线删除——v2 是唯一编辑入口。
//!
//! v2 的能力面（对齐 edit-tool-design-spec.md）：
//!
//! - 结构化 hunk 协议：`replace` / `insert_after` / `insert_before` /
//!   `prepend_file` / `append_file`，字段名（old/new/anchor）与 v1 刻意区分。
//! - 四层匹配流水线：Tier1 精确（context 全等消歧）→ Tier2 缩进形状 →
//!   Tier3 相似度评分（0.6/0.2/0.2 加权 + 阈值 0.85 + margin 0.10 自动采纳）→
//!   Tier4 拒绝并返回 Top3 候选。
//! - 单快照两阶段全事务：全部 hunk 在同一份未修改快照上定位，任一失败整体拒绝；
//!   区间重叠 → OVERLAPPING_HUNKS；应用按位置倒序走 ropey remove/insert。
//! - 显式 `expected_hash` 乐观锁；失配返回 current_hash + current_content（截断）。
//! - 成功返回 new_hash（sha256，与 read 协议一致），可续接下一次调用。
//!
//! 复用（基础设施白名单，非 v1 能力）：`file_shared::{content_hash,
//! normalize_newlines, atomic_write, unified_diff}`。CRLF 契约与 v1 相同：
//! LF 规范视图上匹配与算 hash，写回按 was_crlf 还原。

use crate::file_shared::{content_hash, normalize_newlines};
use crate::{ToolHandler, ToolManager, ToolPlacement, ToolResult, ToolRisk};

// ─────────────────────────────────────────────────────────────
// 配置常量
// ─────────────────────────────────────────────────────────────

/// Tier3 相似度采纳阈值
pub(crate) const T3_THRESHOLD: f32 = 0.85;
/// Tier3 胜出边际
pub(crate) const T3_MARGIN: f32 = 0.10;
/// 单次调用 hunk 数上限。
pub(crate) const MAX_HUNKS: usize = 64;

/// 读取相关上限（复用 file_shared 单点上限）
pub(crate) use crate::file_shared::{CANDIDATE_MAX, CONTENT_CAP, READ_MAX_CHARS, READ_MAX_CONTEXT, READ_MAX_LINES, SNIPPET_MAX};
/// hint_line 兜底窗口
pub(crate) const HINT_WINDOW: usize = 10;

// ─────────────────────────────────────────────────────────────
// 子模块
// ─────────────────────────────────────────────────────────────

pub mod hunk;
pub mod view;
pub mod matching;
pub mod locate;
pub mod resolve;
pub mod transaction;
pub mod read;
pub mod handler;

// 核心重导出（保持 crate::edit::* 兼容 file_edit_v2 的旧导入）
#[allow(unused_imports)]
pub(crate) use hunk::Hunk;
pub(crate) use view::FileView;
pub(crate) use matching::{Candidate, Located, LocateError, Tier3Probe};
pub use handler::register;
pub(crate) use handler::handle_edit;

// 为了让 file_edit_v2 shim 的 `pub use crate::edit::*;` 能继续暴露 register 等
// 也把常用函数重导出
pub(crate) use locate::{locate_anchor, locate_hunk, locate_pure_insert, locate_replace, locate_replace_all};
pub(crate) use locate::{locate_with_hint, tier1_in_window};
pub(crate) use resolve::{apply_ops, check_overlap, ranges_overlap, resolve, ResolvedOp};
pub(crate) use transaction::{FileOutcome, HunkReport, Mode, run_edit, render_text, truncate_content, hint_for, render_hunk_error, hunk_ok_line};
pub(crate) use handler::hunk_report_json;
pub(crate) use read::{candidate_json, read_anchored, read_path, read_range, render_read_candidates};
pub use handler::exec_edit;

#[cfg(test)]
mod tests;
