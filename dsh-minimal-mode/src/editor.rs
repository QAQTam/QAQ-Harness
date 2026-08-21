//! `str_replace_editor` — faithful replication of the deepseek-harness
//! minimal-mode editor tool.
//!
//! Commands: `view`, `create`, `str_replace`, `insert`.
//! All model-facing returns (including error messages) are reproduced
//! verbatim from the minimal-mode extraction.

use qaqh_types::ToolResult;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolPlacement, ToolRisk};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_workspace::permission::ToolCategory;

const TRUNCATED_MESSAGE: &str = "<response clipped><NOTE>To save on context only part of this file has been shown to you. You should retry this tool after you have searched inside the file with `grep -n` in order to find the line numbers of what you are looking for.</NOTE>";

const DEFAULT_DESCRIPTION: &str = "Custom editing tool for viewing, creating and editing files\n* State is persistent across command calls and discussions with the user\n* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep\n* The `create` command cannot be used if the specified `path` already exists as a file\n* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`\n\nNotes for using the `str_replace` command:\n* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!\n* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique\n* The `new_str` parameter should contain the edited lines that should replace the `old_str`";

const DEFAULT_MAX_OUTPUT_CHARS: usize = 16_000;

/// A lightweight stable identity for a file's content version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    len: u64,
    modified_ns: u128,
}

impl Version {
    fn of(path: &Path) -> Option<Version> {
        let meta = std::fs::metadata(path).ok()?;
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Some(Version {
            len: meta.len(),
            modified_ns,
        })
    }
}

fn maybe_truncate(content: &str, max_output_chars: usize) -> String {
    if content.chars().count() <= max_output_chars {
        content.to_string()
    } else {
        let mut text: String = content.chars().take(max_output_chars).collect();
        text.push_str(TRUNCATED_MESSAGE);
        text
    }
}

/// Lexicographic codepoint comparison (mirrors JS `<` / `>` string ordering).
fn codepoint_compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.cmp(right)
}

/// All non-overlapping byte offsets where `search` appears in `content`.
fn match_offsets(content: &str, search: &str) -> Vec<usize> {
    content
        .match_indices(search)
        .map(|(offset, _)| offset)
        .collect()
}

/// 1-based line number for each byte offset (counting `\n` before it).
fn line_numbers_at(content: &str, offsets: &[usize]) -> Vec<usize> {
    offsets
        .iter()
        .map(|&offset| content[..offset].chars().filter(|&c| c == '\n').count() + 1)
        .collect()
}

fn required_for_command(
    value: Option<&str>,
    parameter: &str,
    command: &str,
    allow_empty: bool,
) -> Result<String, String> {
    match value {
        None => Err(format!(
            "Parameter `{parameter}` is required for command: {command}"
        )),
        Some(v) => {
            if !allow_empty && v.is_empty() {
                Err(format!(
                    "Parameter `{parameter}` is empty for command: {command}"
                ))
            } else {
                Ok(v.to_string())
            }
        }
    }
}

fn resolve_target(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path must be a non-empty string".to_string());
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!(
            "The path {path} is not an absolute path, it should start with `/`. Maybe you meant /{path}?"
        ));
    }
    // Normalise redundant `.` components while keeping an absolute path.
    let normalized: PathBuf = p.components().collect();
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsType {
    File,
    Directory,
    Other,
}

impl FsType {
    fn from_meta(meta: &std::fs::Metadata) -> FsType {
        if meta.is_file() {
            FsType::File
        } else if meta.is_dir() {
            FsType::Directory
        } else {
            FsType::Other
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FsInfo {
    fs_type: FsType,
    version: Option<Version>,
}

fn stat(path: &Path) -> Option<FsInfo> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FsInfo {
        fs_type: FsType::from_meta(&meta),
        version: Version::of(path),
    })
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `statExisting`: returns the info or the exact minimal-mode error message.
fn stat_existing(path: &Path, command: &str) -> Result<FsInfo, String> {
    let info = match stat(path) {
        Some(info) => info,
        None => {
            return Err(format!(
                "The path {} does not exist. Please provide a valid path.",
                display_path(path)
            ));
        }
    };
    if info.fs_type == FsType::Directory && command != "view" {
        return Err(format!(
            "The path {} is a directory and only the `view` command can be used on directories",
            display_path(path)
        ));
    }
    Ok(info)
}

fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read \"{}\": {}", display_path(path), e))
}

