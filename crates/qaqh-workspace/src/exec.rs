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
    /// PowerShell 默认走 `-EncodedCommand`（Base64 UTF-16LE），彻底避免引号/中文/特殊字符在
    /// Win32 命令行解析中的转义地狱；stdout/stderr 仍通过管道捕获，编码不影响输出。
    /// 当 `args` 非空且为 PowerShell 时，自动走 `-CommandWithArgs`（7.6 LTS 主流），
    /// 把 `args` 原样作为 CommandParameters 填入 `$args`，避免在脚本内拼接引号。
    /// 目前仅测试使用（生产路径走 `derive_exec_args_with`）；非 test 构建豁免死代码告警。
    #[cfg_attr(not(test), allow(dead_code))]
    fn derive_exec_args(&self, command: &str) -> Vec<String> {
        self.derive_exec_args_with(command, None)
    }

    fn derive_exec_args_with(&self, command: &str, args: Option<&[String]>) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Sh => {
                // bash 暂不消费 args（POSIX 侧可用 `bash -c '... ' _ arg1` 但 Harness 未暴露）
                vec![
                    self.path().to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ]
            }
            Shell::PowerShell => {
                if let Some(a) = args.filter(|a| !a.is_empty()) {
                    // -CommandWithArgs：首参是脚本，后续空格分隔的 CommandParameters 原样进 $args
                    let mut v = vec![
                        self.path().to_string(),
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        "-InputFormat".to_string(),
                        "Text".to_string(),
                        "-OutputFormat".to_string(),
                        "Text".to_string(),
                        "-CommandWithArgs".to_string(),
                        command.to_string(),
                    ];
                    v.extend(a.iter().cloned());
                    v
                } else {
                    let encoded = ps_encode(command);
                    vec![
                        self.path().to_string(),
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        // 强制 Text 格式：-EncodedCommand 默认在管道重定向时会以 CLIXML 序列化 ErrorRecord，
                        // 导致 Harness 捕获的 stderr 变成 XML。显式 Text 保证与 -Command 行为一致。
                        "-InputFormat".to_string(),
                        "Text".to_string(),
                        "-OutputFormat".to_string(),
                        "Text".to_string(),
                        "-EncodedCommand".to_string(),
                        encoded,
                    ]
                }
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

/// PowerShell `-EncodedCommand` 要求的编码：UTF-16LE → Base64（RFC 4648）。
/// 与 `pwsh -EncodedCommand` 文档一致：`[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cmd))`
fn ps_encode(command: &str) -> String {
    let utf16le: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64_encode(&utf16le)
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        if ch.is_whitespace() {
            continue;
        }
        let val = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(format!("invalid base64 char: {ch}")),
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
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

/// 读线程共享上下文（收拢参数列表，亦便于 per-stream 构造）。
struct PipePumpCtx {
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: String,
    output_stream: ExecOutputStream,
    progress_seq: Arc<AtomicU64>,
    registry_id: u32,
}
/// 平台 readiness 探测结果（`drain_pipe_to_registry` 的平台胶水协议）。
/// Empty/Closed 仅在 windows 探测路径构造（unix 走 read→WouldBlock）。
#[cfg_attr(not(windows), allow(dead_code))]
enum Readiness {
    /// 有数据可读（或非阻塞 fd 的 read 即将返回数据/自身报告 Empty）。
    Ready,
    /// 管道当前为空（Windows PeekNamedPipe 为 0；unix 走 read→WouldBlock）。
    Empty,
    /// 管道异常关闭（如 Windows 探测失败）——按读端关闭处理，不得阻塞。
    Closed,
}

/// 有界管道泵（读线程主循环，exec 生命周期重写阶段 2）。
///
/// 职责：把子进程输出解码后逐 chunk 推给 progress 通道（流式 UX），同时
/// 写入注册表（tail 视图 + full 捕获——seal 以 `captured_full` 为权威）。
///
/// 退出条件（任一，读线程永不无限阻塞）：
/// - EOF（`Ok(0)`）——正常路径，子进程退出即达，零额外等待；
/// - 读错误（WouldBlock/Interrupted 除外）；
/// - 字节预算（`max_bytes`）耗尽——超限数据就地丢弃（与旧实现一致），
///   但不再 sink 排空到 EOF：旧实现在此同样可被孙进程卡死；
/// - **settle 到期**：子进程终态（`is_running == false`）后继续排空
///   `READER_SETTLE_BUDGET`，到期即退出——孙进程持有管道写端时读线程
///   必须确定性退出、释放 progress sender（阶段 1 的 1.3 遗留驻留治愈）。
///
/// 返回 (saw_eof, capped)，seal 据此判定 truncated。
fn drain_pipe_to_registry<S: std::io::Read>(
    stream: &mut S,
    max_bytes: usize,
    ctx: &PipePumpCtx,
    readiness: &mut dyn FnMut(&mut S) -> std::io::Result<Readiness>,
) -> (bool, bool) {
    let mut buf = vec![0u8; 8192];
    let mut pending_utf8 = Vec::new();
    let mut captured_bytes = 0usize;
    let mut capped = false;
    let mut saw_eof = false;
    let mut exit_seen: Option<std::time::Instant> = None;
    loop {
        match readiness(stream) {
            Ok(Readiness::Ready) => {}
            Ok(Readiness::Empty) => {
                if child_settled(&mut exit_seen, ctx) {
                    break;
                }
                std::thread::sleep(READER_POLL_TICK);
                continue;
            }
            Ok(Readiness::Closed) => break,
            Err(_) => break,
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(n) => {
                let retained = n.min(max_bytes.saturating_sub(captured_bytes));
                if retained > 0 {
                    forward_progress(
                        &mut pending_utf8,
                        &buf[..retained],
                        ctx.progress_tx.as_ref(),
                        &ctx.tool_call_id,
                        ctx.output_stream,
                        &ctx.progress_seq,
                        Some(ctx.registry_id),
                    );
                    captured_bytes += retained;
                }
                if retained < n {
                    capped = true;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                if child_settled(&mut exit_seen, ctx) {
                    break;
                }
                std::thread::sleep(READER_POLL_TICK);
                continue;
            }
            Err(_) => break,
        }
        if child_settled(&mut exit_seen, ctx) {
            break;
        }
    }
    if !pending_utf8.is_empty() {
        send_progress(
            ctx.progress_tx.as_ref(),
            &ctx.tool_call_id,
            ctx.output_stream,
            &ctx.progress_seq,
            String::from_utf8_lossy(&pending_utf8).into_owned(),
        );
        append_registry(
            ctx.registry_id,
            ctx.output_stream,
            &String::from_utf8_lossy(&pending_utf8),
        );
    }
    (saw_eof, capped)
}

/// 读线程 settle 判定：子进程终态后起算 `READER_SETTLE_BUDGET`，到期 true。
/// status 单调（Running→Exited/Killed 不可逆），再见 Running 即重置计时
/// （防御性；正常时序下不可达）。
fn child_settled(exit_seen: &mut Option<std::time::Instant>, ctx: &PipePumpCtx) -> bool {
    if crate::process_registry::ProcessRegistry::is_running(ctx.registry_id) {
        *exit_seen = None;
        return false;
    }
    let seen = exit_seen.get_or_insert_with(std::time::Instant::now);
    seen.elapsed() >= READER_SETTLE_BUDGET
}

/// 生成读线程：泵循环 + 退出信号（seal 有界 join 的对象）。
/// sender（progress 与 done 信道）随线程结束必然 drop——这是
/// "读线程生命周期有界"的可观测保证。
fn spawn_pipe_reader<S>(
    stream: S,
    max_bytes: usize,
    ctx: PipePumpCtx,
    readiness: impl FnMut(&mut S) -> std::io::Result<Readiness> + Send + 'static,
    done_tx: std::sync::mpsc::Sender<(bool, bool)>,
) where
    S: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut readiness = readiness;
        let outcome = drain_pipe_to_registry(&mut stream, max_bytes, &ctx, &mut readiness);
        let _ = done_tx.send(outcome);
    });
}

