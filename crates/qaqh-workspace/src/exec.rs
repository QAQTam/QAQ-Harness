//! Command execution — direct process spawn via argv array.
//!
//! No PTY and no shell. Uses `std::process::Command` and streams pipe chunks
//! to the UI while retaining a bounded final result for the LLM.
//! Output is read via pipes (not `output()`) to prevent OOM on large outputs,
//! and truncated by actual token count using `qaqh_types::token::count_tokens`.
//!
//! Two invocation modes:
//!   • `argv`  — direct exec, no shell (for simple program calls).
//!   • `command` — auto-wrapped in the platform shell, enabling pipes,
//!     redirects, and builtins without the model needing to spell out the
//!     shell executable manually.

use crate::{ExecOutputStream, ExecProgressEvent, ExecProgressSender, ToolCallCtx, ToolResult};
use serde::Serialize;
use std::io::Read;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

// ── Platform shell detection ──
// Adapted from codex-rs/shell-command/src/shell_detect.rs & core/src/shell.rs.
// Stripped to the minimum needed: pick the right shell, derive argv.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Shell {
    Bash,
    Zsh,
    Sh,
    PowerShell,
    Cmd,
}

static DETECTED_SHELL: OnceLock<Shell> = OnceLock::new();
/// Full path to bash on Windows — avoids the WSL wrapper at System32\\bash.exe.
static DETECTED_BASH_PATH: OnceLock<String> = OnceLock::new();
/// Full path to PowerShell on Windows（pwsh 7 优先，powershell.exe 兜底）。
static DETECTED_PWSH_PATH: OnceLock<String> = OnceLock::new();

impl Shell {
    /// Auto-detect the best available shell on this platform.
    fn detect() -> Self {
        *DETECTED_SHELL.get_or_init(Self::detect_uncached)
    }

    /// Resolve an explicit shell name requested by the model (exec `shell`
    /// parameter). Windows `bash` resolves to Git-for-Windows / MSYS2 when
    /// present, avoiding the WSL wrapper. Unknown names fall back to None so
    /// the caller can report a clean error.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "bash" => {
                #[cfg(windows)]
                {
                    const WIN_BASH_CANDIDATES: &[&str] = &[
                        "C:\\Program Files\\Git\\bin\\bash.exe",
                        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
                        "C:\\msys64\\usr\\bin\\bash.exe",
                    ];
                    for p in WIN_BASH_CANDIDATES {
                        if std::path::Path::new(p).is_file() {
                            DETECTED_BASH_PATH.get_or_init(|| p.to_string());
                            return Some(Shell::Bash);
                        }
                    }
                    if let Some(found) = find_bash_on_path() {
                        DETECTED_BASH_PATH.get_or_init(|| found);
                        return Some(Shell::Bash);
                    }
                    // No git bash available — plain `bash` (may be WSL wrapper,
                    // but the model explicitly asked for bash).
                    Some(Shell::Bash)
                }
                #[cfg(not(windows))]
                {
                    Some(Shell::Bash)
                }
            }
            "zsh" => Some(Shell::Zsh),
            "sh" => Some(Shell::Sh),
            "pwsh" | "powershell" => Some(Shell::PowerShell),
            "cmd" => Some(Shell::Cmd),
            _ => None,
        }
    }

    fn detect_uncached() -> Self {
        #[cfg(windows)]
        {
            // Windows 默认 PowerShell（pwsh 7 优先，Windows 自带 powershell.exe 兜底）；
            // 模型需要 POSIX 语义时显式传 `shell: "bash"`。
            if executable_on_path("pwsh") {
                DETECTED_PWSH_PATH.get_or_init(|| "pwsh".to_string());
                return Shell::PowerShell;
            }
            if executable_on_path("powershell") {
                DETECTED_PWSH_PATH.get_or_init(|| "powershell".to_string());
                return Shell::PowerShell;
            }
            // 无 PowerShell（罕见）：退回 Git for Windows / MSYS2 bash，
            // 避免 WSL wrapper（System32\\bash.exe）。
            const WIN_BASH_CANDIDATES: &[&str] = &[
                "C:\\Program Files\\Git\\bin\\bash.exe",
                "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
                "C:\\msys64\\usr\\bin\\bash.exe",
            ];
            for p in WIN_BASH_CANDIDATES {
                if std::path::Path::new(p).is_file() {
                    DETECTED_BASH_PATH.get_or_init(|| p.to_string());
                    return Shell::Bash;
                }
            }
            if let Some(found) = find_bash_on_path() {
                DETECTED_BASH_PATH.get_or_init(|| found);
                return Shell::Bash;
            }
            Shell::Cmd
        }
        #[cfg(not(windows))]
        {
            if executable_on_path("bash") {
                return Shell::Bash;
            }
            Shell::Sh
        }
    }

    /// Path to the shell executable.
    fn path(&self) -> &str {
        match self {
            Shell::Bash => DETECTED_BASH_PATH
                .get()
                .map(String::as_str)
                .unwrap_or("bash"),
            Shell::Zsh => "zsh",
            Shell::Sh => "sh",
            Shell::PowerShell => DETECTED_PWSH_PATH
                .get()
                .map(String::as_str)
                .unwrap_or("pwsh"),
            Shell::Cmd => "cmd",
        }
    }

    /// Build the argv that runs `command` through this shell.
    fn derive_exec_args(&self, command: &str) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Sh => {
                vec![
                    self.path().to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ]
            }
            Shell::PowerShell => {
                vec![
                    self.path().to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ]
            }
            Shell::Cmd => {
                vec![
                    self.path().to_string(),
                    "/c".to_string(),
                    command.to_string(),
                ]
            }
        }
    }
}

fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    executable_in_dirs(name, std::env::split_paths(&path))
}

fn executable_in_dirs(name: &str, dirs: impl IntoIterator<Item = std::path::PathBuf>) -> bool {
    #[cfg(windows)]
    let candidates = if std::path::Path::new(name).extension().is_some() {
        vec![name.to_string()]
    } else {
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    };
    #[cfg(not(windows))]
    let candidates = vec![name.to_string()];

    dirs.into_iter().any(|dir| {
        candidates
            .iter()
            .any(|candidate| is_executable_file(&dir.join(candidate)))
    })
}

/// Find `bash` on Windows PATH, skipping known WSL wrapper locations
/// (System32, WindowsApps). Returns the full path on success.
#[cfg(windows)]
fn find_bash_on_path() -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let dir_s = dir.to_string_lossy().to_lowercase();
        // Windows System32 contains WSL's bash.exe launcher — skip it.
        if dir_s.contains("\\system32") || dir_s.contains("\\windowsapps") {
            continue;
        }
        let candidate = dir.join("bash.exe");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(not(unix))]
    true
}

