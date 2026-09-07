//! axum_impl — daemon HTTP 层（Ringing V1 + 服务面 + 调试/控制）。
//!
//! 由单文件 `axum_server.rs` 拆分（Phase 2-4）：`mod axum_impl` 内联模块解体为
//! 目录模块，对外 API 不变（`AppState` + `build_router`）。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Component, Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};

use qaqh_domain::{ControlCommand, RingingChannel};
use qaqh_ringing::{
    ClientOpenRequest, ClientOpenResponse, RINGING_SCHEMA, RINGING_VERSION, RingingCommandAck,
    RingingCommandAckStatus, RingingCommandEnvelope, RingingCommandState, RingingResetRequired,
};
use qaqh_runtime::ringing::{PendingCommandStore, RingingLeaseStore, service_methods};
use qaqh_runtime::{QaqhService, RingingHub};

use crate::server::random_hex;

use qaqh_runtime::ringing::hydrate_attachment_previews;

pub mod auth;
pub mod command;
pub mod content;
pub mod debug_control;
pub mod service_api;
pub mod sse;
pub mod timeline_api;

pub(crate) use auth::{
    get_session_id, is_authorized, lease_required_json, parse_channel, publish_session_created,
    session_close_seed, unauthorized,
};
pub(crate) use command::{handle_command, handle_command_status, handle_open, handle_renew};
pub(crate) use content::{handle_content_get, handle_content_upload};
pub(crate) use debug_control::{
    activity, handle_debug, handle_debug_bridge, handle_debug_index, handle_stop,
    handle_stop_if_idle, health, loopback_guard, not_found,
};
pub(crate) use service_api::handle_service;
pub(crate) use sse::{handle_events, handle_timeline_events};
#[cfg(test)]
pub(crate) use sse::{parse_sse_cursor, parse_timeline_cursor};
#[cfg(test)]
pub(crate) use timeline_api::paginate_turns;
pub(crate) use timeline_api::{handle_bootstrap, handle_timeline_snapshot};

const RENEW_TTL_MS: u64 = 30_000;
const RENEW_INTERVAL_MS: u64 = 10_000;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 128;
const TIMELINE_PAGE_LIMIT: usize = 30;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<RingingHub>,
    pub leases: Arc<Mutex<RingingLeaseStore>>,
    pub pending: Arc<Mutex<PendingCommandStore>>,
    pub service: QaqhService,
    pub token: String,
    pub epoch: String,
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    pub before_turn: Option<String>,
    pub limit: Option<usize>,
}

