//! axum_impl::service_api — see parent module docs.

use super::*;

/// `POST /ringing/v1/service/{method}` — 服务面 RPC（Read/Write 两类，
/// 方法表见 `qaqh_runtime::ringing::service_methods`）。旧
/// `/queries/{name}` 与 `/actions/{name}` 双端点已并入此处。
pub(crate) async fn handle_service(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    // 单一规范形态 `module.method`：slash 别名已拆除，查不到即 404。
    let Some(info) = service_methods::lookup(name.trim_matches('/')) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"unknown_method","message":"unknown service method"}"#.to_vec(),
        )
            .into_response();
    };
    let params: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(
                        &serde_json::json!({"code":"invalid_body","message":format!("{e}")}),
                    )
                    .unwrap_or_default(),
                )
                    .into_response();
            }
        }
    };
    if info.requires_seed && params.get("seed").and_then(|v| v.as_str()).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"invalid_envelope","message":"seed is required"}"#.to_vec(),
        )
            .into_response();
    }
    // 任何带 seed 的请求：seed 必须归属本 lease。
    if let Some(seed) = params.get("seed").and_then(|v| v.as_str()) {
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, seed);
        if !owns {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required","message":"attach the session seed before calling"}"#
                    .to_vec(),
            )
                .into_response();
        }
    }
    let method = name.trim_matches('/');
    match service_methods::dispatch(&state.service, method, &params) {
        Ok(value) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&value).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&service_methods::error_response(info.kind, &e)).unwrap_or_default(),
        )
            .into_response(),
    }
}

// ---- SSE helpers (P2) ----
