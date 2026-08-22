//! Windows → WSL `/mnt` 路径转换（serve 侧专用）。
//!
//! 当 workspace 后端跑在 WSL（Linux）里时，supervisor 下发的是 Windows 路径
//! （`F:\QAQ-Harness`），但 serve 进程内 `std::path::Path` 在 Linux 上**不把 `\`
//! 当作分隔符**，直接 `cd F:\...` / `Path::new("F:\\x")` 都会把整条当作单个
//! 非法段。因此 serve 侧必须用**字符串解析**（而非 `Path` 组件）把 Windows
//! 绝对盘符路径转成 `/mnt/{drive}/...`。
//!
//! 该函数本体无平台 cfg，纯字符串逻辑，可在 Windows 测试套件里直接验证；
//! 是否启用转换由调用点用 `#[cfg(target_os = "linux")]` 门控（worker 进程跑在
//! Windows 上，天然不触发；serve 跑在 WSL/Linux 上才转换）。

/// 将 Windows 绝对盘符路径转换为 WSL `/mnt` 路径。
///
/// 支持：`F:\QAQ-Harness`、`F:/QAQ-Harness/sub`、`\\?\F:\QAQ-Harness`、`F:\`。
/// 拒绝：UNC 网络路径（`\\server\share`、`\\?\UNC\...`）、已是 Linux/相对路径。
///
/// # Examples
/// ```
/// use qaqh_workspace::wsl_path::windows_to_mnt;
/// assert_eq!(windows_to_mnt(r"F:\QAQ-Harness"), Some("/mnt/f/QAQ-Harness".into()));
/// assert_eq!(windows_to_mnt(r"\\?\F:\QAQ-Harness"), Some("/mnt/f/QAQ-Harness".into()));
/// assert_eq!(windows_to_mnt(r"F:/a b/c"), Some("/mnt/f/a b/c".into()));
/// assert_eq!(windows_to_mnt(r"F:\"), Some("/mnt/f".into()));
/// assert_eq!(windows_to_mnt(r"\\server\share\x"), None);
/// assert_eq!(windows_to_mnt("/home/user/proj"), None);
/// assert_eq!(windows_to_mnt("relative/path"), None);
/// ```
pub fn windows_to_mnt(path: &str) -> Option<String> {
    // 去掉 Windows 设备路径前缀 `\\?\`（也接受 `/` 形式的 `//?/`）。
    let p = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix("//?/"))
        .unwrap_or(path);

    // `\\?\UNC\server\share` —— 网络共享，拒绝。
    if p.starts_with("UNC\\") || p.starts_with("UNC/") {
        return None;
    }

    let bytes = p.as_bytes();
    // 盘符前缀：单个字母 + `:`（`F:`），后跟分隔符或路径体。
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        // 前两字节已验明为 ASCII 盘符前缀，split_at 恒安全。
        let rest = &p.split_at(2).1;
        let mut out = format!("/mnt/{drive}");
        for seg in rest.split(['\\', '/']) {
            if !seg.is_empty() {
                out.push('/');
                out.push_str(seg);
            }
        }
        return Some(out);
    }

    // UNC 网络路径 `\\server\share` / `//server/share` —— 拒绝。
    if p.starts_with(r"\\") || p.starts_with("//") {
        return None;
    }

    None
}

/// 若 serve 运行在 Linux/WSL 上，把可能来自 worker 的 Windows 绝对路径
/// 归一化为 `/mnt` 形式；否则（Windows worker 进程内）原样返回。
pub fn platform_workspace_path(path: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(mnt) = windows_to_mnt(path) {
            return mnt;
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::windows_to_mnt;

    #[test]
    fn converts_drive_backslash() {
        assert_eq!(
            windows_to_mnt(r"F:\QAQ-Harness"),
            Some("/mnt/f/QAQ-Harness".into())
        );
        assert_eq!(
            windows_to_mnt(r"D:\project\QAQ-Harness\crates\foo"),
            Some("/mnt/d/project/QAQ-Harness/crates/foo".into())
        );
    }

    #[test]
    fn converts_forward_slash_and_spaces() {
        assert_eq!(windows_to_mnt("F:/a b/c"), Some("/mnt/f/a b/c".into()));
        assert_eq!(
            windows_to_mnt(r"F:\中文 路径\x"),
            Some("/mnt/f/中文 路径/x".into())
        );
    }

    #[test]
    fn converts_verbatim_prefix() {
        assert_eq!(
            windows_to_mnt(r"\\?\F:\QAQ-Harness"),
            Some("/mnt/f/QAQ-Harness".into())
        );
        assert_eq!(
            windows_to_mnt(r"//?/F:/QAQ-Harness"),
            Some("/mnt/f/QAQ-Harness".into())
        );
    }

    #[test]
    fn converts_drive_root_only() {
        assert_eq!(windows_to_mnt(r"F:\"), Some("/mnt/f".into()));
        assert_eq!(windows_to_mnt("F:"), Some("/mnt/f".into()));
    }

    #[test]
    fn rejects_unc() {
        assert_eq!(windows_to_mnt(r"\\server\share\x"), None);
        assert_eq!(windows_to_mnt(r"\\?\UNC\server\share"), None);
    }

    #[test]
    fn rejects_non_windows() {
        assert_eq!(windows_to_mnt("/home/user/proj"), None);
        assert_eq!(windows_to_mnt("relative/path"), None);
        assert_eq!(windows_to_mnt(""), None);
    }

    #[test]
    fn keeps_dotdot_components() {
        // 保留 `..`，由下游文件系统正常解析（/mnt 支持）。
        assert_eq!(
            windows_to_mnt(r"F:\QAQ-Harness\..\foo"),
            Some("/mnt/f/QAQ-Harness/../foo".into())
        );
    }
}
