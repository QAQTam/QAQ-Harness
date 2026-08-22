//! webUI 静态托管：daemon 直接服务前端产物（浏览器直连入口）。
//!
//! 动机（决策记录 2026-07-31）：Electron/Tauri 本质是浏览器，前端 renderer
//! 是纯静态产物。开发/调试循环不应依赖壳打包（改前端 → 浏览器刷新即可）。
//! daemon 以 HTTP 静态服务直接托管 `out/renderer`，浏览器打开
//! `http://127.0.0.1:<port>/debug/` 即获得与桌面壳 renderer 相同的前端，
//! 并内联当前 token 供 Ringing V1 HTTP/SSE 直连——页面与桌面壳走同一
//! 协议面（open 协商 / 三频道命令 / SSE / timeline），daemon 侧无任何
//! 第二前端协议。这也是多前端的正式形态：任何能讲 Ringing V1 的壳都能接入。
//!
//! 安全边界（loopback 信任模型）：
//! - **仅限本机回环连接**：非 loopback 来源一律 403。token 已明文存在于
//!   `~/.deepx/daemon.json`，本页内联 token 与既有威胁模型一致；LAN server
//!   模式下远端壳是持有 token 的原生应用，无需也不得从本端点获取 token；
//! - 静态服务仅限 renderer 目录内（防目录穿越）；
//! - 只读：无命令、无写端点。切流等写操作仍走带 lease 的 Ringing 端点。

use std::path::{Component, Path, PathBuf};

use tokio::net::TcpStream;

use crate::http::{read_request, stringify, write_response_no_cache};
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

/// GET /debug/... 静态服务 + token 注入（仅限 loopback 来源）。
pub async fn handle_debug_http(mut stream: TcpStream, token: &str) -> Result<(), String> {
    // LAN server 模式（0.0.0.0）下，本端点的桥脚本会内联真实 token——
    // 必须拒绝非回环来源，否则等于向整个局域网发放 daemon 凭据。
    let peer_is_loopback = stream
        .peer_addr()
        .map(|addr| addr.ip().is_loopback())
        .unwrap_or(false);
    if !peer_is_loopback {
        return write_response_no_cache(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"webUI hosting is restricted to loopback connections",
        )
        .await;
    }

    // 与 Ringing 端点共用完整请求解析（消费 header + body；此前仅凭
    // server 分流的 peek 首行嗅探，无法区分 GET 细节与残留数据）。
    let request = read_request(&mut stream).await?;
    if request.method != "GET" {
        return write_response_no_cache(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only",
        )
        .await;
    }
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if !path.starts_with("/debug") {
        return write_response_no_cache(&mut stream, "404 Not Found", "text/plain", b"not found")
            .await;
    }

    // 桥配置 JS（CSP 兼容：index.html 注入的是 `<script src>` 同源外部脚本，
    // 不触发 script-src 'self' 之外的 inline 限制）。
    if path == "/debug/__qaqh_bridge__.js" {
        let body = format!(
            "window.__QAQH_DEBUG__={{\"token\":\"{token}\",\"nonce\":\"{}\"}};\n",
            random_hex()
        );
        return write_response_no_cache(
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
        return write_response_no_cache(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"invalid path",
        )
        .await;
    };

    if !file.exists() || !file.is_file() {
        return write_response_no_cache(&mut stream, "404 Not Found", "text/plain", b"not found")
            .await;
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
        return write_response_no_cache(&mut stream, "200 OK", mime, html.as_bytes()).await;
    }

    write_response_no_cache(&mut stream, "200 OK", mime, &bytes).await
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
}
