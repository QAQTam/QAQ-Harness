//! `bash_v2` — faithful replication of the deepseek-harness minimal-mode
//! persistent `bash` tool.
//!
//! Registered as `bash_v2` (isolated from the existing non-persistent QAQ-Harness
//! `bash` tool). Model-facing returns are reproduced verbatim from the
//! minimal-mode extraction, including markers, exit-code suffixes, timeout /
//! truncation / reset messages.

use crate::pty::{PtySession, read_page, retained_text};
use qaqh_types::ToolResult;
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolPlacement, ToolRisk};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const TRUNCATED_MESSAGE: &str = "<response clipped><NOTE>To save on context only part of this file has been shown to you. You should retry this tool after you have searched inside the file with `grep -n` in order to find the line numbers of what you are looking for.</NOTE>";
const LOST_PREFIX_MESSAGE: &str = "<response clipped><NOTE>The beginning of this command output was dropped by the terminal scrollback limit. The following text is the earliest retained output.</NOTE>\n";
const SHELL_RESET_MESSAGE: &str = "The persistent bash shell was reset; the next bash call starts from the workspace with a fresh current directory and environment.";
const SHELL_PROMPT: &str = "__DSH_PERSISTENT_BASH_PROMPT__ ";
#[allow(dead_code)] // kept for minimal-mode fidelity
const TIMEOUT_CODE: &str = "PERSISTENT_BASH_TIMEOUT";
const SCROLLBACK_PAGE_LINES: usize = 1_000;
const POLL_INTERVAL_MS: u64 = 25;

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 16_000;
// The exact description the minimal preset configures for the model-facing
// `bash` schema (minimal-mode-extraction README §4.2).
const DEFAULT_DESCRIPTION: &str = "Run commands in a bash shell\n* When invoking this tool, the contents of the \"command\" parameter does NOT need to be XML-escaped.\n* You don't have access to the internet via this tool.\n* You do have access to a mirror of common linux and python packages via apt and pip.\n* State is persistent across command calls and discussions with the user.\n* To inspect a particular line range of a file, e.g. lines 10-25, try 'sed -n 10,25p /path/to/the/file'.\n* Please avoid commands that may produce a very large amount of output.\n* Please run long lived commands in the background, e.g. 'sleep 10 &' or start a server in the background.";

#[derive(Debug, Clone)]
struct CommandMarkers {
    start: String,
    end: String,
}

struct RetainedOutput {
    text: String,
    #[allow(dead_code)]
    truncated: bool,
}

struct CapturedOutput {
    text: String,
    incomplete: bool,
    exit_code: Option<i32>,
}

fn maybe_truncate(content: &str, max_output_chars: usize, incomplete: bool) -> String {
    let len = content.chars().count();
    if len <= max_output_chars && !incomplete {
        return content.to_string();
    }
    if len <= max_output_chars {
        return format!("{content}{TRUNCATED_MESSAGE}");
    }
    let head: String = content.chars().take(max_output_chars).collect();
    format!("{head}{TRUNCATED_MESSAGE}")
}

fn markers() -> CommandMarkers {
    let nonce = uuid::Uuid::new_v4().to_string();
    CommandMarkers {
        start: format!("__DSH_PERSISTENT_BASH_START_{nonce}__"),
        end: format!("__DSH_PERSISTENT_BASH_END_{nonce}:"),
    }
}

fn quote_for_bash(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("\\'"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            c => escaped.push(c),
        }
    }
    format!("$'{escaped}'")
}

fn wrap_command(command: &str, marker: &CommandMarkers) -> String {
    format!(
        "printf '%s\\n' {}; eval -- {}; __dsh_persistent_bash_status=$?; printf '%s%s\\n' {} \"$__dsh_persistent_bash_status\"",
        quote_for_bash(&marker.start),
        quote_for_bash(command),
        quote_for_bash(&marker.end)
    )
}