/// Stream read from a pipe, capped at `max_bytes`.
///
/// Every retained chunk is also forwarded to the UI progress channel. Once the
/// cap is reached, the rest of the pipe is drained without forwarding so the
/// child cannot block on a full OS pipe.
fn read_stream(
    stream: impl Read,
    max_bytes: usize,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: String,
    output_stream: ExecOutputStream,
    progress_seq: Arc<AtomicU64>,
    registry_id: Option<u32>,
) -> (Vec<u8>, bool) {
    let mut reader = std::io::BufReader::new(stream);
    let mut buf = vec![0u8; 8192];
    let mut out = Vec::new();
    let mut pending_utf8 = Vec::new();
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let retained = n.min(max_bytes.saturating_sub(out.len()));
                if retained > 0 {
                    let chunk = &buf[..retained];
                    out.extend_from_slice(chunk);
                    forward_progress(
                        &mut pending_utf8,
                        chunk,
                        progress_tx.as_ref(),
                        &tool_call_id,
                        output_stream,
                        &progress_seq,
                        registry_id,
                    );
                }
                if retained < n {
                    truncated = true;
                    std::io::copy(&mut reader, &mut std::io::sink()).ok();
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if !pending_utf8.is_empty() {
        send_progress(
            progress_tx.as_ref(),
            &tool_call_id,
            output_stream,
            &progress_seq,
            String::from_utf8_lossy(&pending_utf8).into_owned(),
        );
        if let Some(id) = registry_id {
            append_registry(id, output_stream, &String::from_utf8_lossy(&pending_utf8));
        }
    }
    (out, truncated)
}

/// Forward only complete text units. A command may split one Chinese character
/// across pipe reads; keeping its suffix here avoids replacement glyphs in UI.
/// On Windows, non-UTF-8 console output falls back to the active OEM code page.
fn forward_progress(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    tx: Option<&ExecProgressSender>,
    tool_call_id: &str,
    stream: ExecOutputStream,
    seq: &Arc<AtomicU64>,
    registry_id: Option<u32>,
) {
    pending.extend_from_slice(bytes);
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                send_progress(tx, tool_call_id, stream, seq, valid.to_owned());
                if let Some(id) = registry_id {
                    append_registry(id, stream, valid);
                }
                pending.clear();
                return;
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid_up_to = error.valid_up_to();
                let prefix =
                    String::from_utf8(pending[..valid_up_to].to_vec()).expect("valid UTF-8 prefix");
                pending.drain(..valid_up_to);
                send_progress(tx, tool_call_id, stream, seq, prefix);
            }
            Err(error) if error.error_len().is_some() => {
                #[cfg(windows)]
                if let Some(decoded) = decode_windows_oem(pending) {
                    pending.clear();
                    send_progress(tx, tool_call_id, stream, seq, decoded);
                    return;
                }
                let invalid_len = error.error_len().expect("checked above");
                let replacement = String::from_utf8_lossy(&pending[..invalid_len]).into_owned();
                pending.drain(..invalid_len);
                send_progress(tx, tool_call_id, stream, seq, replacement);
            }
            Err(_) => return, // incomplete character at end; wait for next read.
        }
    }
}

/// 将已解码的输出块追加到进程注册表（backgrounded 后 process_check 可查 tail）。
fn append_registry(id: u32, stream: ExecOutputStream, chunk: &str) {
    match stream {
        ExecOutputStream::Stdout => {
            crate::process_registry::ProcessRegistry::append_output(id, chunk)
        }
        ExecOutputStream::Stderr => {
            crate::process_registry::ProcessRegistry::append_stderr(id, chunk)
        }
    }
}

/// Decode the final capture using UTF-8 first, then the Windows console OEM
/// code page (for example GBK/936 on Simplified-Chinese Windows).
fn decode_captured(bytes: &[u8]) -> String {
    if let Ok(utf8) = std::str::from_utf8(bytes) {
        return utf8.to_owned();
    }
    #[cfg(windows)]
    if let Some(oem) = decode_windows_oem(bytes) {
        return oem;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_windows_oem(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    if bytes.len() > i32::MAX as usize {
        return None;
    }

    // SAFETY: These are well-known Kernel32 FFI functions with stable ABIs.
    // `GetOEMCP` takes no parameters. `MultiByteToWideChar` operates on
    // caller-provided buffers — we pass null for the sizing call, then a
    // properly-sized `Vec<u16>` for the real conversion. All pointer/length
    // pairs are derived from safe Rust slices. The `MB_ERR_INVALID_CHARS`
    // flag prevents silent substitution of invalid sequences (a split DBCS
    // byte at the end of a pipe read is not an error — it waits for the next
    // chunk).  TODO(migration): replace with `windows` crate's
    // `GetOEMCP` / `MultiByteToWideChar` bindings when `qaqh-workspace` gains
    // a `windows` dependency.
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetOEMCP() -> u32;
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte: *const u8,
            multi_byte_len: i32,
            wide_char: *mut u16,
            wide_char_len: i32,
        ) -> i32;
    }

    // MB_ERR_INVALID_CHARS lets a split GBK/DBCS sequence wait for the next
    // read instead of emitting a replacement glyph mid-stream.
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;

    // SAFETY: `GetOEMCP` is a parameterless Kernel32 query with no
    // preconditions on global state.  A returned value of 0 means the OEM
    // code page is unavailable (system corruption or minimal WinPE env) —
    // fall back to UTF-8 lossy decoding.
    let code_page = unsafe { GetOEMCP() };
    if code_page == 0 {
        return None;
    }

    let byte_len = bytes.len() as i32;

    // SAFETY: Sizing call — `wide_char` is null, `wide_char_len` is 0.
    // `bytes` is a valid Rust slice; `byte_len` equals its length.
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];

    // SAFETY: Real conversion — `wide` is a `Vec<u16>` with exactly
    // `wide_len` elements. `bytes.as_ptr()` and `byte_len` match the input
    // slice. The sizing call above guarantees the buffer is large enough.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    (written == wide_len).then(|| String::from_utf16_lossy(&wide))
}

fn send_progress(
    tx: Option<&ExecProgressSender>,
    tool_call_id: &str,
    stream: ExecOutputStream,
    seq: &Arc<AtomicU64>,
    chunk: String,
) {
    if chunk.is_empty() {
        return;
    }
    if let Some(tx) = tx {
        tx.try_send(ExecProgressEvent {
            tool_call_id: tool_call_id.to_string(),
            stream,
            seq: seq.fetch_add(1, Ordering::Relaxed),
            chunk,
        });
    }
}

/// CJK character ranges used for token-count estimation.
/// CJK characters consume ~1.67 tokens each, vs ~3.3 for ASCII.
const fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}'
        | '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}'
        | '\u{3040}'..='\u{30ff}'
    )
}

/// Find byte index for `target` tokens walking forward.
fn find_token_boundary(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    for (i, c) in text.char_indices() {
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return i;
        }
    }
    text.len()
}

/// Find byte index for `target` tokens walking backward from end.
fn find_token_boundary_reverse(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, c) in chars.iter().rev() {
        if is_cjk(*c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return *i;
        }
    }
    0
}