/// Atomic write: write to a temp sibling then rename over the target.
fn write_file_atomic(path: &Path, content: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    let tmp = dir.join(format!(
        ".{file_name}.dsh-tmp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("cannot write \"{}\": {}", display_path(path), e))
}

fn format_file_view(
    path: &str,
    content: &str,
    max_output_chars: usize,
    view_range: Option<&[i64]>,
) -> Result<String, String> {
    let all_lines: Vec<&str> = content.split('\n').collect();
    let mut lines: Vec<&str> = all_lines.clone();
    let mut initial_line: i64 = 1;
    let mut prompt = format!(
        "Here's the content of {path} with line numbers (which has a total of {} lines)",
        all_lines.len()
    );
    if let Some(range) = view_range {
        if range.len() != 2 {
            return Err("Invalid `view_range`. It should be a list of two integers.".to_string());
        }
        let (a, b) = (range[0], range[1]);
        // The TS implementation treats any non-integer as invalid; JSON
        // integers arrive here already, so length-2 is the only check left.
        initial_line = a;
        let final_line = b;
        if initial_line < 1 || initial_line > all_lines.len() as i64 {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its first element `{}` should be within the range of lines of the file: [1, {}]",
                range[0],
                range[1],
                initial_line,
                all_lines.len()
            ));
        }
        if final_line > all_lines.len() as i64 {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its second element `{}` should be smaller than the number of lines in the file: `{}`",
                range[0],
                range[1],
                final_line,
                all_lines.len()
            ));
        }
        if final_line != -1 && final_line < initial_line {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its second element `{}` should be larger or equal than its first `{}`",
                range[0], range[1], final_line, initial_line
            ));
        }
        lines = if final_line == -1 {
            all_lines[(initial_line - 1) as usize..].to_vec()
        } else {
            all_lines[(initial_line - 1) as usize..final_line as usize].to_vec()
        };
        prompt.push_str(&format!(" with view_range=[{initial_line}, {final_line}]"));
    }
    let numbered = lines
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>6}  {}", initial_line + index as i64, line))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(maybe_truncate(
        &format!("{prompt}:\n{numbered}\n"),
        max_output_chars,
    ))
}

fn list_directory(path: &Path, max_output_chars: usize) -> Result<String, String> {
    fn visit(path: &Path, depth: usize) -> Result<Vec<String>, String> {
        let entries = std::fs::read_dir(path)
            .map_err(|e| format!("cannot list \"{}\": {}", display_path(path), e))?;
        let mut rows = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("cannot list \"{}\": {}", display_path(path), e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "__pycache__" {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|e| format!("cannot list \"{}\": {}", display_path(path), e))?;
            let t = if file_type.is_dir() {
                'd'
            } else if file_type.is_file() {
                'f'
            } else {
                '?'
            };
            let target = entry.path();
            rows.push(format!("{t}\t{}", display_path(&target)));
            if file_type.is_dir() && depth < 2 {
                rows.extend(visit(&target, depth + 1)?);
            }
        }
        Ok(rows)
    }
    let mut rows = vec![format!("d\t{}", display_path(path))];
    rows.extend(visit(path, 1)?);
    rows.sort_by(|left, right| {
        let left_path = &left[left.find('\t').map(|i| i + 1).unwrap_or(0)..];
        let right_path = &right[right.find('\t').map(|i| i + 1).unwrap_or(0)..];
        codepoint_compare(left_path, right_path)
    });
    let listing = maybe_truncate(&(rows.join("\n") + "\n"), max_output_chars);
    Ok(format!(
        "Here're the files and directories up to 2 levels deep in {}, excluding hidden items, node_modules, and Python cache directories:\n{listing}\n",
        display_path(path)
    ))
}

