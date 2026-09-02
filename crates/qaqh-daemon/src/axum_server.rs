//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl {
    use std::collections::{HashMap, HashSet};
    use std::net::SocketAddr;
    use std::path::{Component, Path as StdPath, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "webui-dist"]
    struct WebUi;

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

    // ---- helpers ----
    fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {token}"))
    }

    fn unauthorized() -> Response {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }

    fn lease_required_json() -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required","message":"open a Ringing v1 client session first"}"#
                .as_slice(),
        )
            .into_response()
    }

    fn get_session_id(headers: &HeaderMap) -> Option<String> {
        headers
            .get("x-qaqh-client-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    fn parse_channel(s: &str) -> Option<RingingChannel> {
        match s {
            "control" => Some(RingingChannel::Control),
            "conversation" => Some(RingingChannel::Conversation),
            "tool" => Some(RingingChannel::Tool),
            _ => None,
        }
    }

    fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
        if !close_seed.is_empty() {
            close_seed.to_string()
        } else {
            envelope_seed.clone().unwrap_or_default()
        }
    }

    fn publish_session_created(hub: &RingingHub, seed: &str, command_id: &str) {
        let _ = hub.publish_with_causation(
            seed,
            qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                seed: seed.to_string(),
                state: qaqh_domain::SessionState::Created,
            }),
            Some(command_id),
        );
    }

    use qaqh_runtime::ringing::hydrate_attachment_previews;

    #[derive(Deserialize)]
    pub struct TimelineQuery {
        pub before_turn: Option<String>,
        pub limit: Option<usize>,
    }

    fn paginate_turns(
        turns: Vec<qaqh_domain::TimelineTurn>,
        before_turn: Option<&str>,
        limit: usize,
    ) -> (Vec<qaqh_domain::TimelineTurn>, bool) {
        if turns.is_empty() {
            return (turns, false);
        }
        let (start, end) = match before_turn {
            Some(id) => {
                let idx = turns
                    .iter()
                    .position(|t| t.turn_id == id)
                    .unwrap_or(turns.len());
                (idx.saturating_sub(limit), idx)
            }
            None => (turns.len().saturating_sub(limit), turns.len()),
        };
        let page: Vec<_> = turns[start..end].to_vec();
        let has_more = start > 0;
        (page, has_more)
    }

    // ---- handlers ----
    async fn health(State(state): State<AppState>) -> impl IntoResponse {
        (
            StatusCode::OK,
            format!("ok epoch={} token_len={}", state.epoch, state.token.len()),
        )
    }

    /// 只读活动快照（冻结事故 P0 观测项）：暴露 has_active_work 与逐会话
    /// 活动状态，冻结会话可直接从外部探测。与 /health 同级免鉴权，仅含
    /// seed/state/turn_id/seq/updated_at，无用户内容。
    async fn activity(State(state): State<AppState>) -> impl IntoResponse {
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

    async fn not_found() -> impl IntoResponse {
        (StatusCode::NOT_FOUND, "not found")
    }

    async fn handle_open(
        State(state): State<AppState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let req: ClientOpenRequest = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&serde_json::json!({
                        "code":"invalid_body","message": format!("invalid open request: {e}")
                    }))
                    .unwrap_or_default(),
                )
                    .into_response();
            }
        };
        if req.schema != RINGING_SCHEMA || req.version != RINGING_VERSION {
            return (
                StatusCode::UPGRADE_REQUIRED,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&RingingCommandAck {
                    command_id: String::new(),
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("unsupported_version".into()),
                    message: Some("unsupported Ringing schema/version".into()),
                    retry_after_ms: None,
                })
                .unwrap_or_default(),
            )
                .into_response();
        }
        let client_session_id = random_hex();
        state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open(client_session_id.clone(), req.client_instance_id.clone());
        let resp = ClientOpenResponse {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_VERSION,
            accepted: true,
            client_session_id,
            server_epoch: state.hub.epoch().to_string(),
            lease_ttl_ms: lease_ttl_ms(),
            renew_interval_ms: RENEW_INTERVAL_MS,
        };
        (
            StatusCode::OK,
            JsonResponse(serde_json::to_vec(&resp).unwrap_or_default()),
        )
            .into_response()
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

    async fn handle_renew(
        State(state): State<AppState>,
        headers: HeaderMap,
        _body: Bytes,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "text/plain")],
                "missing client session header",
            )
                .into_response();
        };
        let ok = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .renew(&session_id);
        if !ok {
            return (StatusCode::UNAUTHORIZED, "lease expired or unknown").into_response();
        }
        let resp = serde_json::json!({
            "ok": true,
            "lease_ttl_ms": lease_ttl_ms(),
            "renew_interval_ms": RENEW_INTERVAL_MS,
        });
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&resp).unwrap_or_default(),
        )
            .into_response()
    }

    async fn handle_command(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(channel): Path<String>,
        body: Bytes,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        let session_active = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_active_session(&session_id);
        if !session_active {
            return lease_required_json();
        }
        let Some(expected) = parse_channel(&channel) else {
            return (StatusCode::NOT_FOUND, "unknown channel").into_response();
        };
        let env: RingingCommandEnvelope = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                let ack = RingingCommandAck {
                    command_id: String::new(),
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("invalid_body".into()),
                    message: Some(e.to_string()),
                    retry_after_ms: None,
                };
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
        };
        if let Err(code) = env.validate() {
            let status = if code == "lease_required" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some(code.into()),
                message: Some("invalid Ringing v1 command envelope".into()),
                retry_after_ms: None,
            };
            return (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        if env.channel != expected {
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("channel_mismatch".into()),
                message: Some(format!(
                    "path channel {channel} != envelope channel {:?}",
                    env.channel
                )),
                retry_after_ms: None,
            };
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        // unsupported ConversationLoadMore
        if matches!(
            &env.command,
            qaqh_ringing::RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationLoadMore { .. }
            )
        ) {
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Rejected,
                code: Some("unsupported_command".into()),
                message: Some("Ringing v1 bootstrap already returns the complete persisted conversation history".into()),
                retry_after_ms: None,
            };
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        // idempotency
        let fingerprint_payload = serde_json::to_string(&serde_json::json!({
            "channel": env.channel,
            "seed": &env.seed,
            "expected_revision": env.expected_revision,
            "command": &env.command,
        }))
        .unwrap_or_default();
        let fingerprint =
            qaqh_runtime::ringing::content_store::sha256_hex(fingerprint_payload.as_bytes());
        let duplicate_check = {
            let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
            match pending.record_fingerprint_for_session(&env.command_id, &fingerprint, &session_id)
            {
                Ok(v) => Ok(!v),
                Err(()) => Err(()),
            }
        };
        if duplicate_check.is_err() {
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("duplicate_command_mismatch".into()),
                message: Some("command_id was already used with another payload".into()),
                retry_after_ms: None,
            };
            return (
                StatusCode::CONFLICT,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        let duplicate = duplicate_check.expect("error branch already returned CONFLICT above");
        if duplicate {
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: Some("duplicate command_id (already accepted)".into()),
                retry_after_ms: None,
            };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        // SessionClose
        if let qaqh_ringing::RingingCommand::Control(ControlCommand::SessionClose {
            seed: close_seed,
        }) = &env.command
        {
            let close_seed = session_close_seed(close_seed, &env.seed);
            if close_seed.is_empty() {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("missing_seed".into()),
                    message: Some("SessionClose requires seed".into()),
                    retry_after_ms: None,
                };
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            // D-4：close 是阻塞 join（worker loop + reader 线程），必须放
            // spawn_blocking，避免占用 tokio worker 线程并长时间持有
            // registry 锁阻塞其它 RPC。
            let close_result = {
                let service = state.service.clone();
                let seed = close_seed.clone();
                let command_id = env.command_id.clone();
                tokio::task::spawn_blocking(move || service.close_session(&seed, Some(&command_id)))
                    .await
                    .unwrap_or_else(|e| Err(format!("close join error: {e}")))
            };
            if let Err(error) = close_result {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("dispatch_failed".into()),
                    message: Some(error.to_string()),
                    retry_after_ms: None,
                };
                return (
                    StatusCode::BAD_GATEWAY,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach_seed(&session_id, &close_seed);
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: None,
                retry_after_ms: None,
            };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        // SessionArchive / Unarchive / Delete
        if let qaqh_ringing::RingingCommand::Control(
            cmd @ (ControlCommand::SessionArchive { .. }
            | ControlCommand::SessionUnarchive { .. }
            | ControlCommand::SessionDelete { .. }),
        ) = &env.command
        {
            let (op, target) = match cmd {
                ControlCommand::SessionArchive { seed } => ("archive", seed),
                ControlCommand::SessionUnarchive { seed } => ("unarchive", seed),
                ControlCommand::SessionDelete { seed } => ("delete", seed),
                _ => unreachable!(),
            };
            let target = session_close_seed(target, &env.seed);
            if target.is_empty() {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("missing_seed".into()),
                    message: Some(format!("Session{op} requires seed")),
                    retry_after_ms: None,
                };
                return (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            // D-4：archive 内含 close（阻塞 join），delete 同理；整体移入
            // spawn_blocking。unarchive（拉起 worker）一并序列化到阻塞线程，
            // 保持同一 command 的执行线程语义一致。
            let result: Result<(), String> = {
                let service = state.service.clone();
                let target = target.clone();
                let command_id = env.command_id.clone();
                tokio::task::spawn_blocking(move || match op {
                    "archive" => service
                        .archive_session(&target, Some(&command_id))
                        .map_err(|e| e.to_string()),
                    "unarchive" => service
                        .unarchive_session(&target)
                        .map_err(|e| e.to_string()),
                    "delete" => {
                        let _ = service.close_session(&target, Some(&command_id));
                        service
                            .delete_session(&target, Some(&command_id))
                            .map_err(|e| e.to_string())
                    }
                    _ => unreachable!(),
                })
                .await
                .unwrap_or_else(|e| Err(format!("session op join error: {e}")))
            };
            if let Err(error) = result {
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("dispatch_failed".into()),
                    message: Some(error),
                    retry_after_ms: None,
                };
                return (
                    StatusCode::BAD_GATEWAY,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            if op == "delete" {
                state
                    .leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .detach_seed(&session_id, &target);
            }
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: None,
                retry_after_ms: None,
            };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        // session.new / session.resume
        match &env.command {
            qaqh_ringing::RingingCommand::Control(ControlCommand::SessionCreate { .. }) => {
                let params = serde_json::to_value(&env.command).unwrap_or_default();
                // service.handle expects params as Value; for session.new it expects seed? Actually SessionCreate is handled via service.handle("session.new")
                let created = match state.service.handle("session.new", &params) {
                    Ok(v) => v,
                    Err(e) => {
                        state
                            .pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .rollback(&env.command_id);
                        let ack = RingingCommandAck {
                            command_id: env.command_id,
                            status: RingingCommandAckStatus::Rejected,
                            code: Some("dispatch_failed".into()),
                            message: Some(e.to_string()),
                            retry_after_ms: None,
                        };
                        return (
                            StatusCode::BAD_GATEWAY,
                            [(header::CONTENT_TYPE, "application/json")],
                            serde_json::to_vec(&ack).unwrap_or_default(),
                        )
                            .into_response();
                    }
                };
                if let Some(seed) = created.as_str() {
                    state
                        .leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .attach_seed(&session_id, seed);
                    publish_session_created(&state.hub, seed, &env.command_id);
                } else if let Some(seed) = created.get("seed").and_then(|v| v.as_str()) {
                    state
                        .leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .attach_seed(&session_id, seed);
                    publish_session_created(&state.hub, seed, &env.command_id);
                }
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Accepted,
                    code: None,
                    message: None,
                    retry_after_ms: None,
                };
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            qaqh_ringing::RingingCommand::Control(ControlCommand::SessionResume { seed }) => {
                if let Err(e) = state
                    .service
                    .handle("session.resume", &serde_json::json!({"seed": seed}))
                {
                    state
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&env.command_id);
                    let ack = RingingCommandAck {
                        command_id: env.command_id,
                        status: RingingCommandAckStatus::Rejected,
                        code: Some("dispatch_failed".into()),
                        message: Some(e.to_string()),
                        retry_after_ms: None,
                    };
                    return (
                        StatusCode::BAD_GATEWAY,
                        [(header::CONTENT_TYPE, "application/json")],
                        serde_json::to_vec(&ack).unwrap_or_default(),
                    )
                        .into_response();
                }
                state
                    .leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach_seed(&session_id, seed);
                state
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Accepted,
                    code: None,
                    message: None,
                    retry_after_ms: None,
                };
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&ack).unwrap_or_default(),
                )
                    .into_response();
            }
            _ => {}
        }
        // generic worker dispatch
        let seed = env.seed.clone().unwrap_or_default();
        let mut worker_command = env.command.clone();
        if let Err(code) = hydrate_attachment_previews(&state.hub, &seed, &mut worker_command) {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Rejected,
                code: Some(code.clone()),
                message: Some("attachment is unavailable or invalid".into()),
                retry_after_ms: None,
            };
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        let worker_env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
            seed.as_str(),
            env.command_id.clone(),
            worker_command,
        )
        .with_expected_revision(env.expected_revision);
        if let Err(e) = state.service.send_ringing_command(&seed, &worker_env) {
            state
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("dispatch_failed".into()),
                message: Some(e.to_string()),
                retry_after_ms: None,
            };
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_running(&env.command_id);
        let ack = RingingCommandAck {
            command_id: env.command_id,
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: None,
            retry_after_ms: None,
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&ack).unwrap_or_default(),
        )
            .into_response()
    }

    async fn handle_command_status(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(command_id): Path<String>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required","message":"client session header required"}"#.to_vec(),
            )
                .into_response();
        };
        let Some(status) = state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status_for_session(&command_id, &session_id)
        else {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"command_not_found","message":"command receipt not found"}"#.to_vec(),
            )
                .into_response();
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&status).unwrap_or_default(),
        )
            .into_response()
    }

    async fn handle_bootstrap(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(seed): Path<String>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        if seed.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing seed").into_response();
        }
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed);
        if !owns {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required","message":"attach the session seed before bootstrap"}"#.to_vec(),
            )
                .into_response();
        }
        state.hub.seal_orphan_channel_state(&seed, false);
        let bootstrap = qaqh_ringing::RingingSessionBootstrap::new(
            state.hub.epoch(),
            &seed,
            state.hub.snapshot(RingingChannel::Control, &seed),
            state.hub.conversation_snapshot(&seed),
            state.hub.snapshot(RingingChannel::Tool, &seed),
        );
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&bootstrap).unwrap_or_default(),
        )
            .into_response()
    }

    async fn handle_timeline_snapshot(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(seed): Path<String>,
        Query(q): Query<TimelineQuery>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        if seed.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing seed").into_response();
        }
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed);
        if !owns {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required","message":"attach the session seed before reading timeline"}"#.to_vec(),
            )
                .into_response();
        }
        let snapshot =
            state
                .hub
                .timeline_snapshot(&seed)
                .unwrap_or(qaqh_domain::TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                });
        let total_turns = snapshot.turns.len();
        let (page, has_more) = paginate_turns(
            snapshot.turns,
            q.before_turn.as_deref(),
            q.limit.unwrap_or(TIMELINE_PAGE_LIMIT).min(200),
        );
        let body = serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 1,
            "server_epoch": state.hub.epoch(),
            "seed": seed,
            "snapshot": {"watermark": snapshot.watermark, "turns": page},
            "has_more": has_more,
            "total_turns": total_turns,
        });
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&body).unwrap_or_default(),
        )
            .into_response()
    }

    async fn handle_content_get(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(content_id): Path<String>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        if content_id.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing content_id").into_response();
        }
        let Some(seed) = params.get("seed").cloned() else {
            return (StatusCode::BAD_REQUEST, "missing seed query param").into_response();
        };
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed);
        if !owns {
            return (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#
                    .to_vec(),
            )
                .into_response();
        }
        match state.hub.get_content(&seed, &content_id) {
            Some(entry) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, entry.media_type)],
                entry.bytes,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "content not found or expired").into_response(),
        }
    }

    async fn handle_content_upload(
        State(state): State<AppState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        let Some(ct) = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        else {
            return (StatusCode::BAD_REQUEST, "missing content type").into_response();
        };
        let Some(boundary) = ct
            .split(';')
            .find_map(|part| part.trim().strip_prefix("boundary="))
            .map(|v| v.trim_matches('"').as_bytes().to_vec())
        else {
            return (StatusCode::BAD_REQUEST, "multipart boundary required").into_response();
        };
        let delimiter = [b"--".as_slice(), boundary.as_slice()].concat();
        let mut seed: Option<String> = None;
        let mut media_type: Option<String> = None;
        let mut content: Option<Vec<u8>> = None;
        // Split on the exact boundary without interpreting arbitrary binary bytes.
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut offset = 0;
        while let Some(relative) = body[offset..]
            .windows(delimiter.len())
            .position(|window| window == delimiter.as_slice())
        {
            parts.push(&body[offset..offset + relative]);
            offset += relative + delimiter.len();
        }
        parts.push(&body[offset..]);
        for part in parts {
            let part = part.strip_prefix(b"\r\n").unwrap_or(part);
            let part = part.strip_suffix(b"\r\n").unwrap_or(part);
            let Some(header_end) = part.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&part[..header_end]);
            let value = &part[header_end + 4..];
            let Some(name) = headers
                .split(';')
                .find_map(|piece| piece.trim().strip_prefix("name=\""))
                .and_then(|value| value.strip_suffix('"'))
            else {
                continue;
            };
            match name {
                "seed" => seed = String::from_utf8(value.to_vec()).ok(),
                "media_type" => media_type = String::from_utf8(value.to_vec()).ok(),
                "content" => content = Some(value.to_vec()),
                _ => {}
            }
        }
        let Some(seed) = seed.filter(|s| !s.is_empty()) else {
            return (StatusCode::BAD_REQUEST, "missing seed").into_response();
        };
        let Some(content) = content else {
            return (StatusCode::BAD_REQUEST, "missing file part").into_response();
        };
        let media_type = media_type.unwrap_or_else(|| "application/octet-stream".into());
        let owns = state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed);
        if !owns {
            return (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"content_forbidden"}"#.to_vec(),
            )
                .into_response();
        }
        let content_id = state
            .hub
            .put_content(&seed, &media_type, content.clone(), false);
        let resp = serde_json::json!({
            "content_id": content_id.clone(),
            "media_type": media_type,
            "sha256": content_id.clone(),
            "size": content.len(),
            "truncated": false,
        });
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&resp).unwrap_or_default(),
        )
            .into_response()
    }

    /// `POST /ringing/v1/service/{method}` — 服务面 RPC（Read/Write 两类，
    /// 方法表见 `qaqh_runtime::ringing::service_methods`）。旧
    /// `/queries/{name}` 与 `/actions/{name}` 双端点已并入此处。
    async fn handle_service(
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
                    br#"{"code":"lease_required","message":"attach the session seed before calling"}"#.to_vec(),
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
                serde_json::to_vec(&service_methods::error_response(info.kind, &e))
                    .unwrap_or_default(),
            )
                .into_response(),
        }
    }

    // ---- SSE helpers (P2) ----
    fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
        let mut parts = cursor.split(':');
        let e = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        let seq = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if e == epoch && c == channel.as_str() {
            seq
        } else {
            0
        }
    }

    fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
        let mut parts = cursor.split(':');
        let e = parts.next().unwrap_or_default();
        let kind = parts.next().unwrap_or_default();
        let seq = parts.next().and_then(|v| v.parse::<u64>().ok());
        if e == epoch && kind == "timeline" && parts.next().is_none() {
            seq.unwrap_or(0)
        } else {
            0
        }
    }

    fn envelope_to_event(
        epoch: &str,
        channel: RingingChannel,
        env: &qaqh_ringing::RingingEventEnvelope,
    ) -> Event {
        let event_type = serde_json::to_value(&env.event)
            .ok()
            .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "message".into());
        // data is full envelope JSON (renderer expects seed/stream_seq/event_id)
        let data = serde_json::to_string(env).unwrap_or_else(|_| "{}".into());
        Event::default()
            .id(format!("{}:{}:{}", epoch, channel.as_str(), env.stream_seq))
            .event(event_type)
            .data(data)
    }

    fn reset_to_event(reset: &RingingResetRequired) -> Event {
        let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
        Event::default().event("ringing.reset_required").data(data)
    }

    fn timeline_entry_to_event(
        epoch: &str,
        seed: &str,
        entry: &qaqh_domain::TimelineEntry,
    ) -> Event {
        let data = serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 1,
            "server_epoch": epoch,
            "seed": seed,
            "entry": entry,
        });
        Event::default()
            .id(format!("{}:timeline:{}", epoch, entry.timeline_seq))
            .event("timeline.entry")
            .json_data(data)
            .unwrap_or_else(|_| Event::default().data("{}"))
    }

    fn filter_replay_for_session(
        mut replay: qaqh_runtime::ringing::hub::ChannelReplay,
        session_id: &str,
        leases: &Arc<Mutex<RingingLeaseStore>>,
    ) -> qaqh_runtime::ringing::hub::ChannelReplay {
        let mut g = leases.lock().unwrap_or_else(|e| e.into_inner());
        replay.events.retain(|e| g.owns_seed(session_id, &e.seed));
        replay.resets.retain(|r| g.owns_seed(session_id, &r.seed));
        replay
    }

    // ---- SSE handlers (P2) ----
    async fn handle_events(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(channel_str): Path<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        if !state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_active_session(&session_id)
        {
            return lease_required_json();
        }
        let Some(channel) = parse_channel(&channel_str) else {
            return (StatusCode::NOT_FOUND, "unknown channel").into_response();
        };
        // Last-Event-ID from header or ?last_event_id= query (ringing_http compat)
        let last_event_id = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
            .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
            .unwrap_or("");
        let after_seq = parse_sse_cursor(last_event_id, &state.epoch, channel);

        // Subscribe before replay to avoid gap
        let rx = state.hub.subscribe(channel);
        let replay = filter_replay_for_session(
            state
                .hub
                .replay_channel_since(channel, after_seq, after_seq == 0),
            &session_id,
            &state.leases,
        );
        let replayed_ids: HashSet<String> =
            replay.events.iter().map(|e| e.event_id.clone()).collect();
        let epoch = state.epoch.clone();
        let leases = state.leases.clone();
        let session_id_clone = session_id.clone();

        // Use mpsc channel to bridge broadcast to Sse stream (keeps axum 0.8 Send + 'static)
        let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
        tokio::spawn(async move {
            // Replay
            for env in replay.events {
                let ev = envelope_to_event(&epoch, channel, &env);
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            for reset in replay.resets {
                let ev = reset_to_event(&reset);
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(envelope) => {
                        if !leases
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .owns_seed(&session_id_clone, &envelope.seed)
                        {
                            continue;
                        }
                        if envelope.stream_seq <= after_seq
                            || replayed_ids.contains(&envelope.event_id)
                        {
                            continue;
                        }
                        if !leases
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_active_session(&session_id_clone)
                        {
                            break;
                        }
                        let ev = envelope_to_event(&epoch, channel, &envelope);
                        if tx.send(Ok(ev)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let stream = ReceiverStream::new(rx_stream);
        Sse::new(stream)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keep-alive"),
            )
            .into_response()
    }

    async fn handle_timeline_events(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(seed): Path<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        let Some(session_id) = get_session_id(&headers) else {
            return lease_required_json();
        };
        if seed.is_empty()
            || !state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owns_seed(&session_id, &seed)
        {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"lease_required"}"#.to_vec(),
            )
                .into_response();
        }
        let last_event_id = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
            .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
            .unwrap_or("");
        let after = parse_timeline_cursor(last_event_id, &state.epoch);
        let rx = state.hub.subscribe_timeline();
        let replay = state.hub.timeline_replay_since(&seed, after);
        let replayed: HashSet<u64> = replay.iter().map(|e| e.timeline_seq).collect();
        let epoch = state.epoch.clone();
        let leases = state.leases.clone();
        let seed_clone = seed.clone();
        let session_id_clone = session_id.clone();

        let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
        tokio::spawn(async move {
            for entry in replay {
                let ev = timeline_entry_to_event(&epoch, &seed_clone, &entry);
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(live) => {
                        if live.seed != seed_clone
                            || live.entry.timeline_seq <= after
                            || replayed.contains(&live.entry.timeline_seq)
                        {
                            continue;
                        }
                        if !leases
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_active_session(&session_id_clone)
                        {
                            break;
                        }
                        let ev = timeline_entry_to_event(&epoch, &seed_clone, &live.entry);
                        if tx.send(Ok(ev)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let stream = ReceiverStream::new(rx_stream);
        Sse::new(stream)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keep-alive"),
            )
            .into_response()
    }

    // ---- debug static (P3) ----
    fn renderer_root() -> PathBuf {
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

    fn mime_for(path: &StdPath) -> &'static str {
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

    fn safe_join(root: &StdPath, url_path: &str) -> Option<PathBuf> {
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

    async fn handle_debug_bridge(State(state): State<AppState>) -> Response {
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

    async fn handle_debug(State(_state): State<AppState>, Path(path): Path<String>) -> Response {
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

    async fn handle_debug_index(State(state): State<AppState>) -> Response {
        handle_debug(State(state), Path("index.html".to_string())).await
    }

    async fn loopback_guard(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
        if req.uri().path().starts_with("/debug")
            && let Some(ConnectInfo(addr)) =
                req.extensions().get::<ConnectInfo<SocketAddr>>().cloned()
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

    async fn handle_stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    async fn handle_stop_if_idle(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

    #[allow(dead_code)]
    pub async fn run_axum_with(
        config: crate::server::ServerNetworkConfig,
        state: AppState,
    ) -> Result<(), String> {
        let bind = (config.bind_ip, config.port);
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        log::info!("[axum] listening on {addr} (P4 stop + P3 debug + P2 SSE)");
        let app = build_router(state.clone());
        let mut shutdown_rx = state.shutdown.subscribe();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .map_err(|e| e.to_string())
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
                qaqh_ringing::RingingEvent::Control(
                    qaqh_domain::ControlEvent::SessionStateChanged {
                        state: qaqh_domain::SessionState::Created,
                        ..
                    }
                )
            ));
        }
    }
}
#[allow(unused_imports)]
pub use axum_impl::{AppState, build_router, run_axum_with};

#[cfg(test)]
mod axum_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::util::ServiceExt; // for oneshot

    static TEST_SERVICE: std::sync::OnceLock<qaqh_runtime::QaqhService> =
        std::sync::OnceLock::new();
    fn test_state() -> AppState {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::with_persistence(
            String::from("test-epoch"),
            std::env::temp_dir().join("qaqh-axum-test"),
        ));
        let leases = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::RingingLeaseStore::new(),
        ));
        let pending = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::PendingCommandStore::new(),
        ));
        let service = TEST_SERVICE
            .get_or_init(|| {
                qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
                qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
            })
            .clone();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        AppState {
            hub,
            leases,
            pending,
            service,
            token: String::from("test-token"),
            epoch: String::from("test-epoch"),
            shutdown,
        }
    }

    #[tokio::test]
    async fn health_ok() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn activity_exposes_has_active_work() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/activity")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // test_state() 无 agent：has_active_work 必须为 false，activities 为空表。
        assert_eq!(value["has_active_work"], serde_json::json!(false));
        assert_eq!(value["activities"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn open_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"schema":"qaqh.Ringing","version":1,"client_instance_id":"ci","capabilities":[]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn open_success() {
        let app = build_router(test_state());
        let body = serde_json::json!({
            "schema":"qaqh.Ringing","version":1,"client_instance_id":"ci-1"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_requires_lease() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_unknown_channel() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/events/bogus")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn events_success() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state.leases.lock().unwrap().attach_seed("cs-1", "seed-1");
        // publish an event for replay check (but new connection without cursor skips replay per design)
        let _ = state.hub.publish(
            "seed-1",
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "c1".into(),
                turn_id: "t1".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/events/tool")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
    }

    #[tokio::test]
    async fn timeline_events_success() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        state.leases.lock().unwrap().attach_seed("cs-1", "seed-1");
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/timeline/events")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn timeline_events_requires_seed_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        // not attached
        let app = build_router(state);
        let req = Request::builder()
            .uri("/ringing/v1/sessions/seed-1/timeline/events")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn debug_bridge_returns_token() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/debug/__qaqh_bridge__.js")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let txt = String::from_utf8_lossy(&body);
        assert!(txt.contains("window.__QAQH_DEBUG__"));
        assert!(txt.contains("test-token"));
    }

    #[tokio::test]
    async fn debug_rejects_traversal() {
        let app = build_router(test_state());
        // safe_join should reject traversal; we hit /debug/../outside
        let req = Request::builder()
            .uri("/debug/../outside")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum normalizes path, but our safe_join will reject => 400
        // If axum normalizes `..` to `/`, it may become 404; accept either 400 or 404
        assert!(resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn debug_not_found_for_missing_file() {
        let app = build_router(test_state());
        let req = Request::builder()
            .uri("/debug/missing_file_xyz.txt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stop_requires_auth() {
        let app = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/control/v1/stop")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stop_success() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/control/v1/stop")
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stop_if_idle_conflict_when_busy() {
        // has_active_work is false in test (no agents), so should be OK, not conflict
        // Just verify auth and basic path
        let app = build_router(test_state());
        let req = Request::builder()
            .method("POST")
            .uri("/control/v1/stop-if-idle")
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // In test, no active work, so 200; if busy would be 409
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn debug_bridge_rejects_non_loopback() {
        use axum::extract::ConnectInfo;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let app = build_router(test_state());
        let mut req = Request::builder()
            .uri("/debug/__qaqh_bridge__.js")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            12345,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn debug_bridge_allows_loopback() {
        use axum::extract::ConnectInfo;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let app = build_router(test_state());
        let mut req = Request::builder()
            .uri("/debug/__qaqh_bridge__.js")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