/// Token-aware smart truncation: keeps head (70%) + tail (30%).
fn token_truncate(text: &str, max_tokens: u32) -> String {
    let total = qaqh_types::token::count_tokens(text);
    if total <= max_tokens {
        return text.to_string();
    }
    let head_tokens = (max_tokens as f64 * 0.7).max(1.0) as u32;
    let tail_tokens = (max_tokens as f64 * 0.3).max(1.0) as u32;
    let head_end = find_token_boundary(text, head_tokens);
    let tail_start = find_token_boundary_reverse(text, tail_tokens);
    if head_end >= tail_start {
        let end = find_token_boundary(text, max_tokens);
        format!(
            "{}\n...[TRUNCATED: {}/{} tokens. Call exec again with narrower argv or a filtering command.]",
            text.get(..end).expect("token boundary is a char boundary"),
            max_tokens,
            total
        )
    } else {
        let tail = text
            .get(tail_start..)
            .expect("token boundary is a char boundary");
        format!(
            "{}\n\n...[TRUNCATED: {}/{} tokens, {} lines dropped. Call exec again with narrower argv or a filtering command.]\n\n{}",
            text.get(..head_end)
                .expect("token boundary is a char boundary"),
            max_tokens,
            total,
            text.get(head_end..tail_start)
                .expect("token boundaries are char boundaries")
                .lines()
                .count(),
            tail.trim_start(),
        )
    }
}

