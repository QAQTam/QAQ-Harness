//! 远端 daemon 路径的显示层转换（临时跨端模式）。
//!
//! 规则：
//! - daemon 侧永远使用它自己的绝对路径（Linux 为 `/home/user/...`，Windows 为 `C:\...`）；
//! - Windows 壳只做**字符串显示**：`//<ip>/home/user/...`（或 `//<ip>/C:/...`），
//!   不把它当成本地文件系统路径使用；
//! - 提交回 daemon 时剥掉 `//<ip>` 前缀，还原成 daemon 侧路径。
//!
//! 注意：这不是 UNC 挂载语义，只是显示与输入之间的双向文本映射。

/// 把 daemon 侧路径转成 `//ip/path` 显示形式。
///
/// `host` 可传裸 IP（`192.168.1.10`）、`ip:port` 或完整 `http(s)://...` URL，
/// 函数会剥掉 scheme。空 host 时退化为直接返回原路径。
pub fn display_path(host: &str, remote_path: &str) -> String {
    let host = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        return remote_path.to_string();
    }
    let path = remote_path.trim_start_matches('/');
    format!("//{host}/{path}")
}

/// 把用户输入/显示形式转回 daemon 侧路径。
///
/// 接受三种输入：
/// - `//192.168.1.10/home/user/...` → `/home/user/...`（显示形式）
/// - `/home/user/...` → 原样（已经是 daemon 侧路径）
/// - Windows 风格盘符路径 `C:\...` / `C:/...` → 原样（daemon 在 Windows 的场景）
///
/// 无法识别的形式返回 `None`（例如相对路径）。
pub fn remote_path_from_display(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("//") {
        // `//ip/home/...` → 取第一段斜杠之后的部分；`//ip` 后没有路径则视为根。
        let body = rest
            .split_once('/')
            .map(|(_, body)| body)
            .unwrap_or_default();
        if body.is_empty() {
            return Some("/".to_string());
        }
        // Windows daemon 的盘符路径（`//ip/C:/work`）不补前导斜杠。
        let bytes = body.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Some(body.replace('\\', "/"));
        }
        let path = format!("/{body}");
        return Some(path);
    }
    if trimmed.starts_with('/') {
        return Some(trimmed.to_string());
    }
    // Windows 盘符（`C:\`、`C:/`）或 UNC 输入直接透传。
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0].is_ascii_alphabetic()) {
        return Some(trimmed.replace('\\', "/"));
    }
    None
}

/// 从 base_url 提取 `//ip` 显示用的 host 部分（去 scheme，保留端口）。
pub fn display_host(base_url: &str) -> &str {
    base_url
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_uses_double_slash_ip_form() {
        assert_eq!(
            display_path("192.168.1.10", "/home/alice/proj"),
            "//192.168.1.10/home/alice/proj"
        );
        assert_eq!(
            display_path("http://192.168.1.10:64413", "/home/alice"),
            "//192.168.1.10:64413/home/alice"
        );
    }

    #[test]
    fn display_path_handles_root_and_empty_host() {
        assert_eq!(display_path("10.0.0.2", "/"), "//10.0.0.2/");
        assert_eq!(display_path("", "/home/a"), "/home/a");
    }

    #[test]
    fn from_display_strips_authority() {
        assert_eq!(
            remote_path_from_display("//192.168.1.10/home/alice/proj"),
            Some("/home/alice/proj".into())
        );
        assert_eq!(
            remote_path_from_display("//192.168.1.10/"),
            Some("/".into())
        );
    }

    #[test]
    fn from_display_passes_through_daemon_paths() {
        assert_eq!(
            remote_path_from_display("/home/alice"),
            Some("/home/alice".into())
        );
        assert_eq!(remote_path_from_display("C:\\work"), Some("C:/work".into()));
        assert_eq!(remote_path_from_display("rel/path"), None);
    }

    #[test]
    fn drive_letter_display_round_trips() {
        let shown = display_path("10.0.0.2", "C:/work");
        assert_eq!(shown, "//10.0.0.2/C:/work");
        assert_eq!(remote_path_from_display(&shown), Some("C:/work".into()));
    }
}
