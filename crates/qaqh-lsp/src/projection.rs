//! LSP → 模型面投影（mcp projection.rs 同款职责：纯函数翻译 + 结果渲染）。
//!
//! - [`render_locations`]：`Location[]/LocationLink[]` → `path:line:col` 行式
//!   文本（1-based 回显，M1 决策 L2）；超长截断 2KB + 标记（mcp 同款）；头部
//!   `resultCount/fileCount` 计数行（Claude schema 同款）；
//! - [`render_hover`]：`Hover` → markdown/plain 文本直通；
//! - [`render_symbols`]：`DocumentSymbol[]/SymbolInformation[]` → 缩进树行式。

/// 描述截断上限（mcp `DYNAMIC_DESCRIPTION_LIMIT` 同款：防上下文膨胀）。
pub const RESULT_TRUNCATION_LIMIT: usize = 2048;

/// 描述截断标记（追加在被截断文本尾部；mcp 同款）。
const TRUNCATION_MARKER: &str = " [truncated]";

/// 按字符边界把文本截断到 [`RESULT_TRUNCATION_LIMIT`] 内，追加截断标记。
pub fn truncate_result(text: &str) -> String {
    if text.len() <= RESULT_TRUNCATION_LIMIT {
        return text.to_owned();
    }
    let budget = RESULT_TRUNCATION_LIMIT.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    match text.get(..end) {
        Some(head) => format!("{head}{TRUNCATION_MARKER}"),
        None => TRUNCATION_MARKER.to_owned(),
    }
}

/// file:// URI → 本地路径（失败回退原文；只处理 file scheme）。
pub fn uri_to_path(uri: &lsp_types::Url) -> String {
    if uri.scheme() == "file" {
        uri.to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| uri.as_str().to_owned())
    } else {
        uri.as_str().to_owned()
    }
}

/// 0-based LSP Position → 1-based 回显行。
pub fn display_line_col(pos: lsp_types::Position) -> (u32, u32) {
    (pos.line + 1, pos.character + 1)
}

/// Location → `path:line:col` 单行。
pub fn render_location(loc: &lsp_types::Location) -> String {
    let (line, col) = display_line_col(loc.range.start);
    format!("{}:{line}:{col}", uri_to_path(&loc.uri))
}

/// LocationLink → 取 target 侧渲染（definition 系回 LocationLink 时用）。
pub fn render_location_link(link: &lsp_types::LocationLink) -> String {
    let (line, col) = display_line_col(link.target_range.start);
    format!("{}:{line}:{col}", uri_to_path(&link.target_uri))
}

/// GotoDefinition 结果 → 行式文本（`Location` 与 `LocationLink` 双形态）。
pub fn render_goto_result(result: Option<lsp_types::GotoDefinitionResponse>) -> String {
    let lines: Vec<String> = match result {
        None => return "no definition found".to_owned(),
        Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![render_location(&loc)],
        Some(lsp_types::GotoDefinitionResponse::Array(locs)) if locs.is_empty() => {
            return "no definition found".to_owned();
        }
        Some(lsp_types::GotoDefinitionResponse::Array(locs)) => {
            locs.iter().map(render_location).collect()
        }
        Some(lsp_types::GotoDefinitionResponse::Link(links)) if links.is_empty() => {
            return "no definition found".to_owned();
        }
        Some(lsp_types::GotoDefinitionResponse::Link(links)) => {
            links.iter().map(render_location_link).collect()
        }
    };
    render_counted_lines(&lines)
}

/// references 结果 → 行式文本（含空结果提示）。
pub fn render_references(locations: Option<Vec<lsp_types::Location>>) -> String {
    let locations = locations.unwrap_or_default();
    if locations.is_empty() {
        return "no references found".to_owned();
    }
    let lines: Vec<String> = locations.iter().map(render_location).collect();
    render_counted_lines(&lines)
}

/// hover 结果 → 文本直通（MarkupContent value / MarkedString / LanguageString）。
pub fn render_hover(hover: Option<lsp_types::Hover>) -> String {
    let Some(hover) = hover else {
        return "no hover information".to_owned();
    };
    match hover.contents {
        lsp_types::HoverContents::Scalar(marked) => render_marked_string(&marked),
        lsp_types::HoverContents::Array(marked) => {
            let parts: Vec<String> = marked.iter().map(render_marked_string).collect();
            parts.join("\n---\n")
        }
        lsp_types::HoverContents::Markup(content) => content.value,
    }
}

fn render_marked_string(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            if ls.language.is_empty() {
                ls.value.clone()
            } else {
                format!("```{}\n{}\n```", ls.language, ls.value)
            }
        }
    }
}

/// documentSymbol 结果 → 缩进树行式（`kind name [selection path:line]`）。
pub fn render_document_symbols(
    result: Option<lsp_types::DocumentSymbolResponse>,
) -> String {
    let Some(result) = result else {
        return "no document symbols".to_owned();
    };
    let mut lines = Vec::new();
    match result {
        lsp_types::DocumentSymbolResponse::Flat(infos) => {
            if infos.is_empty() {
                return "no document symbols".to_owned();
            }
            for info in &infos {
                let (line, _) = display_line_col(info.location.range.start);
                lines.push(format!(
                    "{:?} {} @{}:{line}",
                    info.kind,
                    info.name,
                    uri_to_path(&info.location.uri)
                ));
            }
        }
        lsp_types::DocumentSymbolResponse::Nested(symbols) => {
            if symbols.is_empty() {
                return "no document symbols".to_owned();
            }
            render_symbol_tree(&symbols, 0, &mut lines);
        }
    }
    render_counted_lines(&lines)
}

