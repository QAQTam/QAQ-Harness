//! Debug 只读页：daemon 直接服务前端产物（浏览器调试入口）。
//!
//! 动机（决策记录 2026-07-31）：Electron 本质是浏览器，前端 renderer 是纯静态
//! 产物。开发/调试循环不应依赖 Electron 打包（改前端 → 浏览器刷新即可），
//! 也不应为了替换 asar 而终止桌面应用。daemon 以 HTTP 静态服务直接托管
//! `out/renderer`，浏览器打开 `http://127.0.0.1:<port>/debug/` 即获得与
//! Electron renderer 相同的前端，并内联当前 token 供 WebSocket/SSE 直连。
//!
//! 安全边界（loopback 信任模型）：
//! - daemon 只绑定 127.0.0.1；token 已明文存在于 `~/.deepx/daemon.json`，
//!   本页内联 token 与既有威胁模型一致；
//! - 静态服务仅限 renderer 目录内（防目录穿越）；
//! - 只读：无命令、无写端点。切流等写操作仍走带 lease 的 Ringing 端点。

use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::server::random_hex;

/// renderer 产物根目录（dev 调试）。定位优先级：
/// 1. `QAQH_DEBUG_RENDERER_DIR` 环境变量；
/// 2. 当前工作目录下 `out/renderer`（electron-vite 默认输出）；
/// 3. 可执行文件旁 `out/renderer`——生产布局：daemon 在
///    `<install>/resources/qaqh-daemon.exe`，renderer 复制到
///    `<install>/resources/out/renderer`（asar 外部，静态服务可直接读）。
pub fn renderer_root() -> PathBuf {
    if let Ok(dir) = std::env::var("QAQH_DEBUG_RENDERER_DIR") {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let candidates = [
        cwd.join("out").join("renderer"),
        cwd.join("resources").join("out").join("renderer"),
        exe_dir.join("out").join("renderer"),
    ];
    for c in &candidates {
        if c.join("index.html").exists() {
            return c.clone();
        }
    }
    candidates[0].clone()
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 规范化 URL 路径并限制在 root 内（防目录穿越）。
fn safe_join(root: &Path, url_path: &str) -> Option<PathBuf> {
    let decoded = url_path.replace("%20", " ").replace("%2E", ".");
    let mut parts = Vec::new();
    for comp in Path::new(&decoded).components() {
        match comp {
            Component::Normal(seg) => parts.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let mut joined = root.to_path_buf();
    for seg in parts {
        joined.push(seg);
    }
    // 规范化后再校验一次前缀（防御符号链接/重复分隔符）。
    // Windows canonicalize 返回 `\\?\` UNC 形式：字符串级剥离该前缀后
    // 双方比较（Path::strip_prefix 对 UNC 前缀在 Windows 语义下不可靠）。
    // 不存在的文件 canonicalize 失败 → 原路径参与比较，由调用方按 404 处理。
    fn strip_unc(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s.to_string())
    }
    let canonical_root = strip_unc(&root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let canonical_joined = strip_unc(&joined.canonicalize().unwrap_or_else(|_| joined));
    if !canonical_joined.starts_with(&canonical_root) {
        return None;
    }
    Some(canonical_joined)
}

/// 从 preview 提取 GET 路径（只服务 GET）。
fn get_path(preview: &str) -> Option<&str> {
    let first = preview.lines().next()?;
    let mut parts = first.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let path = parts.next()?;
    // 去掉 query string
    Some(path.split('?').next().unwrap_or(path))
}

/// GET /debug/... 静态服务 + token 注入。
pub async fn handle_debug_http(
    mut stream: TcpStream,
    preview: &str,
    token: &str,
) -> Result<(), String> {
    let Some(path) = get_path(preview) else {
        return write_static(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only",
        )
        .await;
    };
    if !path.starts_with("/debug") {
        return write_static(
            &mut stream,
            "404 Not Found",
            "text/plain",
            b"not a debug path",
        )
        .await;
    }

    // 桥配置 JS（CSP 兼容：index.html 注入的是 `<script src>` 同源外部脚本，
    // 不触发 script-src 'self' 之外的 inline 限制）。
    if path == "/debug/__qaqh_bridge__.js" {
        let body = format!(
            "window.__QAQH_DEBUG__={{\"token\":\"{token}\",\"nonce\":\"{}\"}};\n",
            random_hex()
        );
        return write_static(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            body.as_bytes(),
        )
        .await;
    }

    let root = renderer_root();
    let rel = path.trim_start_matches("/debug").trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let Some(file) = safe_join(&root, rel) else {
        return write_static(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"invalid path",
        )
        .await;
    };

    if !file.exists() || !file.is_file() {
        return write_static(&mut stream, "404 Not Found", "text/plain", b"not found").await;
    }
    let bytes = tokio::fs::read(&file).await.map_err(stringify)?;
    let mime = mime_for(&file);

    // SPA 入口页注入桥脚本标签（仅 index.html）。
    // 用 `<script src>` 而非内联脚本：CSP `script-src 'self'` 允许同源
    // 外部脚本，内联脚本会被 CSP 直接阻止（2026-07-31 实测）。
    if file.file_name().and_then(|n| n.to_str()) == Some("index.html") {
        let mut html = String::from_utf8_lossy(&bytes).into_owned();
        let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
        if let Some(idx) = html.find("</head>") {
            html.insert_str(idx, script);
        } else {
            html.push_str(script);
        }
        return write_static(&mut stream, "200 OK", mime, html.as_bytes()).await;
    }

    write_static(&mut stream, "200 OK", mime, &bytes).await
}

async fn write_static(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.write_all(body).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;
    // 优雅关闭写方向后，读空接收缓冲（server.rs peek 残留的请求数据）。
    // Windows 上直接 close 带未读数据的 socket 会发 RST，客户端收不全 body。
    stream.shutdown().await.map_err(stringify)?;
    let mut sink = [0_u8; 4096];
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        stream.read(&mut sink),
    )
    .await;
    Ok(())
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_allows_files_inside_root() {
        let root = PathBuf::from(std::env::temp_dir()).join("debug-http-test-root");
        std::fs::create_dir_all(root.join("assets")).expect("create");
        std::fs::write(root.join("assets").join("a.js"), b"x").expect("write");
        let joined = safe_join(&root, "assets/a.js").expect("inside root");
        assert!(joined.ends_with("a.js"));
        // 不存在的文件也放行（调用方按 404 处理）
        assert!(safe_join(&root, "nope.js").is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let root = PathBuf::from(std::env::temp_dir()).join("debug-http-test-root2");
        std::fs::create_dir_all(&root).expect("create");
        assert!(safe_join(&root, "../outside").is_none());
        assert!(safe_join(&root, "a/../../outside").is_none());
        assert!(safe_join(&root, "/etc/passwd").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn get_path_extracts_get_only() {
        assert_eq!(get_path("GET /debug/ HTTP/1.1\r\nHost: x"), Some("/debug/"));
        assert_eq!(get_path("GET /debug/?a=1 HTTP/1.1\r\n"), Some("/debug/"));
        assert_eq!(get_path("POST /debug/ HTTP/1.1\r\n"), None);
        assert_eq!(get_path(""), None);
    }
}