fn view_path(
    path: &str,
    view_range: Option<&[i64]>,
    max_output_chars: usize,
) -> Result<String, String> {
    let target = resolve_target(path)?;
    let info = stat_existing(&target, "view")?;
    if info.fs_type == FsType::Directory {
        if view_range.is_some() {
            return Err(
                "The `view_range` parameter is not allowed when `path` points to a directory."
                    .to_string(),
            );
        }
        return list_directory(&target, max_output_chars);
    }
    if info.fs_type != FsType::File {
        return Err(format!(
            "cannot view \"{}\": not a regular file or directory",
            display_path(&target)
        ));
    }
    let content = read_text(&target)?;
    format_file_view(
        &display_path(&target),
        &content,
        max_output_chars,
        view_range,
    )
}

fn create_file(path: &str, file_text: Option<&str>) -> Result<String, String> {
    let content = required_for_command(file_text, "file_text", "create", true)?;
    let target = resolve_target(path)?;
    if stat(&target).is_some() {
        return Err(format!(
            "File already exists at: {}. Cannot overwrite files using command `create`.",
            display_path(&target)
        ));
    }
    write_file_atomic(&target, &content)?;
    Ok(format!(
        "New file created successfully at: {}",
        display_path(&target)
    ))
}

fn replace_in_file(
    path: &str,
    old_str: Option<&str>,
    new_str: Option<&str>,
) -> Result<String, String> {
    let target = resolve_target(path)?;
    let old_value = required_for_command(old_str, "old_str", "str_replace", false)?;
    let new_value = new_str.unwrap_or("");
    let info = stat_existing(&target, "str_replace")?;
    if info.fs_type != FsType::File {
        return Err(format!(
            "cannot edit \"{}\": not a regular file",
            display_path(&target)
        ));
    }
    let before = read_text(&target)?;
    let offsets = match_offsets(&before, &old_value);
    let offset = offsets.first().copied();
    match offset {
        None => {
            return Err(format!(
                "No replacement was performed, old_str `{}` did not appear verbatim in {}.",
                old_value,
                display_path(&target)
            ));
        }
        Some(_) if offsets.len() > 1 => {
            let lines = line_numbers_at(&before, &offsets);
            let joined = lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "No replacement was performed. Multiple occurrences of old_str `{}` in lines [{}]. Please ensure it is unique",
                old_value, joined
            ));
        }
        Some(offset) => {
            // Stale-version guard: fail if the file changed between read and write.
            if info.version != Version::of(&target) {
                return Err(format!(
                    "cannot write \"{}\": file changed since it was read",
                    display_path(&target)
                ));
            }
            let mut after = before;
            after.replace_range(offset..offset + old_value.len(), new_value);
            write_file_atomic(&target, &after)?;
            Ok(format!(
                "The file {} has been edited successfully.",
                display_path(&target)
            ))
        }
    }
}

fn insert_in_file(
    path: &str,
    insert_line: Option<i64>,
    new_str: Option<&str>,
) -> Result<String, String> {
    let insert_line = insert_line
        .ok_or_else(|| "Parameter `insert_line` is required for command: insert".to_string())?;
    let value = required_for_command(new_str, "new_str", "insert", true)?;
    let target = resolve_target(path)?;
    let info = stat_existing(&target, "insert")?;
    if info.fs_type != FsType::File {
        return Err(format!(
            "cannot insert into \"{}\": not a regular file",
            display_path(&target)
        ));
    }
    let before = read_text(&target)?;
    let lines: Vec<&str> = before.split('\n').collect();
    if insert_line < 0 || insert_line > lines.len() as i64 {
        return Err(format!(
            "Invalid `insert_line` parameter: {insert_line}. It should be within the range of lines of the file: [0, {}]",
            lines.len()
        ));
    }
    if info.version != Version::of(&target) {
        return Err(format!(
            "cannot write \"{}\": file changed since it was read",
            display_path(&target)
        ));
    }
    let mut after = Vec::new();
    after.extend_from_slice(&lines[..insert_line as usize]);
    after.extend(value.split('\n'));
    after.extend_from_slice(&lines[insert_line as usize..]);
    let content = after.join("\n");
    write_file_atomic(&target, &content)?;
    Ok(format!(
        "The file {} has been edited successfully.",
        display_path(&target)
    ))
}