/// Direct command execution: argv array, no shell.
/// Uses background threads for pipe reading and poll-based timeout.
fn direct_exec(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
) -> ExecOutput {
    let start_time = std::time::Instant::now();
    let display_name = if argv.len() > 1 {
        format!("{} ...", argv[0])
    } else {
        argv[0].clone()
    };
    const HARD_BYTE_CAP: usize = 5 * 1024 * 1024;

    let mut cmd = std::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    if let Some(env) = env {
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutput {
                status: "completed",
                command: display_name,
                exit_code: Some(-1),
                output: format!("SPAWN FAILED: {e}"),
                truncated: false,
                timed_out: false,
                cancelled: false,
                process_id: None,
            };
        }
    };

    // 接线 ProcessRegistry：先注册（管道线程捕获 proc_id），take 管道后
    // 再把子进程句柄移入注册表（poll 经 try_wait、超时移交可查）。
    let proc_id = crate::process_registry::ProcessRegistry::register(&display_name);
    // 移交标志：只有 backgrounded（超时移交）才需要管道线程善后写回状态；
    // 正常路径 direct_exec 自己 mark_exited，线程不得空轮询。
    let handoff = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Start background pipe readers
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    let progress_seq = Arc::new(AtomicU64::new(0));
    if let Some(p) = child.stdout.take() {
        let progress_tx = progress_tx.clone();
        let tool_call_id = tool_call_id.to_string();
        let progress_seq = progress_seq.clone();
        let handoff = handoff.clone();
        std::thread::spawn(move || {
            let (s, t) = read_stream(
                p,
                HARD_BYTE_CAP,
                progress_tx,
                tool_call_id,
                ExecOutputStream::Stdout,
                progress_seq,
                Some(proc_id),
            );
            let _ = stdout_tx.send((s, t));
            // 仅 backgrounded（超时移交）善后：轮询写回退出状态（EOF 与
            // 进程退出存在毫秒级竞态，try_wait 一次可能恰逢 None）。
            if handoff.load(std::sync::atomic::Ordering::SeqCst) {
                for _ in 0..100 {
                    if let Some(code) = crate::process_registry::ProcessRegistry::try_wait(proc_id)
                    {
                        crate::process_registry::ProcessRegistry::mark_exited(proc_id, code);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        });
    } else {
        let _ = stdout_tx.send((Vec::new(), false));
    }
    if let Some(p) = child.stderr.take() {
        let progress_tx = progress_tx.clone();
        let tool_call_id = tool_call_id.to_string();
        let progress_seq = progress_seq.clone();
        let handoff = handoff.clone();
        std::thread::spawn(move || {
            let (s, t) = read_stream(
                p,
                HARD_BYTE_CAP,
                progress_tx,
                tool_call_id,
                ExecOutputStream::Stderr,
                progress_seq,
                Some(proc_id),
            );
            let _ = stderr_tx.send((s, t));
            if handoff.load(std::sync::atomic::Ordering::SeqCst) {
                for _ in 0..100 {
                    if let Some(code) = crate::process_registry::ProcessRegistry::try_wait(proc_id)
                    {
                        crate::process_registry::ProcessRegistry::mark_exited(proc_id, code);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        });
    } else {
        let _ = stderr_tx.send((Vec::new(), false));
    }

    // 管道已 take，子进程句柄移入注册表（唯一持有）
    crate::process_registry::ProcessRegistry::attach_child(proc_id, child);

    // Poll child with timeout（子进程句柄唯一持有在注册表，经 try_wait 查询）
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    // 快速移交：子进程存活超过 background_after_secs 即移交后台（不等 timeout）。
    // 用于拉起长驻服务（serve/daemon/watch）—���调用方希望尽快拿到
    // backgrounded tool_result，用 process(action=check/wait/kill) 接管，而不是
    // 死等到 timeout_secs 让 agent loop 阻塞。
    let handoff_deadline =
        background_after_secs.map(|secs| start_time + std::time::Duration::from_secs(secs));
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match crate::process_registry::ProcessRegistry::try_wait(proc_id) {
            Some(code) => {
                exit_code = Some(code);
                break;
            }
            None => {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    || crate::is_cancel()
                {
                    // 取消 = 杀进程树（含后代），��止管道泄漏
                    crate::process_registry::ProcessRegistry::kill(proc_id);
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // 超时 = 移交后台（不 kill）：进程存活、管道线程继续
                    // append_output/推流，LLM 可用 process(action=...) 接管。
                    timed_out = true;
                    break;
                }
                if handoff_deadline.is_some_and(|hd| std::time::Instant::now() >= hd) {
                    // 快速移交：进程仍在运行且已超过观察窗口 → 立即转后台。
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // 超时移交：不再等待管道（读取线程仍在后台 append 到注册表）
    if timed_out {
        handoff.store(true, std::sync::atomic::Ordering::SeqCst);
        let info = crate::process_registry::ProcessRegistry::get_info(proc_id)
            .unwrap_or_else(|| serde_json::json!({}));
        return ExecOutput {
            status: "backgrounded",
            command: display_name,
            exit_code: None,
            output: serde_json::json!({
                "backgrounded": true,
                "process_id": proc_id,
                "transferred_after_secs": start_time.elapsed().as_secs_f64(),
                "hint": "进程已转入后台（未终止）。用 process(action=\"check\", id=process_id) 查看状态，process(action=\"wait\", id=process_id) 等待完成，process(action=\"kill\", id=process_id) 终止。",
                "info": info,
            })
            .to_string(),
            truncated: false,
            timed_out: true,
            cancelled: false,
            process_id: Some(proc_id),
        };
    }

    // 正常退出 / 取消：标记注册表状态
    if cancelled {
        crate::process_registry::ProcessRegistry::kill(proc_id);
    } else if let Some(code) = exit_code {
        crate::process_registry::ProcessRegistry::mark_exited(proc_id, code);
    }

    // Collect pipe output (threads finish after child exits)
    let (stdout_out, stdout_trunc) = stdout_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| (b"[WARN] stdout pipe timed out\n".to_vec(), true));
    let (stderr_out, stderr_trunc) = stderr_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| (b"[WARN] stderr pipe timed out\n".to_vec(), true));

    let stdout_out = decode_captured(&stdout_out);
    let stderr_out = decode_captured(&stderr_out);

    let mut combined = String::new();
    if !stderr_out.is_empty() {
        combined.push_str(&stderr_out);
        if !stdout_out.is_empty() {
            combined.push('\n');
        }
    }
    combined.push_str(&stdout_out);

    let hard_trunc = stderr_trunc || stdout_trunc;
    let cleaned = strip_ansi(&combined);
    let total_tokens = qaqh_types::token::count_tokens(&cleaned);
    let (output_str, truncated) = if total_tokens > max_output_tokens || hard_trunc {
        (token_truncate(&cleaned, max_output_tokens), true)
    } else {
        (cleaned, false)
    };

    ExecOutput {
        status: if cancelled { "cancelled" } else { "completed" },
        command: display_name,
        exit_code,
        output: output_str,
        truncated,
        timed_out,
        cancelled,
        process_id: Some(proc_id),
    }
}

/// Structured output from a command execution.
#[derive(Serialize, Debug, Clone)]
pub(crate) struct ExecOutput {
    status: &'static str,
    command: String,
    exit_code: Option<i32>,
    output: String,
    truncated: bool,
    timed_out: bool,
    cancelled: bool,
    /// 超时移交后台时的注册表进程 id（由 process 的 action 使用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    process_id: Option<u32>,
}

impl ExecOutput {
    fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"status":"error","output":"serialization failed"}"#.into())
    }
}

// ── Tool handler ──

/// ripgrep `-rn` 习惯陷阱防御（grep 迁移）。
///
/// `grep -rn`（`-r` 递归 + `-n` 行号）是 POSIX 经典组合；ripgrep 中 `-r` 被
/// 定义为 `--replace`，`rg -rn "pat"` 会被解析为 `-r n`（把匹配替换成字面
/// `n`），输出被污染、搜索不到预期内容。exec 层在调用前把误用的紧贴组合
/// 改写为 rg 的正确写法（递归默认开启，行号用 `-n`）：
///   `rg -rn`  → `rg -n`
///   `rg -rni` → `rg -ni`   （+ 忽略大小写）
///   `rg -rnl` → `rg -nl`   （+ 仅列文件名）
/// 仅改写紧贴组合；`-r` 单独出现（`--replace` 的合法用法，如 `rg -r x pat`）
/// 与长选项 `--replace` 不受影响。grep 本身（`grep -rn` 合法）不处理。
fn normalize_rg_argv(argv: &mut [String]) {
    if !matches!(
        argv.first().map(|p| p.to_lowercase()).as_deref(),
        Some("rg") | Some("rg.exe")
    ) {
        return;
    }
    for arg in argv.iter_mut().skip(1) {
        if let Some(rest) = arg.strip_prefix("-rn") {
            let cleaned = format!("-n{rest}");
            log::info!("[exec] rg habit fix (argv): '{arg}' -> '{cleaned}'");
            *arg = cleaned;
        }
    }
}

/// command 模式版本：在 shell 命令字符串里改写 `rg -rn...` → `rg -n...`。
/// 匹配 `rg` / `rg.exe` 后紧跟的 `-rn` 前缀组合（大小写不敏感，适配
/// Windows `RG.EXE`）；管道/多命令场景同样覆盖。引号内出现的字面文本
/// 也会被改写——罕见且语义无害，接受。
fn normalize_command_rg(command: &str) -> String {
    use std::sync::OnceLock;
    static RG_HABIT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RG_HABIT_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(rg(\.exe)?)\s+-rn([a-z]*)").expect("rg habit regex")
    });
    re.replace_all(command, |caps: &regex::Captures| {
        // caps[1] 为完整程序名（含 .exe，原始大小写），caps[3] 为组合尾缀
        let prog = &caps[1];
        let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let cleaned = format!("{prog} -n{rest}");
        log::info!("[exec] rg habit fix (command): 'rg -rn{rest}' -> '{cleaned}'");
        cleaned
    })
    .into_owned()
}

pub(super) fn handle_run(ctx: ToolCallCtx) -> ToolResult {
    handle_run_with_shell(ctx, None)
}

/// bash 独立工具：固定 Shell::Bash（Windows 解析 git-for-windows/MSYS2，避开 WSL wrapper）。
pub(super) fn handle_run_bash(ctx: ToolCallCtx) -> ToolResult {
    handle_run_with_shell(ctx, Some(Shell::Bash))
}

/// pwsh 独立工具：固定 Shell::PowerShell（pwsh7 → powershell.exe 降级链）。
pub(super) fn handle_run_pwsh(ctx: ToolCallCtx) -> ToolResult {
    handle_run_with_shell(ctx, Some(Shell::PowerShell))
}

/// 独立 shell 工具的软检测：注册不拒绝，调用时解析路径不可用才报错。
/// 先触发探测缓存（Windows git-bash 标准路径解析、pwsh 降级链），再判可用性。
fn shell_available(shell: Shell) -> bool {
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    let path = shell.path();
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.is_file()
    } else {
        executable_on_path(path)
    }
}

/// 本机可用 shell 清单（软检测报错的引导信息）。
fn available_shells() -> String {
    let mut list = Vec::new();
    for (name, shell) in [
        ("bash", Shell::Bash),
        ("pwsh", Shell::PowerShell),
        ("cmd", Shell::Cmd),
    ] {
        if shell_available(shell) {
            list.push(name);
        }
    }
    if list.is_empty() {
        "none detected".to_string()
    } else {
        list.join(", ")
    }
}

/// 共享执行引擎。`fixed` = 独立 shell 工具（bash/pwsh）固定包装 shell；
/// None = exec 通用入口（`shell` 参数或平台默认检测）。
fn handle_run_with_shell(ctx: ToolCallCtx, fixed: Option<Shell>) -> ToolResult {
    // ── Resolve argv ──
    // Two modes: `command` (auto-wrapped in platform shell) or `argv` (direct exec).
    let argv: Vec<String> = if let Some(command) = ctx.get_str("command") {
        if command.is_empty() {
            return ToolResult::error(crate::json_err(
                "EMPTY_COMMAND",
                "command string is empty",
                "Provide a shell command string.",
            ));
        }
        // ── rg 习惯陷阱防御（grep 迁移）：`rg -rn` → `rg -n` ──
        // `grep -rn`（-r 递归 + -n 行号）是 POSIX 经典组合；ripgrep 中
        // `-r` 是 --replace，`rg -rn "pat"` 会被解析成 `-r n`（把匹配替换成
        // 字面 `n`），输出被污染。在此把 command 字符串中的紧贴组合改写为
        // rg 正确写法（递归默认开启，行号用 -n）。
        let command = normalize_command_rg(command);
        // 固定 shell（bash/pwsh 独立工具）或 exec 的 `shell` 参数/平台默认。
        let shell = match fixed {
            Some(shell) => {
                if !shell_available(shell) {
                    return ToolResult::error(crate::json_err(
                        "SHELL_NOT_FOUND",
                        format!("{} not found on this machine", shell.path()),
                        format!("available shells: {}", available_shells()),
                    ));
                }
                shell
            }
            None => match ctx.args.get("shell").and_then(|v| v.as_str()) {
                Some(name) if !name.is_empty() => match Shell::from_name(name) {
                    Some(shell) => shell,
                    None => {
                        return ToolResult::error(crate::json_err(
                            "UNKNOWN_SHELL",
                            format!("unknown shell '{name}'"),
                            "Use one of: bash, zsh, sh, pwsh, cmd. The default is auto-detected (bash on Windows).",
                        ));
                    }
                },
                _ => Shell::detect(),
            },
        };
        shell.derive_exec_args(&command)
    } else {
        match ctx.args.get("argv").and_then(|v| v.as_array()) {
            Some(arr) => {
                let mut argv: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                // ── rg 习惯陷阱防御（argv 模式）：`rg -rn...` → `rg -n...` ──
                normalize_rg_argv(&mut argv);
                argv
            }
            None => {
                return ToolResult::error(crate::json_err(
                    "MISSING_ARGV",
                    "exec requires argv or command",
                    "Example: {\"argv\": [\"cargo\", \"check\"]} or {\"command\": \"cargo check\"}",
                ));
            }
        }
    };
    if argv.is_empty() {
        return ToolResult::error(crate::json_err(
            "EMPTY_ARGV",
            "argv array is empty",
            "Provide at least one element.",
        ));
    }
    // 默认 token 上限跟随折叠策略：StandardPolicy=10K；NoFoldPolicy（极限模式）
    // = 不截断（u32::MAX，模型显式传 max_output_tokens 时以模型参数为准）。
    let policy_default = crate::tool_side_fold::policy()
        .exec_max_output_tokens()
        .unwrap_or(u32::MAX);
    let max_output_tokens = ctx
        .get_u64("max_output_tokens")
        .filter(|&n| (100..=50000).contains(&n))
        .map(|n| n as u32)
        .unwrap_or(policy_default);
    let timeout_secs = ctx
        .get_u64("timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or_else(|| ctx.timeout_secs.unwrap_or(30).clamp(1, 3600));
    // 快速后台移交窗口：进程存活超过该时长（秒）即返回 backgrounded，
    // 不等 timeout_secs。用于拉起长驻服务（serve/daemon/watch）。
    let background_after_secs = ctx
        .get_u64("background_after_secs")
        .filter(|&n| n > 0 && n <= 3600);
    // Fall back to workspace root when the caller doesn't supply cwd.
    // A relative cwd resolves against the workspace root (or the process
    // directory when no workspace is set) — same semantics as file tools.
    let cwd: Option<String> = ctx
        .get_str("cwd")
        .map(String::from)
        .map(|cwd| {
            let resolved = crate::resolve_workspace_path(&cwd);
            if resolved.is_empty() { cwd } else { resolved }
        })
        .or_else(|| {
            let ws = crate::current_workspace();
            if ws.is_empty() || ws == "." {
                None
            } else {
                Some(ws)
            }
        });
    let cwd_ref: Option<&str> = cwd.as_deref();
    // 可选环境变量覆盖（传入完整 env 供子进程使用）。
    let env: Option<Vec<(String, String)>> = ctx
        .args
        .get("env")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .filter(|pairs: &Vec<(String, String)>| !pairs.is_empty());
    let result = direct_exec(
        &argv,
        env.as_deref(),
        cwd_ref,
        max_output_tokens,
        timeout_secs,
        background_after_secs,
        Some(ctx.cancel.as_ref()),
        ctx.tx_progress.clone(),
        &ctx.id,
    );
    let success = match result.exit_code {
        Some(0) => true,
        Some(_) => false,
        None => !result.timed_out && !result.cancelled,
    };
    let json = result.to_json();
    if success {
        // 极限模式（NoFoldPolicy）：exec/bash/pwsh 输出完全透传，
        // 连 qaqh-types 的 24K 字符硬顶也放开（仍保留 read_stream 字节保护）。
        if crate::tool_side_fold::policy()
            .exec_max_output_tokens()
            .is_none()
        {
            ToolResult::ok_with_limit(json, None)
        } else {
            ToolResult::ok(json)
        }
    } else {
        ToolResult::error(json)
    }
}

// ── Output helpers ──

/// Strip ANSI escape sequences from output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | '_' | '^') => {
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ── Registration ──

use crate::{ToolHandler, ToolPlacement, ToolRisk};
use std::time::Duration;

/// exec / bash / pwsh 共享的 input schema 模板。
/// `with_shell` 控制是否暴露 `shell` 参数（仅 exec 通用入口）。
fn exec_schema(with_shell: bool) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "argv".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "命令数组：argv[0]=可执行文件，argv[1..]=参数；无 shell" }),
    );
    props.insert(
        "command".into(),
        serde_json::json!({ "type": "string", "description": "shell 命令字符串（bash -c / pwsh -Command / cmd /c 包装），用于管道/重定向/一行脚本" }),
    );
    if with_shell {
        props.insert(
            "shell".into(),
            serde_json::json!({ "type": "string", "enum": ["bash", "zsh", "sh", "pwsh", "cmd"], "description": "command 的包装 shell（默认：Windows pwsh、Unix bash）；POSIX 语法选 bash。argv 模式忽略" }),
        );
    }
    props.insert(
        "cwd".into(),
        serde_json::json!({"type": "string", "description": "工作目录（默认 workspace 根；相对路径基于 workspace 解析）"}),
    );
    props.insert(
        "env".into(),
        serde_json::json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "子进程环境变量（可选）；同名覆盖"}),
    );
    props.insert(
        "timeout_secs".into(),
        serde_json::json!({"type": "integer", "description": "超时秒数（1-3600，默认 30）"}),
    );
    props.insert(
        "background_after_secs".into(),
        serde_json::json!({"type": "integer", "description": "快速移交窗口：子进程运行超过该秒数即返回 backgrounded + process_id（用于长驻服务，随后用 process action=check/wait/kill 接管）"}),
    );
    props.insert(
        "max_output_tokens".into(),
        serde_json::json!({ "type": "integer", "description": "输出截断上限（默认 10000，范围 100-50000）" }),
    );
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [],
        "additionalProperties": false,
        "oneOf": [
            {"required": ["argv"]},
            {"required": ["command"]}
        ]
    })
}

