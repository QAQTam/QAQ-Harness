//! exec::shell — 平台 shell 探测与 argv 派生（Shell/detect/path/derive_exec_args_with + base64/ps_encode）。

use std::sync::OnceLock;

// ── Platform shell detection ──
// Adapted from codex-rs/shell-command/src/shell_detect.rs & core/src/shell.rs.
// Stripped to the minimum needed: pick the right shell, derive argv.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
#[allow(clippy::enum_variant_names)] // 变体名 PowerShell 含枚举名，属既有命名
pub(crate) enum Shell {
    Bash,
    Zsh,
    Sh,
    PowerShell,
    Cmd,
}

pub(crate) static DETECTED_SHELL: OnceLock<Shell> = OnceLock::new();
/// Full path to bash on Windows — avoids the WSL wrapper at System32\\bash.exe.
pub(crate) static DETECTED_BASH_PATH: OnceLock<String> = OnceLock::new();
/// Full path to PowerShell on Windows（pwsh 7 优先，powershell.exe 兜底）。
pub(crate) static DETECTED_PWSH_PATH: OnceLock<String> = OnceLock::new();

impl Shell {
    /// Auto-detect the best available shell on this platform.
    pub(crate) fn detect() -> Self {
        *DETECTED_SHELL.get_or_init(Self::detect_uncached)
    }

    /// Resolve an explicit shell name requested by the model (exec `shell`
    /// parameter). Windows `bash` resolves to Git-for-Windows / MSYS2 when
    /// present, avoiding the WSL wrapper. Unknown names fall back to None so
    /// the caller can report a clean error.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
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

    pub(crate) fn detect_uncached() -> Self {
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
    pub(crate) fn path(&self) -> &str {
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
    pub(crate) fn derive_exec_args_with(
        &self,
        command: &str,
        args: Option<&[String]>,
    ) -> Vec<String> {
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
pub(crate) fn ps_encode(command: &str) -> String {
    let utf16le: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64_encode(&utf16le)
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
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
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
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

pub(crate) fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    executable_in_dirs(name, std::env::split_paths(&path))
}

pub(crate) fn executable_in_dirs(
    name: &str,
    dirs: impl IntoIterator<Item = std::path::PathBuf>,
) -> bool {
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
    let candidates = [name.to_string()];

    dirs.into_iter().any(|dir| {
        candidates
            .iter()
            .any(|candidate| is_executable_file(&dir.join(candidate)))
    })
}

/// Find `bash` on Windows PATH, skipping known WSL wrapper locations
/// (System32, WindowsApps). Returns the full path on success.
#[cfg(windows)]
pub(crate) fn find_bash_on_path() -> Option<String> {
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

pub(crate) fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    true
}