fn parse_view_range(args: &serde_json::Value) -> Option<Vec<i64>> {
    args.get("view_range")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect::<Vec<i64>>())
}

fn handle(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx
        .args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = ctx.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let file_text = ctx.args.get("file_text").and_then(|v| v.as_str());
    let new_str = ctx.args.get("new_str").and_then(|v| v.as_str());
    let old_str = ctx.args.get("old_str").and_then(|v| v.as_str());
    let insert_line = ctx.args.get("insert_line").and_then(|v| v.as_i64());
    let view_range = parse_view_range(&ctx.args);

    let result = match command {
        "view" => view_path(path, view_range.as_deref(), DEFAULT_MAX_OUTPUT_CHARS),
        "create" => create_file(path, file_text),
        "str_replace" => replace_in_file(path, old_str, new_str),
        "insert" => insert_in_file(path, insert_line, new_str),
        _ => Err(format!("unknown command: {command}")),
    };

    match result {
        Ok(text) => ToolResult::ok_with_limit(text, None),
        Err(message) => ToolResult::error_with("TOOL_ERROR", message, false, None),
    }
}

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The commands to run. Allowed options are: `view`, `create`, `str_replace`, `insert`.",
                "enum": ["view", "create", "str_replace", "insert"]
            },
            "path": {
                "type": "string",
                "description": "Absolute path to file or directory, e.g. `/repo/file.py` or `/repo`."
            },
            "file_text": {
                "type": "string",
                "description": "Required parameter of `create` command, with the content of the file to be created."
            },
            "insert_line": {
                "type": "integer",
                "description": "Required parameter of `insert` command. The `new_str` will be inserted AFTER the line `insert_line` of `path`."
            },
            "new_str": {
                "type": "string",
                "description": "Optional parameter of `str_replace` command containing the new string (if not given, no string will be added). Required parameter of `insert` command containing the string to insert."
            },
            "old_str": {
                "type": "string",
                "description": "Required parameter of `str_replace` command containing the string in `path` to replace."
            },
            "view_range": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "Optional parameter of `view` command when `path` points to a file. If none is given, the full file is shown. If provided, the file will be shown in the indicated line number range, e.g. [11, 12] will show lines 11 and 12. Indexing at 1 to start. Setting `[start_line, -1]` shows all lines from `start_line` to the end of the file."
            }
        },
        "required": ["command", "path"]
    })
}