fn strip_prompt(text: &str) -> String {
    let mut result: String;
    if let Some(stripped) = text.strip_suffix("\r\n") {
        result = stripped.to_string();
    } else if let Some(stripped) = text.strip_suffix('\n') {
        result = stripped.to_string();
    } else {
        result = text.to_string();
    }
    while result.ends_with(SHELL_PROMPT) {
        result.truncate(result.len() - SHELL_PROMPT.len());
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

fn command_output(snapshot: &RetainedOutput, marker: &CommandMarkers) -> Option<CapturedOutput> {
    let text = &snapshot.text;
    let end = text.rfind(&marker.end)?;
    let after_end = &text[end + marker.end.len()..];

    // status = /^(\d+)\r?\n/.exec(after_end)
    let after_cr = after_end.strip_prefix('\r').unwrap_or(after_end);
    let digits_len = after_cr
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, _)| i)
        .last()
        .map(|last| last + 1)
        .unwrap_or(0);
    if digits_len == 0 {
        return None;
    }
    let digits = &after_cr[..digits_len];
    let rest = &after_cr[digits_len..];
    if !(rest.starts_with("\r\n") || rest.starts_with('\n')) {
        return None;
    }
    let status: i64 = digits.parse().ok()?;

    let start_marker = text[..end].rfind(&marker.start);
    let start = match start_marker {
        Some(i) => i + marker.start.len(),
        None => 0,
    };
    let raw = &text[start..end];
    let body = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    Some(CapturedOutput {
        text: strip_prompt(body),
        incomplete: start_marker.is_none(),
        exit_code: Some(status as i32),
    })
}

fn prompt_completed(viewport: &str) -> bool {
    viewport.ends_with(SHELL_PROMPT)
        || viewport.ends_with(&format!("{SHELL_PROMPT}\r\n"))
        || viewport.ends_with(&format!("{SHELL_PROMPT}\n"))
}

fn partial_output(
    snapshot: &RetainedOutput,
    marker: &CommandMarkers,
    fallback: &str,
    fallback_truncated: bool,
) -> CapturedOutput {
    if let Some(i) = snapshot.text.rfind(&marker.start) {
        let after = &snapshot.text[i + marker.start.len()..];
        let body = after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
            .unwrap_or(after);
        return CapturedOutput {
            text: strip_prompt(body),
            incomplete: false,
            exit_code: None,
        };
    }
    let fallback_start = fallback.rfind(&marker.start);
    let after_start = match fallback_start {
        Some(i) => {
            let a = &fallback[i + marker.start.len()..];
            a.strip_prefix("\r\n")
                .or_else(|| a.strip_prefix('\n'))
                .unwrap_or(a)
                .to_string()
        }
        None => fallback.to_string(),
    };
    let fallback_end = after_start.rfind(&marker.end);
    let before_end = match fallback_end {
        Some(i) => &after_start[..i],
        None => &after_start,
    };
    CapturedOutput {
        text: strip_prompt(&before_end.replace(SHELL_PROMPT, "")),
        incomplete: fallback_truncated || fallback_start.is_none(),
        exit_code: None,
    }
}

fn append_status_marker(content: &str, marker: Option<&str>) -> String {
    match marker {
        None => content.to_string(),
        Some(m) => {
            if content.is_empty() {
                m.to_string()
            } else {
                format!("{content}\n{m}")
            }
        }
    }
}

fn render_captured(output: &CapturedOutput, max_output_chars: usize) -> String {
    let rendered = maybe_truncate(&output.text, max_output_chars, output.incomplete);
    let with_prefix = if output.incomplete && !output.text.is_empty() {
        format!("{LOST_PREFIX_MESSAGE}{rendered}")
    } else {
        rendered
    };
    let marker = match output.exit_code {
        Some(code) if code != 0 => Some(format!("[exit code: {code}]")),
        _ => None,
    };
    append_status_marker(&with_prefix, marker.as_deref())
}

fn render_shell_exit_status(content: &str, exit_code: Option<i32>, signal: Option<i32>) -> String {
    let marker = match signal {
        Some(sig) => format!("[shell killed by signal: {sig}]"),
        None => match exit_code {
            Some(code) => format!("[shell exited: code {code}]"),
            None => "[shell exited]".to_string(),
        },
    };
    append_status_marker(content, Some(&marker))
}

// ── Persistent shell registry (owner-scoped) ──

struct PersistentShells {
    live: Mutex<HashMap<String, ArcSafe<PtySession>>>,
}

/// Small wrapper so the map value is cheaply cloneable across calls.
type ArcSafe<T> = std::sync::Arc<T>;

static SHELLS: OnceLock<PersistentShells> = OnceLock::new();

fn shells() -> &'static PersistentShells {
    SHELLS.get_or_init(|| PersistentShells {
        live: Mutex::new(HashMap::new()),
    })
}