/// unix：读端 fd 置非阻塞（poll 化读循环的前提）。
/// O_NONBLOCK 挂在"打开文件描述"上——父进程的读端与子进程的写端是
/// 两个独立描述，互不影响。
#[cfg(unix)]
fn set_pipe_nonblocking<P: std::os::fd::AsRawFd + ?Sized>(pipe: &P) {
    let fd = pipe.as_raw_fd();
    // SAFETY: fcntl on an fd we exclusively own; F_GETFL/F_SETFL are
    // parameterless-in/out queries on that descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: same descriptor; enabling O_NONBLOCK only changes the
        // blocking behaviour of our own read end.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
}

/// windows：PeekNamedPipe 查询可读字节数（匿名管道不支持 O_NONBLOCK，
/// 这是 poll 化读循环的等价物）。None = 探测失败（非管道句柄等），
/// 调用方按 Closed 处理——宁可放弃也不退回无限阻塞读。
#[cfg(windows)]
fn pipe_available_bytes(handle: std::os::windows::io::RawHandle) -> Option<u32> {
    // SAFETY: PeekNamedPipe is a well-known Kernel32 API with stable ABI.
    // We pass null buffers and only request the total-bytes-available
    // counter; the handle comes from std's piped stdio (a valid anonymous
    // pipe handle we exclusively read from).
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            named_pipe: *mut core::ffi::c_void,
            buffer: *mut core::ffi::c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_avail: *mut u32,
            bytes_left_this_message: *mut u32,
        ) -> i32;
    }
    let mut avail: u32 = 0;
    let ok = unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut avail,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

