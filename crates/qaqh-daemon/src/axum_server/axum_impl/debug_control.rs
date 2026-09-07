//! axum_impl::debug_control — see parent module docs.

use super::*;

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "webui-dist"]
struct WebUi;

pub(crate) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        format!("ok epoch={} token_len={}", state.epoch, state.token.len()),
    )
}

/// 只读活动快照（冻结事故 P0 观测项）：暴露 has_active_work 与逐会话
/// 活动状态，冻结会话可直接从外部探测。与 /health 同级免鉴权，仅含
/// seed/state/turn_id/seq/updated_at，无用户内容。
pub(crate) async fn activity(State(state): State<AppState>) -> impl IntoResponse {
    let (has_active_work, activities) = state.service.activity_snapshot();
    let body = serde_json::json!({
        "has_active_work": has_active_work,
        "activities": activities,
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

pub(crate) async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

pub(crate) fn renderer_root() -> PathBuf {
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

pub(crate) fn mime_for(path: &StdPath) -> &'static str {
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

pub(crate) fn safe_join(root: &StdPath, url_path: &str) -> Option<PathBuf> {
    let decoded = url_path.replace("%20", " ").replace("%2E", ".");
    let mut parts = Vec::new();
    for comp in StdPath::new(&decoded).components() {
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
    fn strip_unc(p: &StdPath) -> PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s.to_string())
    }
    let canonical_root = strip_unc(&root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let canonical_joined = strip_unc(&joined.canonicalize().unwrap_or(joined));
    if !canonical_joined.starts_with(&canonical_root) {
        return None;
    }
    Some(canonical_joined)
}

pub(crate) async fn handle_debug_bridge(State(state): State<AppState>) -> Response {
    let body = format!(
        "window.__QAQH_DEBUG__={{\"token\":\"{}\",\"nonce\":\"{}\"}};\n",
        state.token,
        random_hex()
    );
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn handle_debug(
    State(_state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let rel = if path.is_empty() { "index.html" } else { &path };
    // 优先尝试编译时嵌入的产物（单文件分发），缺失时回退到文件系统（dev 实时构建）
    let decoded_rel = rel.replace("%20", " ").replace("%2E", ".");
    if !decoded_rel.contains("..") && !decoded_rel.starts_with('/') {
        if let Some(embedded) = WebUi::get(&decoded_rel) {
            let data = embedded.data;
            let mime = mime_for(StdPath::new(&decoded_rel));
            // index.html 注入桥脚本（与文件系统路径一致，CSP 兼容）
            let is_index = decoded_rel == "index.html" || decoded_rel.ends_with("/index.html");
            if is_index {
                let mut html = String::from_utf8_lossy(&data).into_owned();
                let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
                if let Some(idx) = html.find("</head>") {
                    html.insert_str(idx, script);
                } else {
                    html.push_str(script);
                }
                return (
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    html,
                )
                    .into_response();
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                data.into_owned(),
            )
                .into_response();
        }
        // 尝试嵌入的 index.html 作为 SPA 回退（前端路由）——仅当 rel 非文件且嵌入存在
        if WebUi::get(&decoded_rel).is_none()
            && !decoded_rel.contains('.')
            && let Some(embedded) = WebUi::get("index.html")
        {
            let data = embedded.data;
            let mime = mime_for(StdPath::new("index.html"));
            let mut html = String::from_utf8_lossy(&data).into_owned();
            let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
            if let Some(idx) = html.find("</head>") {
                html.insert_str(idx, script);
            } else {
                html.push_str(script);
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                html,
            )
                .into_response();
        }
    }
    let root = renderer_root();
    let Some(file) = safe_join(&root, rel) else {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-cache")],
            "invalid path",
        )
            .into_response();
    };
    if !file.exists() || !file.is_file() {
        // 文件系统缺失 → 仅对 SPA 路由（无扩展名）回退到嵌入 index.html
        if !decoded_rel.contains('.')
            && let Some(embedded) = WebUi::get("index.html")
        {
            let data = embedded.data;
            let mime = mime_for(StdPath::new("index.html"));
            let mut html = String::from_utf8_lossy(&data).into_owned();
            let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
            if let Some(idx) = html.find("</head>") {
                html.insert_str(idx, script);
            } else {
                html.push_str(script);
            }
            return (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                html,
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "not found",
        )
            .into_response();
    }
    let bytes = match tokio::fs::read(&file).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mime = mime_for(&file);
    if file.file_name().and_then(|n| n.to_str()) == Some("index.html") {
        let mut html = String::from_utf8_lossy(&bytes).into_owned();
        let script = "<script src=\"./__qaqh_bridge__.js\"></script>";
        if let Some(idx) = html.find("</head>") {
            html.insert_str(idx, script);
        } else {
            html.push_str(script);
        }
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            html,
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) async fn handle_debug_index(State(state): State<AppState>) -> Response {
    handle_debug(State(state), Path("index.html".to_string())).await
}

pub(crate) async fn loopback_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path().starts_with("/debug")
        && let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>().cloned()
        && !addr.ip().is_loopback()
    {
        return (
            StatusCode::FORBIDDEN,
            [
                (header::CONTENT_TYPE, "text/plain"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            "webUI hosting is restricted to loopback connections",
        )
            .into_response();
    }
    next.run(req).await
}

pub(crate) async fn handle_stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    // Windows 95 semantics: seal before 200.
    // D-4：shutdown_all 对每个 worker 阻塞 join，放 spawn_blocking。
    let service = state.service.clone();
    let _ = tokio::task::spawn_blocking(move || service.shutdown()).await;
    state.hub.seal_all_orphans();
    state.hub.flush_timeline_persistence();
    let _ = state.shutdown.send(true);
    (StatusCode::OK, "").into_response()
}

pub(crate) async fn handle_stop_if_idle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    if state.service.has_active_work() {
        return (StatusCode::CONFLICT, "").into_response();
    }
    // D-4：同 handle_stop。
    let service = state.service.clone();
    let _ = tokio::task::spawn_blocking(move || service.shutdown()).await;
    state.hub.seal_all_orphans();
    state.hub.flush_timeline_persistence();
    let _ = state.shutdown.send(true);
    (StatusCode::OK, "").into_response()
}