struct JsonResponse(Vec<u8>);
impl IntoResponse for JsonResponse {
    fn into_response(self) -> Response {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            self.0,
        )
            .into_response()
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/activity", get(activity))
        .route("/ringing/v1/clients/open", post(handle_open))
        .route("/ringing/v1/leases/renew", post(handle_renew))
        .route(
            "/ringing/v1/commands/{id}",
            post(handle_command).get(handle_command_status),
        )
        .route(
            "/ringing/v1/sessions/{seed}/bootstrap",
            get(handle_bootstrap),
        )
        .route(
            "/ringing/v1/sessions/{seed}/timeline",
            get(handle_timeline_snapshot),
        )
        .route("/ringing/v1/content/{content_id}", get(handle_content_get))
        .route("/ringing/v1/content", post(handle_content_upload))
        .route("/ringing/v1/service/{method}", post(handle_service))
        .route("/ringing/v1/events/{channel}", get(handle_events))
        .route(
            "/ringing/v1/sessions/{seed}/timeline/events",
            get(handle_timeline_events),
        )
        .route("/control/v1/stop", post(handle_stop))
        .route("/control/v1/stop-if-idle", post(handle_stop_if_idle))
        .route("/debug/__qaqh_bridge__.js", get(handle_debug_bridge))
        .route("/debug", get(handle_debug_index))
        .route("/debug/", get(handle_debug_index))
        .route("/debug/{*path}", get(handle_debug))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(loopback_guard))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod pure_tests {
    use super::*;
    #[test]
    fn channel_parsing() {
        assert_eq!(parse_channel("control"), Some(RingingChannel::Control));
        assert_eq!(
            parse_channel("conversation"),
            Some(RingingChannel::Conversation)
        );
        assert_eq!(parse_channel("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel("bogus"), None);
    }
    #[test]
    fn sse_cursor_parsing() {
        assert_eq!(
            parse_sse_cursor("epoch-1:tool:42", "epoch-1", RingingChannel::Tool),
            42
        );
        assert_eq!(
            parse_sse_cursor("epoch-2:tool:42", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("epoch-1:conversation:7", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("garbage", "epoch-1", RingingChannel::Tool),
            0
        );
    }
    #[test]
    fn timeline_cursor_is_separate() {
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:42", "epoch-1"), 42);
        assert_eq!(parse_timeline_cursor("epoch-1:tool:42", "epoch-1"), 0);
        assert_eq!(parse_timeline_cursor("epoch-2:timeline:42", "epoch-1"), 0);
        assert_eq!(
            parse_timeline_cursor("epoch-1:timeline:42:extra", "epoch-1"),
            0
        );
    }
    fn paged_turns(n: usize) -> Vec<qaqh_domain::TimelineTurn> {
        (1..=n)
            .map(|i| qaqh_domain::TimelineTurn {
                turn_id: format!("t{i}"),
                created_seq: i as u64,
                user_text: format!("q{i}"),
                sealed: true,
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
                rounds: vec![],
            })
            .collect()
    }
    #[test]
    fn timeline_pagination_first_page_is_tail_window() {
        let (page, has_more) = paginate_turns(paged_turns(40), None, 30);
        assert_eq!(page.len(), 30);
        assert_eq!(page.first().unwrap().turn_id, "t11");
        assert_eq!(page.last().unwrap().turn_id, "t40");
        assert!(has_more);
    }
    #[test]
    fn timeline_pagination_short_session_has_no_more() {
        let (page, has_more) = paginate_turns(paged_turns(10), None, 30);
        assert_eq!(page.len(), 10);
        assert!(!has_more);
    }
    #[test]
    fn timeline_pagination_before_turn_fetches_earlier_page() {
        let (page, has_more) = paginate_turns(paged_turns(40), Some("t11"), 10);
        assert_eq!(page.len(), 10);
        assert_eq!(page.first().unwrap().turn_id, "t1");
        assert_eq!(page.last().unwrap().turn_id, "t10");
        assert!(!has_more);
    }
    #[test]
    fn timeline_pagination_before_turn_mid_page_and_unknown_fallback() {
        let (page, has_more) = paginate_turns(paged_turns(40), Some("t21"), 10);
        assert_eq!(page.first().unwrap().turn_id, "t11");
        assert_eq!(page.last().unwrap().turn_id, "t20");
        assert!(has_more);
        let (page, _) = paginate_turns(paged_turns(40), Some("t-unknown"), 10);
        assert_eq!(page.last().unwrap().turn_id, "t40");
        let (page, has_more) = paginate_turns(vec![], Some("t1"), 10);
        assert!(page.is_empty());
        assert!(!has_more);
    }
    #[test]
    fn session_close_seed_resolution_prefers_command_seed() {
        assert_eq!(
            session_close_seed("s-command", &Some("s-envelope".into())),
            "s-command"
        );
        assert_eq!(session_close_seed("s-command", &None), "s-command");
        assert_eq!(
            session_close_seed("", &Some("s-envelope".into())),
            "s-envelope"
        );
        assert_eq!(session_close_seed("", &None), "");
    }
    #[test]
    fn session_create_event_carries_command_causation() {
        let hub = RingingHub::new("epoch-1");
        publish_session_created(&hub, "s-created", "cmd-create");
        let replay = hub.replay_channel_since(RingingChannel::Control, 0, false);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].seed, "s-created");
        assert_eq!(replay.events[0].causation_id.as_deref(), Some("cmd-create"));
        assert!(matches!(
            &replay.events[0].event,
            qaqh_ringing::RingingEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                state: qaqh_domain::SessionState::Created,
                ..
            })
        ));
    }
}