/// 独立 shell 工具注册（bash / pwsh）：共享 exec 引擎 + 固定 Shell，
/// schema 无 `shell` 参数；description 注入本机解析路径（软检测）。
fn register_shell_tool(
    mgr: &mut crate::ToolManager,
    key: &str,
    shell: Shell,
    handler: fn(crate::ToolCallCtx) -> crate::ToolResult,
) {
    // 触发探测缓存，让 description 里的解析路径真实（Windows git-bash / pwsh 降级链）。
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    let resolved = shell.path();
    let description = format!(
        "执行命令，固定使用 {key}（{resolved}；本机探测结果，可能随环境变化）。两种模式：(1) argv=[程序, 参数…] 直接执行（无 shell）；(2) command={key} 命令字符串（管道/重定向/一行脚本）。返回 {{status, exit_code, output, wall_time_seconds, timed_out}}；超时移交时返回 backgrounded + process_id。该 shell 不可用时调用报错并列出可用 shell。",
    );
    // ToolHandler.description 是 &'static str：注册仅进程启动一次，leak 即静态。
    let description: &'static str = Box::leak(description.into_boxed_str());
    mgr.register_with_placement(
        ToolHandler {
            key: key.to_string(),
            description,
            input_schema: exec_schema(false),
            handler,
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Exec,
            default_timeout: Duration::from_secs(30),
        },
        ToolPlacement::Workspace,
    );
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "exec".to_string(),
            description: "执行命令。两种模式：(1) argv=[程序, 参数…] 直接执行（无 shell）；(2) command=shell 字符串（自动包装到平台 shell，支持管道/重定向/一行脚本）。返回 {status, exit_code, output, wall_time_seconds, timed_out}；超时移交时返回 backgrounded + process_id。",
            input_schema: exec_schema(true),
            handler: handle_run,
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Exec,
            default_timeout: Duration::from_secs(30),
        },
        ToolPlacement::Workspace,
    );
    // 独立 shell 工具（工具模式白名单可直接引用）。
    register_shell_tool(mgr, "bash", Shell::Bash, handle_run_bash);
    register_shell_tool(mgr, "pwsh", Shell::PowerShell, handle_run_pwsh);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_from_name_resolves_known_shells() {
        assert_eq!(Shell::from_name("pwsh"), Some(Shell::PowerShell));
        assert_eq!(Shell::from_name("powershell"), Some(Shell::PowerShell));
        assert_eq!(Shell::from_name("cmd"), Some(Shell::Cmd));
        assert_eq!(Shell::from_name("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_name("sh"), Some(Shell::Sh));
        assert_eq!(Shell::from_name("bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_name("fish"), None);
        assert_eq!(Shell::from_name(""), None);
    }

    #[test]
    fn rg_habit_fix_argv_mode() {
        // grep 习惯组合 -rn / -rni / -rnl → rg 正确写法（-n 前缀）
        let mut argv = vec!["rg".into(), "-rn".into(), "pattern".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["rg", "-n", "pattern"]);

        let mut argv = vec!["rg".into(), "-rni".into(), "pattern".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["rg", "-ni", "pattern"]);

        let mut argv = vec!["rg.exe".into(), "-rnl".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["rg.exe", "-nl"]);

        // 合法用法不受影响：-r 单独（--replace 等待参数）、长选项、非 rg 程序
        let mut argv = vec!["rg".into(), "-r".into(), "x".into(), "pat".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["rg", "-r", "x", "pat"]);

        let mut argv = vec!["rg".into(), "--replace".into(), "x".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["rg", "--replace", "x"]);

        // grep 的 -rn 是合法组合，不处理
        let mut argv = vec!["grep".into(), "-rn".into(), "pat".into()];
        normalize_rg_argv(&mut argv);
        assert_eq!(argv, vec!["grep", "-rn", "pat"]);
    }

    #[test]
    fn rg_habit_fix_command_mode() {
        // 简单形态
        assert_eq!(
            normalize_command_rg("rg -rn \"pat\" | head"),
            "rg -n \"pat\" | head"
        );
        // 组合变体
        assert_eq!(normalize_command_rg("rg -rni foo"), "rg -ni foo");
        // 管道后的第二个 rg、Windows 可执行名
        assert_eq!(
            normalize_command_rg("rg --files | rg -rn foo"),
            "rg --files | rg -n foo"
        );
        assert_eq!(normalize_command_rg("RG.EXE -rn foo"), "RG.EXE -n foo");
        // 合法用法不受影响
        assert_eq!(normalize_command_rg("rg -r x pat"), "rg -r x pat");
        assert_eq!(
            normalize_command_rg("rg --replace x pat"),
            "rg --replace x pat"
        );
        assert_eq!(normalize_command_rg("grep -rn foo"), "grep -rn foo");
        // 无 rg 调用原样
        assert_eq!(normalize_command_rg("cargo test"), "cargo test");
    }

    #[test]
    fn shell_derive_args_are_shell_specific() {
        // Note: on Windows the bash path may have been resolved to
        // Git-for-Windows by another test (shared DETECTED_BASH_PATH), so
        // only assert the tail of argv[0] and the fixed wrapper arguments.
        let bash = Shell::Bash.derive_exec_args("ls -la");
        assert!(
            bash[0].ends_with("bash") || bash[0].ends_with("bash.exe"),
            "argv[0]={}",
            bash[0]
        );
        assert_eq!(bash[1], "-c");
        assert_eq!(bash[2], "ls -la");

        let pwsh = Shell::PowerShell.derive_exec_args("Get-ChildItem");
        assert_eq!(&pwsh[..3], ["pwsh", "-NoProfile", "-Command"]);
        assert_eq!(pwsh[3], "Get-ChildItem");

        let cmd = Shell::Cmd.derive_exec_args("dir");
        assert_eq!(&cmd[..2], ["cmd", "/c"]);
        assert_eq!(cmd[2], "dir");
    }

    #[test]
    fn test_git_status_returns_output() {
        let argv = vec!["git".to_string(), "status".to_string()];
        let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={}",
            result.exit_code, result.timed_out
        );
        assert!(!result.timed_out, "timed out");
        assert!(!result.output.is_empty(), "no output");
    }

    #[test]
    fn test_git_diff_returns_output() {
        let argv = vec!["git".to_string(), "diff".to_string(), "--stat".to_string()];
        let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={}",
            result.exit_code, result.timed_out
        );
        assert!(!result.timed_out, "timed out");
    }

    #[test]
    fn test_cargo_check_returns_output() {
        let argv = vec![
            "cargo".to_string(),
            "check".to_string(),
            "-p".to_string(),
            "qaqh-types".to_string(),
        ];
        let result = direct_exec(&argv, None, None, 10000, 60, None, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={}",
            result.exit_code, result.timed_out
        );
        assert!(!result.timed_out, "timed out");
        assert!(!result.output.is_empty(), "no output");
    }

    #[cfg(windows)]
    #[test]
    fn per_call_cancel_stops_only_the_running_command() {
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "ping -n 6 127.0.0.1 >NUL".to_string(),
        ];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let result = direct_exec(
            &argv,
            None,
            None,
            100,
            10,
            None,
            Some(cancel.as_ref()),
            None,
            "test",
        );
        assert!(
            result.cancelled,
            "per-call cancellation should stop the child"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn per_call_cancel_stops_only_the_running_command() {
        let argv = vec!["sleep".to_string(), "6".to_string()];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let result = direct_exec(
            &argv,
            None,
            None,
            100,
            10,
            None,
            Some(cancel.as_ref()),
            None,
            "test",
        );
        assert!(
            result.cancelled,
            "per-call cancellation should stop the child"
        );
    }

    #[test]
    fn truncated_output_instructs_the_model_to_retry_narrowly() {
        let text = "token ".repeat(1_000);
        let truncated = token_truncate(&text, 10);
        assert!(truncated.contains("Call exec again with narrower argv or a filtering command."));
    }

    #[test]
    fn pipe_reader_forwards_retained_chunks_with_the_call_id() {
        let (tx, rx) = crate::bounded_exec_progress_channel();
        let (output, truncated) = read_stream(
            std::io::Cursor::new(b"first\nsecond\n".to_vec()),
            1024,
            Some(tx),
            "call-stream-1".to_string(),
            ExecOutputStream::Stdout,
            Arc::new(AtomicU64::new(0)),
            None,
        );

        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(output, b"first\nsecond\n");
        assert!(!truncated);
        assert_eq!(
            chunks,
            vec![ExecProgressEvent {
                tool_call_id: "call-stream-1".to_string(),
                stream: ExecOutputStream::Stdout,
                seq: 0,
                chunk: "first\nsecond\n".to_string(),
            }]
        );
    }

    #[cfg(windows)]
    #[test]
    fn exec_forwards_stdout_to_the_progress_channel_before_returning() {
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo streamed-output".to_string(),
        ];
        let (tx, rx) = crate::bounded_exec_progress_channel();

        let result = direct_exec(
            &argv,
            None,
            None,
            100,
            10,
            None,
            None,
            Some(tx),
            "call-stream-2",
        );
        let chunks: Vec<_> = rx.try_iter().collect();

        assert!(result.output.contains("streamed-output"));
        assert!(chunks.iter().any(|event| {
            event.tool_call_id == "call-stream-2"
                && event.stream == ExecOutputStream::Stdout
                && event.chunk.contains("streamed-output")
        }));
    }

    #[test]
    fn pipe_reader_keeps_split_utf8_characters_intact_for_the_ui() {
        let (tx, rx) = crate::bounded_exec_progress_channel();
        let mut input = vec![b'a'; 8191];
        input.extend_from_slice("中".as_bytes());
        let (_output, truncated) = read_stream(
            std::io::Cursor::new(input),
            16 * 1024,
            Some(tx),
            "utf8".to_string(),
            ExecOutputStream::Stdout,
            Arc::new(AtomicU64::new(0)),
            None,
        );
        assert!(!truncated);
        let text: String = rx.try_iter().map(|event| event.chunk).collect();
        assert!(text.ends_with('中'));
        assert!(!text.contains('\u{fffd}'));
    }

    #[cfg(windows)]
    #[test]
    fn windows_oem_output_is_decoded_without_utf8_beta_mode() {
        // GBK/936 for "正在", representative of cmd.exe ping output.
        assert_eq!(decode_captured(&[0xD5, 0xFD, 0xD4, 0xDA]), "正在");
    }

    #[test]
    fn bounded_progress_queue_drops_updates_without_blocking_pipe_readers() {
        let (tx, _rx) = crate::bounded_exec_progress_channel();
        for seq in 0..=crate::EXEC_PROGRESS_CHANNEL_CAPACITY {
            tx.try_send(ExecProgressEvent {
                tool_call_id: "bounded".to_string(),
                stream: ExecOutputStream::Stdout,
                seq: seq as u64,
                chunk: "x".to_string(),
            });
        }
        assert_eq!(tx.dropped_bytes(), 1);
    }

    #[test]
    fn shell_detect_finds_available_shell() {
        let shell = Shell::detect();
        let path = shell.path();
        // Verify the detected shell binary actually exists
        let status = std::process::Command::new(path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        assert!(
            status.is_ok(),
            "detected shell '{path}' should be runnable (got {:?})",
            shell
        );
    }

    #[test]
    fn command_mode_uses_detected_shell() {
        // 默认检测的 shell（Windows=pwsh / Unix=bash）应可运行 command 模式
        let argv = Shell::detect().derive_exec_args("echo hello-from-shell");
        let result = direct_exec(&argv, None, None, 100, 10, None, None, None, "shell-test");
        assert_eq!(
            result.exit_code,
            Some(0),
            "shell exec failed: {}",
            result.output
        );
        // Should output "hello-from-shell" from the echo command
        assert!(
            result.output.contains("hello-from-shell"),
            "expected 'hello-from-shell' in output, got: '{}'",
            result.output
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_bash_shell_resolves_git_bash() {
        // 模型显式传 shell: bash 时（Windows）应解析到可运行 bash，
        // 且 POSIX 管道语义可用。
        let argv = Shell::from_name("bash")
            .expect("bash name resolves")
            .derive_exec_args("echo posix-ok | tr a-z A-Z");
        let result = direct_exec(&argv, None, None, 100, 10, None, None, None, "bash-test");
        assert_eq!(
            result.exit_code,
            Some(0),
            "bash exec failed: {}",
            result.output
        );
        assert!(
            result.output.contains("POSIX-OK"),
            "bash pipeline output missing marker: {}",
            result.output
        );
    }

    #[test]
    fn shell_discovery_does_not_execute_path_candidates() {
        let root =
            std::env::temp_dir().join(format!("qaqh-exec-shell-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(windows)]
        let candidate = root.join("probe-shell.exe");
        #[cfg(not(windows))]
        let candidate = root.join("probe-shell");
        #[cfg(windows)]
        std::fs::write(&candidate, b"not an executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&candidate, b"#!/bin/sh\n: > \"$0.ran\"\n").unwrap();
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(executable_in_dirs(
            "probe-shell",
            std::iter::once(root.clone())
        ));
        assert!(!root.join("probe-shell.ran").exists());

        let _ = std::fs::remove_file(candidate);
        let _ = std::fs::remove_dir(root);
    }

    #[cfg(windows)]
    #[test]
    fn timeout_transfers_process_to_background_registry() {
        // 8 秒 sleep，超时 3 秒 → 移交后台（不 kill）。
        // 用 PowerShell Start-Sleep（无孙进程，避免句柄继承干扰）。
        let argv = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 8; Write-Output done".to_string(),
        ];
        let result = direct_exec(&argv, None, None, 100, 3, None, None, None, "bg-test");
        assert!(result.timed_out, "应超时");
        assert_eq!(result.status, "backgrounded", "超时 = 移交后台");
        let pid = result.process_id.expect("移交必须携带 process_id");
        // 进程存活于注册表（running）
        let info =
            crate::process_registry::ProcessRegistry::get_info(pid).expect("进程必须在注册表");
        assert_eq!(info["status"], "running", "移交后进程不得被杀");
        assert!(
            result.output.contains("process(action="),
            "提示应指向 process 检查动作"
        );
        // process(wait) 语义：等待自然退出
        let final_info =
            crate::process_registry::ProcessRegistry::wait_for(pid, 15).expect("wait_for 必须返回");
        eprintln!("final_info: {final_info}");
        assert_eq!(final_info["status"], "exited", "ping 自然结束后应为 exited");
        // 输出已逐 chunk 追加到注册表（backgrounded 期间也累积）
        assert!(final_info["output"].is_string() || final_info.get("output_tail").is_some());
    }

    #[cfg(windows)]
    #[test]
    fn backgrounded_process_check_sees_running_then_kill_tree() {
        // cmd /C 生成孙进程树（ping 8 秒）；超时 2 秒移交
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "ping -n 8 127.0.0.1 >NUL".to_string(),
        ];
        let result = direct_exec(&argv, None, None, 100, 2, None, None, None, "bg-kill");
        let pid = result.process_id.expect("process_id");
        assert_eq!(
            crate::process_registry::ProcessRegistry::get_info(pid).unwrap()["status"],
            "running"
        );
        // 注册表 kill = 进程树终止
        assert!(
            crate::process_registry::ProcessRegistry::kill(pid),
            "kill 应成功"
        );
        let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("still tracked");
        assert_eq!(after["status"], "killed");
    }

    #[cfg(windows)]
    #[test]
    fn background_after_secs_handoff_before_timeout() {
        // 长驻进程（8 秒 sleep），timeout 设 60 秒，但 background_after_secs=3
        // → 3 秒即移交后台，而不是死等到 60 秒（验证 agent loop 不阻塞）。
        let argv = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 8; Write-Output done".to_string(),
        ];
        let started = std::time::Instant::now();
        let result = direct_exec(&argv, None, None, 100, 60, Some(3), None, None, "bg-fast");
        let elapsed = started.elapsed().as_secs_f64();
        assert!(result.timed_out, "观察窗口到期应移交");
        assert_eq!(result.status, "backgrounded");
        assert!(
            elapsed < 10.0,
            "移交必须远早于 timeout=60s，实际 {elapsed}s"
        );
        let pid = result.process_id.expect("移交必须携带 process_id");
        let info = crate::process_registry::ProcessRegistry::get_info(pid).expect("in registry");
        assert_eq!(info["status"], "running", "移交后进程存活");
        assert!(
            result.output.contains("transferred_after_secs"),
            "backgrounded 输出应包含移交耗时字段"
        );
        // 清理：等待自然退出（8 秒 sleep 早已结束）
        let final_info =
            crate::process_registry::ProcessRegistry::wait_for(pid, 15).expect("wait_for 必须返回");
        assert_eq!(final_info["status"], "exited");
    }

    #[cfg(windows)]
    #[test]
    fn backgrounded_status_refreshes_when_child_exits_while_grandchild_holds_pipe() {
        // 复现用户场景（cargo test 通过后孙进程未回收）：
        // cmd /C 先 spawn 后台孙进程（ping 6 秒，继承 exec 管道写端），
        // 子进程自身 ping 2 秒后退出。孙进程持有管道 → EOF 永不到达。
        // 修复前：状态停在 running（mark_exited 只在 EOF 后执行），
        // process check/wait 误以为任务未结束。
        // 修复后：try_wait 感知子进程退出即刷新为 exited。
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "start /b cmd /c ping -n 6 127.0.0.1 >NUL & ping -n 2 127.0.0.1 >NUL & exit 0"
                .to_string(),
        ];
        let result = direct_exec(
            &argv,
            None,
            None,
            100,
            15,
            Some(1),
            None,
            None,
            "bg-grandchild",
        );
        assert_eq!(result.status, "backgrounded", "1 秒观察窗到期应移交");
        let pid = result.process_id.expect("process_id");

        // 子进程约 2 秒退出；孙进程（ping 6 秒）继续持有管道
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut status = String::new();
        while std::time::Instant::now() < deadline {
            let _ = crate::process_registry::ProcessRegistry::try_wait(pid);
            status = crate::process_registry::ProcessRegistry::get_info(pid)
                .map(|i| i["status"].as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            if status == "exited" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert_eq!(
            status, "exited",
            "子进程退出后状态必须刷新（不依赖孙进程管道 EOF）"
        );

        // 清理：kill 进程树（孙进程仍活着），验证整树终止
        assert!(
            crate::process_registry::ProcessRegistry::kill(pid),
            "kill 应成功"
        );
        let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("still tracked");
        assert_eq!(after["status"], "killed");
    }

    // ── 独立 shell 工具（4.2：bash / pwsh）──

    fn make_ctx(name: &str, args: serde_json::Value) -> crate::ToolCallCtx {
        crate::ToolCallCtx {
            id: "exec-test".into(),
            name: name.into(),
            action: String::new(),
            args,
            tx_progress: None,
            timeout_secs: Some(30),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn shell_tools_registered_without_shell_param() {
        let mut mgr = crate::ToolManager::new();
        register_shell_tool(&mut mgr, "bash", Shell::Bash, handle_run_bash);
        register_shell_tool(&mut mgr, "pwsh", Shell::PowerShell, handle_run_pwsh);
        let defs = mgr.all_defs();
        assert_eq!(defs.len(), 2);
        for d in &defs {
            let props = d.function.parameters.get("properties").unwrap();
            assert!(
                props.get("shell").is_none(),
                "{} must NOT expose a shell param (tool name is the shell)",
                d.function.name
            );
            assert!(props.get("command").is_some());
            assert!(props.get("argv").is_some());
        }
        let bash_desc = &defs
            .iter()
            .find(|d| d.function.name == "bash")
            .unwrap()
            .function
            .description;
        assert!(bash_desc.contains("bash"), "desc: {bash_desc}");
        let pwsh_desc = &defs
            .iter()
            .find(|d| d.function.name == "pwsh")
            .unwrap()
            .function
            .description;
        assert!(pwsh_desc.contains("pwsh"), "desc: {pwsh_desc}");
    }

    #[test]
    fn bash_tool_executes_command_through_fixed_shell() {
        if !shell_available(Shell::Bash) {
            eprintln!("skipping: bash not available on this machine");
            return;
        }
        // cwd 显式传当前目录：并行测试会污染 CURRENT_WORKSPACE（可能指向已删除
        // 的 tempdir），不传则 spawn 带无效 cwd → os error 267。
        let ctx = make_ctx(
            "bash",
            serde_json::json!({ "command": "echo shell-tool-ok", "cwd": std::env::current_dir().unwrap() }),
        );
        let r = handle_run_bash(ctx);
        assert!(r.is_success(), "model text: {}", r.model_text());
        assert!(r.model_text().contains("shell-tool-ok"));
    }

    #[test]
    fn pwsh_tool_executes_command_through_fixed_shell() {
        if !shell_available(Shell::PowerShell) {
            eprintln!("skipping: powershell not available on this machine");
            return;
        }
        let ctx = make_ctx(
            "pwsh",
            serde_json::json!({ "command": "Write-Output shell-tool-ok", "cwd": std::env::current_dir().unwrap() }),
        );
        let r = handle_run_pwsh(ctx);
        assert!(r.is_success(), "model text: {}", r.model_text());
        assert!(r.model_text().contains("shell-tool-ok"));
    }

    #[test]
    fn bash_tool_argv_mode_still_direct_exec() {
        // argv 模式与 shell 无关：bash 工具也能直跑程序（无包装）。
        #[cfg(windows)]
        let argv = serde_json::json!(["cmd", "/c", "echo", "shell-tool-ok"]);
        #[cfg(not(windows))]
        let argv = serde_json::json!(["echo", "shell-tool-ok"]);
        let ctx = make_ctx(
            "bash",
            serde_json::json!({ "argv": argv, "cwd": std::env::current_dir().unwrap() }),
        );
        let r = handle_run_bash(ctx);
        assert!(r.is_success(), "model text: {}", r.model_text());
    }
}