fn render_symbol_tree(symbols: &[lsp_types::DocumentSymbol], depth: usize, out: &mut Vec<String>) {
    for sym in symbols {
        let (line, col) = display_line_col(sym.selection_range.start);
        out.push(format!(
            "{}{:?} {} :{line}:{col}",
            "  ".repeat(depth),
            sym.kind,
            sym.name
        ));
        if let Some(children) = &sym.children {
            render_symbol_tree(children, depth + 1, out);
        }
    }
}

/// workspaceSymbol 结果 → 行式（`kind name path:line:col (container)?`）。
///
/// 双形态：Flat（`SymbolInformation[]`，location 必含 range）与 Nested
/// （`WorkspaceSymbol[]`，location 可能是无 range 的 `WorkspaceLocation`——
/// 此时只渲染 uri，不带行列）。
pub fn render_workspace_symbols(
    symbols: Option<lsp_types::WorkspaceSymbolResponse>,
) -> String {
    let Some(symbols) = symbols else {
        return "no workspace symbols".to_owned();
    };
    let lines: Vec<String> = match symbols {
        lsp_types::WorkspaceSymbolResponse::Flat(infos) => infos
            .iter()
            .map(|s| {
                let (line, col) = display_line_col(s.location.range.start);
                let container = s
                    .container_name
                    .as_ref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                format!(
                    "{:?} {} {}:{line}:{col}{container}",
                    s.kind,
                    s.name,
                    uri_to_path(&s.location.uri),
                )
            })
            .collect(),
        lsp_types::WorkspaceSymbolResponse::Nested(symbols) => symbols
            .iter()
            .map(|s| {
                let container = s
                    .container_name
                    .as_ref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                match &s.location {
                    lsp_types::OneOf::Left(loc) => {
                        let (line, col) = display_line_col(loc.range.start);
                        format!(
                            "{:?} {} {}:{line}:{col}{container}",
                            s.kind,
                            s.name,
                            uri_to_path(&loc.uri),
                        )
                    }
                    lsp_types::OneOf::Right(wloc) => format!(
                        "{:?} {} {}{container}",
                        s.kind,
                        s.name,
                        uri_to_path(&wloc.uri),
                    ),
                }
            })
            .collect(),
    };
    if lines.is_empty() {
        return "no workspace symbols".to_owned();
    }
    render_counted_lines(&lines)
}

/// 计数头行 + 行式 + 2KB 截断（Claude 计数位 + mcp 截断同款）。
fn render_counted_lines(lines: &[String]) -> String {
    let files: std::collections::BTreeSet<&str> = lines
        .iter()
        .filter_map(|l| l.split(':').next())
        .collect();
    let header = format!("resultCount={} fileCount={}", lines.len(), files.len());
    truncate_result(&format!("{header}\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(path: &str, line: u32, col: u32) -> lsp_types::Location {
        lsp_types::Location {
            uri: lsp_types::Url::from_file_path(path).expect("file uri"),
            range: lsp_types::Range {
                start: lsp_types::Position::new(line, col),
                end: lsp_types::Position::new(line, col + 1),
            },
        }
    }

    #[test]
    fn display_is_one_based() {
        let rendered = render_location(&loc("/tmp/a.rs", 0, 4));
        assert_eq!(rendered, "/tmp/a.rs:1:5", "内部 0-based → 模型面 1-based");
    }

    #[test]
    fn goto_empty_reports_no_definition() {
        let out = render_goto_result(Some(lsp_types::GotoDefinitionResponse::Array(vec![])));
        assert_eq!(out, "no definition found");
        let out = render_goto_result(None);
        assert_eq!(out, "no definition found");
    }

    #[test]
    fn goto_scalar_renders_count_header() {
        let out = render_goto_result(Some(lsp_types::GotoDefinitionResponse::Scalar(loc(
            "/tmp/a.rs", 9, 0,
        ))));
        assert!(out.starts_with("resultCount=1 fileCount=1\n"), "{out}");
        assert!(out.contains("/tmp/a.rs:10:1"), "{out}");
    }

    #[test]
    fn references_empty_reports_no_references() {
        assert_eq!(render_references(None), "no references found");
        assert_eq!(render_references(Some(vec![])), "no references found");
    }

    #[test]
    fn hover_none_reports_empty() {
        assert_eq!(render_hover(None), "no hover information");
    }

    #[test]
    fn truncate_keeps_char_boundary_and_marker() {
        let long = "中".repeat(3000);
        let out = truncate_result(&long);
        assert!(out.len() <= RESULT_TRUNCATION_LIMIT);
        assert!(out.ends_with(" [truncated]"));
    }

    #[test]
    fn uri_non_file_passthrough() {
        let uri: lsp_types::Url = "untitled:Untitled-1".parse().expect("parse uri");
        assert_eq!(uri_to_path(&uri), "untitled:Untitled-1");
    }
}