/// seal 侧有界 join：等待读线程退出信号 (saw_eof, capped)。
/// 超时或读线程意外消失（panic → 信道 Disconnected）一律按 (false, false)
/// 处理 → truncated 保守提示；数据本身以 captured_full 为权威，不受影响。
fn wait_reader_done(
    rx: &std::sync::mpsc::Receiver<(bool, bool)>,
    deadline: std::time::Instant,
) -> (bool, bool) {
    let now = std::time::Instant::now();
    if now >= deadline {
        return (false, false);
    }
    match rx.recv_timeout(deadline - now) {
        Ok(pair) => pair,
        Err(_) => (false, false),
    }
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

/// 读线程生命周期（exec 生命周期重写阶段 2，registry-native）。
///
/// - `READER_POLL_TICK`：管道空的轮询粒度（unix 非阻塞 WouldBlock /
///   Windows PeekNamedPipe 为 0 时的重试间隔）。
/// - `READER_SETTLE_BUDGET`：观察到子进程终态后，读线程继续排空在途
///   输出的预算；到期即退出（丢弃后续 chunk，与 drain_bounded 语义对齐），
///   **绝不等待 EOF**——孙进程持有管道写端时读线程必须确定性退出，
///   不再持有 progress sender（阶段 1 的 1.3 遗留驻留问题就此治愈）。
/// - `SEAL_JOIN_BUDGET`：seal 侧对两个读线程退出信号的有界 join 预算，
///   需覆盖"主循环观察到退出（≤50ms）+ settle（300ms）+ 调度余量"。
const READER_POLL_TICK: std::time::Duration = std::time::Duration::from_millis(50);
const READER_SETTLE_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);
const SEAL_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

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
    #[cfg(unix)]
    {
        // H6：独立进程组——取消/超时 kill 时可对整组 SIGKILL，
        // 孙进程不再持有管道写端导致 reader 永不 EOF。
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
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

    // 接线 ProcessRegistry：先注册（读线程捕获 proc_id），take 管道后
    // 再把子进程句柄移入注册表（poll 经 try_wait、超时移交可查）。
    let proc_id = crate::process_registry::ProcessRegistry::register(&display_name);

    // Start bounded pipe readers（registry-native，阶段 2）：
    // - 读线程有界退出（EOF / settle 到期 / 读错误，任一原因发退出信号）；
    // - 输出完整捕获进注册表（tail 视图 + captured_full 权威源）；
    // - progress sender 随线程结束必然 drop（drain 的 Disconnected 快路径）。
    // 旧"汇总信道 + handoff 善后轮询"退役：try_wait 已在任何查询路径
    // 自动置终态，善后块是死代码；汇总数据源被 captured_full 取代。
    let (stdout_done_tx, stdout_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let (stderr_done_tx, stderr_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let progress_seq = Arc::new(AtomicU64::new(0));
    let byte_cap = crate::process_registry::FULL_CAPTURE_BYTE_CAP;
    let stdout_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stdout,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    let stderr_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stderr,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    if let Some(p) = child.stdout.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |_stream: &mut std::process::ChildStdout| Ok(Readiness::Ready),
            stdout_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |stream: &mut std::process::ChildStdout| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(_) => Readiness::Ready,
                    None => Readiness::Closed,
                })
            },
            stdout_done_tx,
        );
    } else {
        let _ = stdout_done_tx.send((true, false));
    }
    if let Some(p) = child.stderr.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |_stream: &mut std::process::ChildStderr| Ok(Readiness::Ready),
            stderr_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |stream: &mut std::process::ChildStderr| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(_) => Readiness::Ready,
                    None => Readiness::Closed,
                })
            },
            stderr_done_tx,
        );
    } else {
        let _ = stderr_done_tx.send((true, false));
    }
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

    // Collect pipe output（P0 去 EOF 化，2026-09-02 冻结事故）：
    // [seal] registry-native（exec 生命周期重写阶段 2，2.1 契约）：
    // 以注册表完整捕获 `captured_full` 为权威——任何路径不得等待管道
    // EOF / 流关闭。对读线程做的是"有界 join"（SEAL_JOIN_BUDGET）：退出
    // 信号在 EOF、settle 到期、读错误时都会发出；正常路径读线程先于
    // seal 完成，首次 recv 立即返回、零额外等待；孙进程持写端路径信号
    // 来自 settle 到期（读线程确定性退出），快照始终完整可用。
    let join_deadline = std::time::Instant::now() + SEAL_JOIN_BUDGET;
    let (stdout_eof, stdout_capped) = wait_reader_done(&stdout_done_rx, join_deadline);
    let (stderr_eof, stderr_capped) = wait_reader_done(&stderr_done_rx, join_deadline);
    let (stdout_out, stderr_out) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .unwrap_or_else(|| {
            // 防御性：seal 紧跟退出执行，条目惰性驱逐（10 分钟终态门槛）
            // 不可能触发；缺失意味着未知的并发破坏——占位并按截断处理。
            log::warn!("[exec] registry full capture missing for process {proc_id}");
            (
                "[WARN] registry full capture missing\n".to_string(),
                String::new(),
            )
        });
    let mut combined = String::new();
    if !stderr_out.is_empty() {
        combined.push_str(&stderr_out);
        if !stdout_out.is_empty() {
            combined.push('\n');
        }
    }
    combined.push_str(&stdout_out);
    // truncated 口径：字节预算耗尽（任一流）或读线程未以 EOF 收尾
    // （settle 放弃 = 孙进程可能继续产出，保守提示输出可能不完整）。
    let hard_trunc = !stdout_eof || !stderr_eof || stdout_capped || stderr_capped;
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
    let shell_command: Option<String> = ctx.get_str("command").map(String::from);
    let argv: Vec<String> = if let Some(command) = shell_command.as_deref() {
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
        // PowerShell 7.6 LTS 新用法：-CommandWithArgs 支持把额外参数原样填入 $args，
        // 避免在脚本字符串内拼接引号。Harness 侧用 `args: string[]` 透传。
        let pwsh_args: Option<Vec<String>> = ctx
            .args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        if pwsh_args.is_some() && shell != Shell::PowerShell {
            return ToolResult::error(crate::json_err(
                "ARGS_NOT_SUPPORTED",
                "args is only supported for pwsh -CommandWithArgs",
                "Use pwsh tool or exec with shell:\"pwsh\" and provide args as string array.",
            ));
        }
        shell.derive_exec_args_with(&command, pwsh_args.as_deref())
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
    let cwd_ref: Option<&str> = cwd.as_deref();
    let mut result = direct_exec(
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
    // 观测线纪律（事故 2026-09-02 预防）：检测 shell 命令中的后台派生 `&`，
    // 以强提示引导走 background_after_secs + process 工具的受控路径。
    if let Some(command) = shell_command.as_deref()
        && result.status == "completed"
        && detect_background_derivation(command)
    {
        result.output.push_str(BACKGROUND_DERIVATION_HINT);
    }
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

/// 后台派生强提示（观测线纪律，fd 持有复现实验 2026-09-06）：裸 `cmd &` 的
/// 孙进程持 fd1/fd2 = 管道写端，孤儿化 reparent 后长期滞留——阶段 2 的
/// settle 机制保证读线程有界退出，但输出完整性仍以重定向到文件 + 受控移交
/// （background_after_secs + process 工具）为正道。
const BACKGROUND_DERIVATION_HINT: &str = "\n[!] 后台派生检测：命令包含 `&`（后台任务）。后台/孙进程可能持有输出管道，导致本结果遗漏其后继输出。长驻服务请改用 background_after_secs 参数移交后台，并以 process 工具（check/wait/kill）接管；后台命令应将 stdout/stderr 重定向到文件（证据：docs/incidents/2026-09-06-fd-hold-repro.md）。\n";

/// 检测 shell 命令中的后台派生操作符。
/// 剥除逻辑与（`&&`）与重定向组合（`>&`/`&>`，覆盖 `2>&1`）后残留的
/// `&` 才是后台派生；引号内的 `&`（sed/awk 等）会误报——提示为建议性
/// 输出，宁滥勿缺。
fn detect_background_derivation(command: &str) -> bool {
    command
        .replace("&&", "")
        .replace(">&", "")
        .replace("&>", "")
        .contains('&')
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
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "argv: [exe,args] no shell" }),
    );
    props.insert(
        "command".into(),
        serde_json::json!({ "type": "string", "description": "Shell command string" }),
    );
    props.insert(
        "args".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "pwsh -CommandWithArgs args" }),
    );
    if with_shell {
        props.insert(
            "shell".into(),
            serde_json::json!({ "type": "string", "enum": ["bash", "zsh", "sh", "pwsh", "cmd"], "description": "Shell for command" }),
        );
    }
    props.insert(
        "cwd".into(),
        serde_json::json!({"type": "string", "description": "Workdir (default workspace root)"}),
    );
    props.insert(
        "env".into(),
        serde_json::json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Env overrides"}),
    );
    props.insert(
        "timeout_secs".into(),
        serde_json::json!({"type": "integer", "description": "Timeout secs (1-3600, default 30)"}),
    );
    props.insert(
        "background_after_secs".into(),
        serde_json::json!({"type": "integer", "description": "Background after secs -> backgrounded+process_id"}),
    );
    props.insert(
        "max_output_tokens".into(),
        serde_json::json!({ "type": "integer", "description": "Max output tokens (10000, 100-50000)" }),
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
    let description = if key == "pwsh" {
        format!(
            "Run command via {key} ({resolved}). Modes: argv|[exe,args] or command|[shell string] (+args for -CommandWithArgs). Returns status/exit_code/output; backgrounded+process_id if timeout."
        )
    } else {
        format!(
            "Run command via {key} ({resolved}). Modes: argv|[exe,args] or command|[shell string]. Returns status/exit_code/output; backgrounded+process_id if timeout."
        )
    };
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
    // exec 已拆分至 bash/pwsh，不再暴露给模型（避免三者鼎立）。
    // bash/pwsh 各自支持 argv 直调（无 shell）与 command(+args) 包装，语义清晰。
    // 内部 handle_run 保留供单测/兼容，但不注册为模型工具。
    register_shell_tool(mgr, "bash", Shell::Bash, handle_run_bash);
    register_shell_tool(mgr, "pwsh", Shell::PowerShell, handle_run_pwsh);
}