fn get_shell(owner: &str, cwd: &str) -> Result<ArcSafe<PtySession>, String> {
    if let Some(existing) = shells().live.lock().unwrap().get(owner) {
        return Ok(existing.clone());
    }
    let session = PtySession::spawn(cwd)?;
    let setup = format!("stty -echo; PS1={}", quote_for_bash(SHELL_PROMPT));
    session.send_line(&setup)?;
    // Wait for the controlled prompt so the shell has accepted initialization
    // (mirrors the minimal-mode setup check).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let text = retained_text(&session.scrollback());
        if text.ends_with(SHELL_PROMPT) || session.is_exited() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            reset_shell(owner, "persistent bash initialization failed");
            return Err("persistent bash shell did not accept initialization".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
    shells()
        .live
        .lock()
        .unwrap()
        .insert(owner.to_string(), session.clone());
    Ok(session)
}

fn reset_shell(owner: &str, _reason: &str) {
    if let Some(removed) = shells().live.lock().unwrap().remove(owner) {
        removed.kill();
    }
}

fn execute_command(owner: &str, command: &str, cwd: &str) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("command must be a non-empty string".to_string());
    }
    let shell = get_shell(owner, cwd)?;
    let marker = markers();
    let wrapped = wrap_command(command, &marker);
    shell.send_line(&wrapped)?;

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS);
    let mut fallback = String::new();
    let fallback_truncated = false;

    loop {
        let snapshot = RetainedOutput {
            text: retained_text(&shell.scrollback()),
            truncated: false,
        };
        let timed_out = start.elapsed() >= timeout;

        if timed_out {
            let partial = partial_output(&snapshot, &marker, &fallback, fallback_truncated);
            let rendered = render_captured(&partial, DEFAULT_MAX_OUTPUT_CHARS);
            reset_shell(owner, "persistent bash command timed out");
            let secs = timeout.as_secs();
            return Ok(format!(
                "Your command timed out after {secs} seconds or experienced an OOM error. Below is partial output:\n{rendered}\n{SHELL_RESET_MESSAGE}"
            ));
        }
        if snapshot.text.contains(&marker.end) {
            if let Some(complete) = command_output(&snapshot, &marker) {
                return Ok(render_captured(&complete, DEFAULT_MAX_OUTPUT_CHARS));
            }
        }
        if shell.is_exited() {
            let partial = partial_output(&snapshot, &marker, &fallback, fallback_truncated);
            let rendered = render_captured(&partial, DEFAULT_MAX_OUTPUT_CHARS);
            let exit = shell.try_exit_code().flatten();
            let status = render_shell_exit_status(&rendered, exit, None);
            reset_shell(owner, "persistent bash shell exited");
            let mut parts = vec![status, SHELL_RESET_MESSAGE.to_string()];
            parts.retain(|p| !p.is_empty());
            return Ok(parts.join("\n"));
        }
        // Only consider the prompt a completion signal once the current
        // command has actually started producing output (its start marker has
        // appeared); otherwise a lingering prompt from a previous command would
        // short-circuit the poll before this command runs.
        if snapshot.text.contains(&marker.start) && prompt_completed(&snapshot.text) {
            let partial = partial_output(&snapshot, &marker, &fallback, fallback_truncated);
            return Ok(render_captured(&partial, DEFAULT_MAX_OUTPUT_CHARS));
        }
        let latest = read_page(&shell.scrollback(), SCROLLBACK_PAGE_LINES);
        if latest.text.len() > fallback.len() {
            fallback = latest.text.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
}

fn owner_key() -> String {
    if let Some(ctx) = qaqh_workspace::runtime::context() {
        return ctx.active_session;
    }
    if let Ok(guard) = qaqh_workspace::CURRENT_SESSION.lock() {
        if let Some(s) = guard.clone() {
            return s;
        }
    }
    "default".to_string()
}

fn shell_cwd() -> String {
    let ws = qaqh_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if ws.trim().is_empty() || ws.trim() == "." {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        ws.clone()
    }
}

fn handle(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx
        .args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if command.trim().is_empty() {
        return ToolResult::error_with(
            "TOOL_ERROR",
            "command must be a non-empty string",
            false,
            None,
        );
    }
    let owner = owner_key();
    let cwd = shell_cwd();
    match execute_command(&owner, command, &cwd) {
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
                "description": "The bash command to run. Relative path is preferred in the command."
            }
        },
        "required": ["command"]
    })
}

