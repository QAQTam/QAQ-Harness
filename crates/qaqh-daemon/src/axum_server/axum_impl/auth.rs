//! axum_impl::auth — see parent module docs.

use super::*;

pub(crate) fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {token}"))
}

pub(crate) fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

pub(crate) fn lease_required_json() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        br#"{"code":"lease_required","message":"open a Ringing v1 client session first"}"#
            .as_slice(),
    )
        .into_response()
}

pub(crate) fn get_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-qaqh-client-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn parse_channel(s: &str) -> Option<RingingChannel> {
    match s {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

pub(crate) fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
    if !close_seed.is_empty() {
        close_seed.to_string()
    } else {
        envelope_seed.clone().unwrap_or_default()
    }
}

pub(crate) fn publish_session_created(hub: &RingingHub, seed: &str, command_id: &str) {
    let _ = hub.publish_with_causation(
        seed,
        qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
            seed: seed.to_string(),
            state: qaqh_domain::SessionState::Created,
        }),
        Some(command_id),
    );
}
