//! hunk — split from file_edit_v2.rs

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
    /// 整文件覆盖（等价 write 的 content 语义）：不要求 `old`，恒成功；
    /// **独占**——必须单独调用，与其它 hunk 混用报 OVERWRITE_EXCLUSIVE。
    Overwrite {
        new: String,
    },
    InsertAfter {
        anchor: String,
        new: String,
        hint_line: Option<usize>,
    },
    InsertBefore {
        anchor: String,
        new: String,
        hint_line: Option<usize>,
    },
    PrependFile {
        new: String,
    },
    AppendFile {
        new: String,
    },
    /// 行内替换（sed `s///` 语义）：anchor 定位行窗口后，**仅在窗口内**做子串/
    /// 正则替换——不跨行、不改行结构。`replace_all=false` 只替换第一处（按行序）；
    /// `regex=true` 时 `old` 为正则（regex crate 语法，大小写敏感）。
    ReplaceInline {
        anchor: String,
        old: String,
        new: String,
        replace_all: bool,
        regex: bool,
        hint_line: Option<usize>,
    },
}

impl Hunk {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Hunk::Replace { .. } => "replace",
            Hunk::Overwrite { .. } => "overwrite",
            Hunk::InsertAfter { .. } => "insert_after",
            Hunk::InsertBefore { .. } => "insert_before",
            Hunk::PrependFile { .. } => "prepend_file",
            Hunk::AppendFile { .. } => "append_file",
            Hunk::ReplaceInline { .. } => "replace_inline",
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
            "overwrite" => {
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "overwrite hunk requires 'new'".to_string())?;
                // 整文件语义：忽略可能误传的 old/context（write 语义不看旧内容）。
                Ok(Hunk::Overwrite {
                    new: norm(new, notes),
                })
            }
            "insert_after" | "insert_before" => {
                let anchor = v
                    .get("anchor")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("{kind} hunk requires non-empty 'anchor'"))?;
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("{kind} hunk requires 'new'"))?;
                let (anchor, new) = (norm(anchor, notes), norm(new, notes));
                if kind == "insert_after" {
                    Ok(Hunk::InsertAfter {
                        anchor,
                        new,
                        hint_line: parse_hint_line(v),
                    })
                } else {
                    Ok(Hunk::InsertBefore {
                        anchor,
                        new,
                        hint_line: parse_hint_line(v),
                    })
                }
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
            "replace_inline" => {
                let anchor = v
                    .get("anchor")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "replace_inline hunk requires non-empty 'anchor'".to_string())?;
                let old = v
                    .get("old")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "replace_inline hunk requires non-empty 'old'".to_string())?;
                if old.contains('\n') {
                    return Err(
                        "replace_inline 'old' must be a single-line substring (use 'replace' for line-level edits)"
                            .to_string(),
                    );
                }
                let new = v
                    .get("new")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "replace_inline hunk requires 'new'".to_string())?;
                let replace_all = v
                    .get("replace_all")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let regex = v.get("regex").and_then(|x| x.as_bool()).unwrap_or(false);
                Ok(Hunk::ReplaceInline {
                    anchor: norm(anchor, notes),
                    old: norm(old, notes),
                    new: norm(new, notes),
                    replace_all,
                    regex,
                    hint_line: parse_hint_line(v),
                })
            }
            other => Err(format!(
                "unknown hunk kind '{other}' (expected replace / overwrite / insert_after / insert_before / prepend_file / append_file / replace_inline)"
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