/// Register the minimal-mode persistent bash tool under the name `bash_v2`.
pub fn register(mgr: &mut ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "bash_v2".to_string(),
            description: DEFAULT_DESCRIPTION,
            input_schema: input_schema(),
            handler: handle,
            risk: ToolRisk::Destructive,
            category: ToolCategory::Exec,
            default_timeout: std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS),
        },
        ToolPlacement::Workspace,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_for_bash_escapes() {
        assert_eq!(quote_for_bash("a'b\\c\r\nd"), "$'a\\'b\\\\c\\r\\nd'");
    }

    #[test]
    fn wrap_command_keeps_single_line() {
        let marker = CommandMarkers {
            start: "S".into(),
            end: "E:".into(),
        };
        let wrapped = wrap_command("echo hi", &marker);
        assert!(wrapped.starts_with("printf '%s\\n' $'S'; eval -- $'echo hi';"));
        assert!(wrapped.ends_with("; printf '%s%s\\n' $'E:' \"$__dsh_persistent_bash_status\""));
    }

    #[test]
    fn strip_prompt_strips_trailing_prompt() {
        assert_eq!(strip_prompt("out\n"), "out");
        assert_eq!(strip_prompt("out"), "out");
        assert_eq!(strip_prompt(&format!("out\n{SHELL_PROMPT}")), "out");
    }

    #[test]
    fn command_output_parses_exit_code() {
        let marker = CommandMarkers {
            start: "S".into(),
            end: "E:".into(),
        };
        let text = format!("S\nhello\nE:7\n");
        let out = command_output(
            &RetainedOutput {
                text: text.clone(),
                truncated: false,
            },
            &marker,
        )
        .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.exit_code, Some(7));
        assert!(!out.incomplete);
    }

    #[test]
    fn render_captured_appends_exit_code() {
        let out = CapturedOutput {
            text: "boom".into(),
            incomplete: false,
            exit_code: Some(2),
        };
        assert_eq!(
            render_captured(&out, DEFAULT_MAX_OUTPUT_CHARS),
            "boom\n[exit code: 2]"
        );
    }

    #[test]
    fn render_captured_zero_exit_no_marker() {
        let out = CapturedOutput {
            text: "ok".into(),
            incomplete: false,
            exit_code: Some(0),
        };
        assert_eq!(render_captured(&out, DEFAULT_MAX_OUTPUT_CHARS), "ok");
    }

    #[test]
    fn maybe_truncate_clips_with_message() {
        let long = "x".repeat(DEFAULT_MAX_OUTPUT_CHARS + 10);
        let out = maybe_truncate(&long, DEFAULT_MAX_OUTPUT_CHARS, false);
        assert!(out.starts_with(&"x".repeat(DEFAULT_MAX_OUTPUT_CHARS)));
        assert!(out.ends_with(TRUNCATED_MESSAGE));
    }

    #[test]
    fn maybe_truncate_cjk_keeps_char_boundary() {
        // 汉字（3 字节 UTF-8）× (cap+100) > 16K cap：截断后的头部必须完整，
        // 绝不切在多字节字符中间（否则会 panic 或产生替换字符）。
        let long = "中".repeat(DEFAULT_MAX_OUTPUT_CHARS + 100);
        let out = maybe_truncate(&long, DEFAULT_MAX_OUTPUT_CHARS, false);
        assert!(out.starts_with(&"中".repeat(DEFAULT_MAX_OUTPUT_CHARS)));
        assert!(out.ends_with(TRUNCATED_MESSAGE));
        assert!(!out.contains('\u{FFFD}'), "replacement char leaked");
    }

    #[test]
    fn timeout_message_shape() {
        let marker = CommandMarkers {
            start: "S".into(),
            end: "E:".into(),
        };
        let snapshot = RetainedOutput {
            text: "S\npartial".into(),
            truncated: false,
        };
        let partial = partial_output(&snapshot, &marker, "", false);
        let rendered = render_captured(&partial, DEFAULT_MAX_OUTPUT_CHARS);
        let secs = DEFAULT_TIMEOUT_MS / 1000;
        let msg = format!(
            "Your command timed out after {secs} seconds or experienced an OOM error. Below is partial output:\n{rendered}\n{SHELL_RESET_MESSAGE}"
        );
        assert!(
            msg.starts_with(
                "Your command timed out after 300 seconds or experienced an OOM error."
            )
        );
        assert!(msg.ends_with(SHELL_RESET_MESSAGE));
    }
}