/// 兼容保留：仅供单测/旧调用链直接使用，不向模型暴露。
#[allow(dead_code)]
pub fn register_exec_for_compat(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "exec".to_string(),
            description: "[compat] 执行命令。已拆分至 bash/pwsh，保留仅供内部调用。",
            input_schema: exec_schema(true),
            handler: handle_run,
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Exec,
            default_timeout: Duration::from_secs(30),
        },
        ToolPlacement::Workspace,
    );
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
        assert_eq!(
            &pwsh[..11],
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-InputFormat",
                "Text",
                "-OutputFormat",
                "Text",
                "-EncodedCommand"
            ]
        );
        assert_eq!(pwsh.len(), 12);
        // 验证 Base64(UTF-16LE) 可逆且与 ps_encode 一致
        assert_eq!(pwsh[11], ps_encode("Get-ChildItem"));
        let decoded = {
            let bytes = base64_decode(&pwsh[11]).expect("valid base64");
            let utf16: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&utf16).expect("valid utf16le")
        };
        assert_eq!(decoded, "Get-ChildItem");

        let cmd = Shell::Cmd.derive_exec_args("dir");
        assert_eq!(&cmd[..2], ["cmd", "/c"]);
        assert_eq!(cmd[2], "dir");
    }

    #[test]
    fn pwsh_command_with_args_uses_command_with_args() {
        let args = vec![
            "arg1".to_string(),
            "hello world".to_string(),
            "a\"b".to_string(),
        ];
        let pwsh = Shell::PowerShell
            .derive_exec_args_with("Write-Output $args[0]; Write-Output $args[1]", Some(&args));
        assert_eq!(
            &pwsh[..12],
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-InputFormat",
                "Text",
                "-OutputFormat",
                "Text",
                "-CommandWithArgs",
                "Write-Output $args[0]; Write-Output $args[1]"
            ]
        );
        assert_eq!(&pwsh[12..], &args);
        assert_eq!(pwsh.len(), 15);
        // 空 args 应回退到 EncodedCommand
        let pwsh2 = Shell::PowerShell.derive_exec_args_with("Get-ChildItem", Some(&[]));
        assert!(pwsh2.contains(&"-EncodedCommand".to_string()));
        assert!(!pwsh2.contains(&"-CommandWithArgs".to_string()));
    }

    #[test]
    fn pwsh_tool_with_args_executes_via_command_with_args() {
        if !shell_available(Shell::PowerShell) {
            eprintln!("skipping: powershell not available on this machine");
            return;
        }
        let ctx = make_ctx(
            "pwsh",
            serde_json::json!({ "command": "$args | % { \"arg: $_\" }", "args": ["hello world", "a\"b"], "cwd": std::env::current_dir().unwrap() }),
        );
        let r = handle_run_pwsh(ctx);
        assert!(r.is_success(), "model text: {}", r.model_text());
        // model_text 是 ExecOutput 的 JSON，output 字段内才是原始 stdout；需解析后检查
        let v: serde_json::Value = serde_json::from_str(r.model_text()).expect("valid json");
        let output = v.get("output").and_then(|x| x.as_str()).unwrap_or("");
        assert!(output.contains("hello world"), "output: {output}");
        assert!(output.contains("a\"b"), "output: {output}");
    }

    #[test]
    fn pwsh_tool_with_chinese_args_via_command_with_args() {
        if !shell_available(Shell::PowerShell) {
            eprintln!("skipping: powershell not available on this machine");
            return;
        }
        let ctx = make_ctx(
            "pwsh",
            serde_json::json!({ "command": "Write-Output $args[0]", "args": ["中文测试"], "cwd": std::env::current_dir().unwrap() }),
        );
        let r = handle_run_pwsh(ctx);
        assert!(r.is_success(), "model text: {}", r.model_text());
        assert!(
            r.model_text().contains("中文测试"),
            "output: {}",
            r.model_text()
        );
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

    /// 2026-09-02 冻结事故回归（P0-1）：孙进程持有管道写端时，子进程退出后
    /// EOF 永不出现；收集必须在有界预算内以注册表快照完成，不得依赖 EOF。
    /// 旧实现 recv_timeout(2s)×2 在此场景固定多等 ~4s（事故 audit 实测 +2.0s）。
    #[cfg(not(windows))]
    #[test]
    fn grandchild_holding_pipe_write_end_collects_bounded() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 5 & echo GRANDCHILD-HOLDS-PIPE".to_string(),
        ];
        let start = std::time::Instant::now();
        let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
        let elapsed = start.elapsed();
        assert!(
            result.output.contains("GRANDCHILD-HOLDS-PIPE"),
            "registry snapshot must retain child output: {}",
            result.output
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "collect must not wait for grandchild-held pipe EOF, took {elapsed:?}"
        );
    }

    /// 阶段 2（1.3 驻留治愈）回归：孙进程持有管道写端时，读线程必须在
    /// 有界时间内退出并 drop progress sender。旧实现读线程永卡 read()，
    /// sender 永不释放（drain 的 Disconnected 快路径永不触发）。
    #[cfg(not(windows))]
    #[test]
    fn reader_threads_terminate_after_grandchild_settle_even_without_eof() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo settled; sleep 5 &".to_string(),
        ];
        let (tx, rx) = crate::bounded_exec_progress_channel();
        let start = std::time::Instant::now();
        let _result = direct_exec(&argv, None, None, 10000, 10, None, None, Some(tx), "test");
        let deadline = start + std::time::Duration::from_secs(5);
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "读线程必须在 settle 预算内退出（sender drop → Disconnected）"
                    );
                }
            }
        }
    }

    /// 阶段 2（2.1 契约）回归：seal 的权威源是注册表**完整**捕获。
    /// 12000+ 字节输出远超 tail 视图的 4000 字符裁剪线；孙进程持写端时
    /// 旧兜底快照只剩尾部（数据损失），captured_full 必须首尾俱全。
    #[cfg(not(windows))]
    #[test]
    fn seal_uses_full_registry_capture_not_tail_when_grandchild_holds_pipe() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "seq 1 3000; sleep 5 &".to_string(),
        ];
        let result = direct_exec(&argv, None, None, 100000, 10, None, None, None, "test");
        assert!(
            result.output.contains("3000"),
            "末行必须存活: {:?}",
            result.output.get(..200)
        );
        assert!(
            result.output.lines().count() >= 2999,
            "3000 行输出不得被 tail 裁剪，实际 {} 行",
            result.output.lines().count()
        );
    }

    /// Windows 等效回归：`start /b` 在同一控制台派生后台子进程并继承管道写端。
    #[cfg(windows)]
    #[test]
    fn grandchild_holding_pipe_write_end_collects_bounded() {
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "start /b cmd /c \"timeout /t 5 >NUL\" & echo GRANDCHILD-HOLDS-PIPE".to_string(),
        ];
        let start = std::time::Instant::now();
        let result = direct_exec(&argv, None, None, 10000, 10, None, None, None, "test");
        let elapsed = start.elapsed();
        assert!(
            result.output.contains("GRANDCHILD-HOLDS-PIPE"),
            "registry snapshot must retain child output: {}",
            result.output
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "collect must not wait for grandchild-held pipe EOF, took {elapsed:?}"
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
        // registry-native 阶段 2：输出权威源在注册表完整捕获（不再由读线程
        // 返回汇总），读线程只回报 (saw_eof, capped) 生命周期信号。
        let proc_id = crate::process_registry::ProcessRegistry::register("reader-test");
        let mut stream = std::io::Cursor::new(b"first\nsecond\n".to_vec());
        let ctx = PipePumpCtx {
            progress_tx: Some(tx),
            tool_call_id: "call-stream-1".to_string(),
            output_stream: ExecOutputStream::Stdout,
            progress_seq: Arc::new(AtomicU64::new(0)),
            registry_id: proc_id,
        };
        let (saw_eof, capped) =
            drain_pipe_to_registry(&mut stream, 1024, &ctx, &mut |_s: &mut std::io::Cursor<
                Vec<u8>,
            >| {
                Ok(Readiness::Ready)
            });

        let chunks: Vec<_> = rx.try_iter().collect();
        let (full_out, _) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
            .expect("registry entry must exist");
        assert_eq!(full_out, "first\nsecond\n");
        assert!(saw_eof, "Cursor 读尽即 EOF");
        assert!(!capped);
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
        let proc_id = crate::process_registry::ProcessRegistry::register("utf8-reader-test");
        let mut stream = std::io::Cursor::new(input);
        let ctx = PipePumpCtx {
            progress_tx: Some(tx),
            tool_call_id: "utf8".to_string(),
            output_stream: ExecOutputStream::Stdout,
            progress_seq: Arc::new(AtomicU64::new(0)),
            registry_id: proc_id,
        };
        let (saw_eof, capped) = drain_pipe_to_registry(
            &mut stream,
            16 * 1024,
            &ctx,
            &mut |_s: &mut std::io::Cursor<Vec<u8>>| Ok(Readiness::Ready),
        );
        assert!(saw_eof);
        assert!(!capped);
        let (full_out, _) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
            .expect("registry entry must exist");
        assert!(full_out.ends_with('中'));
        assert!(!full_out.contains('\u{fffd}'));
        let text: String = rx.try_iter().map(|event| event.chunk).collect();
        assert!(text.ends_with('中'));
        assert!(!text.contains('\u{fffd}'));
    }

    #[cfg(windows)]
    #[test]
    fn windows_oem_output_is_decoded_without_utf8_beta_mode() {
        // GBK/936 for "正在", representative of cmd.exe ping output.
        assert_eq!(
            decode_windows_oem(&[0xD5, 0xFD, 0xD4, 0xDA]),
            Some("正在".to_string())
        );
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

    /// 阶段 2（报告 P1）回归：process wait 阻塞期间收到取消旗标必须立即
    /// 返回，不得阻塞到 timeout_secs（非 exec 阻塞工具的飞行中取消）。
    #[test]
    fn wait_for_returns_promptly_on_per_call_cancel() {
        let id = crate::process_registry::ProcessRegistry::register("wait-cancel-test");
        // 无 child 的条目永远 Running——旧实现会在此阻塞满 timeout_secs。
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let start = std::time::Instant::now();
        let info = crate::process_registry::ProcessRegistry::wait_for(id, 30, Some(&cancel))
            .expect("wait_for 必须返回");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "取消必须立即打破 wait_for 阻塞"
        );
        assert_eq!(info["wait_interrupted_by_cancel"], serde_json::json!(true));
    }

    /// Linux 侧 3.2 冒烟：工具层全生命周期（真 handler 调用，非 direct_exec 直调）。
    /// 前台执行 → background_after_secs 快速移交 → 注册表 check → kill → 终态。
    /// 验证的是 bash 工具 → handle_run_with_shell → direct_exec → registry 的完整链路。
    #[cfg(not(windows))]
    #[test]
    fn bash_tool_full_lifecycle_smoke_foreground_handoff_kill() {
        if !shell_available(Shell::Bash) {
            eprintln!("skipping: bash not available");
            return;
        }
        let cwd = std::env::current_dir().unwrap();
        // ① 前台：echo 经完整 tool 层（spawn → poll → seal → 汇聚）
        let ctx = make_ctx(
            "bash",
            serde_json::json!({ "command": "echo SMOKE-FOREGROUND-OK", "cwd": cwd }),
        );
        let r = handle_run_bash(ctx);
        assert!(r.is_success(), "foreground: {}", r.model_text());
        assert!(r.model_text().contains("SMOKE-FOREGROUND-OK"));

        // ② 快速移交：30s 长任务 + 1s 观察窗 → backgrounded + process_id
        let ctx = make_ctx(
            "bash",
            serde_json::json!({ "command": "sleep 30", "cwd": cwd, "background_after_secs": 1 }),
        );
        let r = handle_run_bash(ctx);
        let v: serde_json::Value =
            serde_json::from_str(r.model_text()).expect("bash 工具结果必须是 ExecOutput JSON");
        assert_eq!(v["status"], "backgrounded", "移交状态: {v}");
        let pid = v["process_id"].as_u64().expect("移交必须携带 process_id") as u32;

        // ③ check：running（try_wait 刷新终态，不依赖管道 EOF）
        let info = crate::process_registry::ProcessRegistry::get_info(pid)
            .expect("移交后条目必须在注册表");
        assert_eq!(info["status"], "running");

        // ④ kill 整树 + 终态收敛（killpg 组杀：bash 与 sleep 同组）
        assert!(
            crate::process_registry::ProcessRegistry::kill(pid),
            "kill 应成功"
        );
        let after = crate::process_registry::ProcessRegistry::get_info(pid).expect("仍被跟踪");
        assert_eq!(after["status"], "killed");

        // ⑤ wait_for 对 Killed 终态立即返回（有界，不空等到 timeout）
        let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 10, None)
            .expect("wait_for 必须返回");
        assert_eq!(final_info["status"], "killed");
    }

    /// 观测线纪律：后台派生检测的判定口径。
    #[test]
    fn background_derivation_detection_boundaries() {
        assert!(detect_background_derivation("nohup ./x run > log 2>&1 &"));
        assert!(detect_background_derivation("sleep 30 &"));
        assert!(detect_background_derivation("a & b"));
        // 逻辑与、重定向组合不得误报
        assert!(!detect_background_derivation("cargo test && cargo clippy"));
        assert!(!detect_background_derivation("cmd >f 2>&1"));
        assert!(!detect_background_derivation("cmd &>f"));
        assert!(!detect_background_derivation("echo ok"));
    }

    /// 工具层 e2e：后台派生命令的前台结果携带强提示（模型可见）。
    /// `sleep 1 &` 同时覆盖"孙进程持写端 → settle 有界封口"路径。
    #[cfg(not(windows))]
    #[test]
    fn bash_tool_appends_background_derivation_hint() {
        if !shell_available(Shell::Bash) {
            eprintln!("skipping: bash not available");
            return;
        }
        let ctx = make_ctx(
            "bash",
            serde_json::json!({
                "command": "sleep 1 & echo HINT-E2E",
                "cwd": std::env::current_dir().unwrap()
            }),
        );
        let r = handle_run_bash(ctx);
        assert!(r.is_success(), "result: {}", r.model_text());
        assert!(r.model_text().contains("HINT-E2E"));
        assert!(
            r.model_text().contains("[!] 后台派生检测"),
            "强提示必须随结果输出"
        );
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
        let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 15, None)
            .expect("wait_for 必须返回");
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
        let final_info = crate::process_registry::ProcessRegistry::wait_for(pid, 15, None)
            .expect("wait_for 必须返回");
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
