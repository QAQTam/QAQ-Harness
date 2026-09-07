//! hunk — split from the v2 edit core

use crate::file_shared::normalize_newlines;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hunk {
    /// 替换；old 为空字符串 = 纯插入（context_before/after 至少一侧非空）。
    /// `replace_all=true` 时替换 old 的全部**精确**匹配位置（仅 Tier1 生效，
    /// 不降级模糊匹配——模糊应用于多位置风险不可控）。
    Replace {
        old: String,
        new: String,
        context_before: String,
        context_after: String,
        replace_all: bool,
        /// 宽松行号提示（1-based，±10 窗口）：四层定位全失败后，仅在
        /// [hint-10, hint+10] 内重试 Tier1 精确匹配，唯一命中才应用。
        /// 不触碰默认路径（未提供时行为与原来完全一致）。
        hint_line: Option<usize>,
    },
    PrependFile {
        new: String,
    },
    AppendFile {
        new: String,
    },
}

impl Hunk {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Hunk::Replace { .. } => "replace",
            Hunk::PrependFile { .. } => "prepend_file",
            Hunk::AppendFile { .. } => "append_file",
        }
    }

    /// 解析单个 hunk。文本字段统一做 CRLF → LF 归一化（LF 规范视图契约），
    /// 检测到 CR 时向 `notes` 追加说明。
    pub(crate) fn parse(v: &Value, notes: &mut Vec<String>) -> Result<Hunk, String> {
        let norm = |s: &str, notes: &mut Vec<String>| -> String {
            if s.contains('\r') {
                notes.push("CRLF in request normalized to LF".to_string());
                normalize_newlines(s).0
            } else {
                s.to_string()
            }
        };
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "missing 'kind'".to_string())?;
        match kind {
            "replace" => {
                let old = v
                    .get("old")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace hunk requires 'old'".to_string())?;
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace hunk requires 'new'".to_string())?;
                let context_before = v
                    .get("context_before")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let context_after = v
                    .get("context_after")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let replace_all = v
                    .get("replace_all")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                Ok(Hunk::Replace {
                    old: norm(old, notes),
                    new: norm(new, notes),
                    context_before: norm(context_before, notes),
                    context_after: norm(context_after, notes),
                    replace_all,
                    hint_line: parse_hint_line(v),
                })
            }
            "prepend_file" | "append_file" => {
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("{kind} hunk requires 'new'"))?;
                let new = norm(new, notes);
                if kind == "prepend_file" {
                    Ok(Hunk::PrependFile { new })
                } else {
                    Ok(Hunk::AppendFile { new })
                }
            }
            other => Err(format!(
                "unknown hunk kind '{other}' (expected replace / prepend_file / append_file)"
            )),
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 文件视图（LF 规范视图）

pub(crate) fn parse_hint_line(v: &Value) -> Option<usize> {
    v.get("hint_line")
        .and_then(|x| x.as_u64())
        .map(|x| x.max(1) as usize)
}