/// Register the `str_replace_editor` tool on a QAQ-Harness ToolManager.
pub fn register(mgr: &mut ToolManager) {
    let description: &'static str = DEFAULT_DESCRIPTION;
    mgr.register_with_placement(
        ToolHandler {
            key: "str_replace_editor".to_string(),
            description,
            input_schema: input_schema(),
            handler: handle,
            risk: ToolRisk::Write,
            category: ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(60),
        },
        ToolPlacement::Workspace,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_verbatim_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let abs = display_path(&path);
        let out = view_path(&abs, None, DEFAULT_MAX_OUTPUT_CHARS).unwrap();
        // Minimal-mode formats every line with a two-space separator, so an
        // empty trailing line keeps its trailing spaces (the harness trims).
        assert_eq!(
            out,
            format!(
                "Here's the content of {abs} with line numbers (which has a total of 3 lines):\n     1  hello\n     2  world\n     3  \n"
            )
        );
    }

    #[test]
    fn view_range_clips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "l1\nl2\nl3\n").unwrap();
        let abs = display_path(&path);
        let out = view_path(&abs, Some(&[2, 3]), DEFAULT_MAX_OUTPUT_CHARS).unwrap();
        assert!(out.contains("with view_range=[2, 3]"));
        assert!(out.contains("     2  l2"));
        assert!(out.contains("     3  l3"));
        assert!(!out.contains("l1"));
    }

    #[test]
    fn view_range_invalid_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "l1\n").unwrap();
        let abs = display_path(&path);
        let err = view_path(&abs, Some(&[0, 2]), DEFAULT_MAX_OUTPUT_CHARS).unwrap_err();
        assert!(
            err.contains("should be within the range of lines of the file: [1, 2]"),
            "{err}"
        );
    }

    #[test]
    fn create_new_file_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let abs = display_path(&path);
        let out = create_file(&abs, Some("content\n")).unwrap();
        assert_eq!(out, format!("New file created successfully at: {abs}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content\n");
    }

    #[test]
    fn create_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        std::fs::write(&path, "x").unwrap();
        let abs = display_path(&path);
        let err = create_file(&abs, Some("content")).unwrap_err();
        assert_eq!(
            err,
            format!(
                "File already exists at: {abs}. Cannot overwrite files using command `create`."
            )
        );
    }

    #[test]
    fn str_replace_single_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "foo\nbar\n").unwrap();
        let abs = display_path(&path);
        let out = replace_in_file(&abs, Some("bar"), Some("baz")).unwrap();
        assert_eq!(out, format!("The file {abs} has been edited successfully."));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo\nbaz\n");
    }

    #[test]
    fn str_replace_not_found_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "foo\n").unwrap();
        let abs = display_path(&path);
        let err = replace_in_file(&abs, Some("nope"), Some("x")).unwrap_err();
        assert_eq!(
            err,
            format!(
                "No replacement was performed, old_str `nope` did not appear verbatim in {abs}."
            )
        );
    }

    #[test]
    fn str_replace_ambiguous_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\ny\nx\n").unwrap();
        let abs = display_path(&path);
        let err = replace_in_file(&abs, Some("x"), Some("z")).unwrap_err();
        assert_eq!(
            err,
            format!(
                "No replacement was performed. Multiple occurrences of old_str `x` in lines [1, 3]. Please ensure it is unique"
            )
        );
    }

    #[test]
    fn insert_middle_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "l1\nl2\n").unwrap();
        let abs = display_path(&path);
        let out = insert_in_file(&abs, Some(1), Some("mid")).unwrap();
        assert_eq!(out, format!("The file {abs} has been edited successfully."));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "l1\nmid\nl2\n");
    }

    #[test]
    fn insert_invalid_line_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "l1\n").unwrap();
        let abs = display_path(&path);
        let err = insert_in_file(&abs, Some(5), Some("x")).unwrap_err();
        assert_eq!(
            err,
            format!(
                "Invalid `insert_line` parameter: 5. It should be within the range of lines of the file: [0, 2]"
            )
        );
    }

    #[test]
    fn non_absolute_path_verbatim() {
        let err = resolve_target("relative/path.txt").unwrap_err();
        assert_eq!(
            err,
            "The path relative/path.txt is not an absolute path, it should start with `/`. Maybe you meant /relative/path.txt?"
        );
    }

    #[test]
    fn list_directory_excludes_hidden_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("a.rs"), "x").unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();
        std::fs::write(dir.path().join("node_modules"), "x").unwrap();
        let abs = display_path(dir.path());
        let out = list_directory(dir.path(), DEFAULT_MAX_OUTPUT_CHARS).unwrap();
        assert!(out.starts_with(&format!(
            "Here're the files and directories up to 2 levels deep in {abs}, excluding hidden items, node_modules, and Python cache directories:"
        )));
        // The header mentions the exclusion list; the *rows* must not contain
        // the hidden / excluded entries.
        let body = out.split_once('\n').map(|(_, b)| b).unwrap_or(&out);
        assert!(!body.contains(".hidden"));
        assert!(
            !body
                .lines()
                .any(|l| l.contains("node_modules") && l.contains('\t'))
        );
        assert!(body.contains("src"));
        assert!(body.contains("a.rs"));
    }
}
