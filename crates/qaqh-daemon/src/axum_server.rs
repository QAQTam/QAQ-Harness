//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse, Response},
        routing::{get, post},
        body::Bytes,
    };
    use serde::Deserialize;
    use tower::limit::ConcurrencyLimitLayer;
    use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};

    use qaqh_domain::{ControlCommand, RingingChannel};
    use qaqh_ringing::{
        ClientOpenRequest, ClientOpenResponse, RingingCommandAck, RingingCommandAckStatus,
        RingingCommandEnvelope, RingingCommandState, RingingEvent, RINGING_BASE_PATH,
        RINGING_SCHEMA, RINGING_VERSION,
    };
    use qaqh_runtime::{QaqhService, RingingHub};
    use qaqh_runtime::ringing::{query, PendingCommandStore, RingingLeaseStore};

    use crate::server::random_hex;

    const RENEW_TTL_MS: u64 = 30_000;
    const RENEW_INTERVAL_MS: u64 = 10_000;
    const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
    const MAX_CONNECTIONS: usize = 128;
    const TIMELINE_PAGE_LIMIT: usize = 30;
    const RECEIPT_TTL_DUR: Duration = Duration::from_secs(300);

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
            br#"{"code":"lease_required","message":"open a Ringing v1 client session first"}"#.as_slice(),
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

    fn parse_query_param(query: &str, key: &str) -> Option<String> {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_string())
        })
    }

    use qaqh_runtime::ringing::hydrate_attachment_previews;

    fn query_method(name: &str) -> Option<&'static str> {
        match name.trim_matches('/') {
            "session/list" | "session.list" => Some("session.list"),
            "session/meta" | "session.meta" => Some("session.meta"),
            "session/activity" | "session.activity" => Some("session.activity"),
            "session/dashboard" | "session.dashboard" => Some("session.dashboard"),
            "session/get_activity" | "session.get_activity" => Some("session.get_activity"),
            "workspace/get" | "workspace.get" => Some("workspace.get"),
            "workspace/status" | "workspace.status" => Some("workspace.status"),
            "fs/list" | "fs.list" => Some("fs.list"),
            "fs/read" | "fs.read" => Some("fs.read"),
            "config/load" | "config.load" => Some("config.load"),
            "skills/list_tools" | "skills.list_tools" => Some("skills.list_tools"),
            "todo/status" | "todo.status" => Some("todo.status"),
            "plan/read" | "plan.read" => Some("plan.read"),
            "plan/context_stats" | "plan.context_stats" => Some("plan.context_stats"),
            "stats/token_usage" | "stats.token_usage" => Some("stats.token_usage"),
            "git/diff" | "git.diff" => Some("git.diff"),
            "git/branch" | "git.branch" => Some("git.branch"),
            "git/branches" | "git.branches" => Some("git.branches"),
            "git/file_diff" | "git.file_diff" => Some("git.file_diff"),
            "daemon/version" | "daemon.version" => Some("daemon.version"),
            _ => None,
        }
    }

    fn is_allowed_action(method: &str) -> bool {
        method.starts_with("git.")
            || method.starts_with("workspace.")
            || method.starts_with("config.")
            || method.starts_with("session.set_tool_mode")
            || method.starts_with("subagent.")
            || matches!(
                method,
                "session.set_tool_mode" | "config.save" | "subagent.spawn" | "subagent.stop"
            )
    }

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
                let idx = turns.iter().position(|t| t.turn_id == id).unwrap_or(turns.len());
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
        (StatusCode::OK, format!("ok epoch={} token_len={}", state.epoch, state.token.len()))
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
        let supported: &[&str] = &[
            "Ringing_v1",
            "Ringing_batch_v1",
            "Ringing_bootstrap_v1",
            "Ringing_command_status_v1",
        ];
        let capabilities: Vec<String> = supported
            .iter()
            .filter(|c| req.capabilities.iter().any(|x| x == *c))
            .map(|c| (*c).to_string())
            .collect();
        if capabilities.len() != supported.len() {
            return (
                StatusCode::UPGRADE_REQUIRED,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"missing_capability","message":"Ringing v1 capabilities are incomplete"}"#.to_vec(),
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
            capabilities,
            server_epoch: state.hub.epoch().to_string(),
            lease_ttl_ms: lease_ttl_ms(),
            renew_interval_ms: RENEW_INTERVAL_MS,
        };
        (StatusCode::OK, JsonResponse(serde_json::to_vec(&resp).unwrap_or_default())).into_response()
    }

    struct JsonResponse(Vec<u8>);
    impl IntoResponse for JsonResponse {
        fn into_response(self) -> Response {
            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], self.0).into_response()
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
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(&resp).unwrap_or_default()).into_response()
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
                    message: Some(format!("{e}")),
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
                message: Some(format!("path channel {channel} != envelope channel {:?}", env.channel)),
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
            qaqh_ringing::RingingCommand::Conversation(qaqh_domain::ConversationCommand::ConversationLoadMore { .. })
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
        let fingerprint = qaqh_runtime::ringing::content_store::sha256_hex(fingerprint_payload.as_bytes());
        let duplicate_check = {
            let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
            match pending.record_fingerprint_for_session(&env.command_id, &fingerprint, &session_id) {
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
        let duplicate = duplicate_check.unwrap();
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
        if let qaqh_ringing::RingingCommand::Control(ControlCommand::SessionClose { seed: close_seed }) = &env.command {
            let close_seed = session_close_seed(close_seed, &env.seed);
            if close_seed.is_empty() {
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
            if let Err(error) = state.service.close_session(&close_seed, Some(&env.command_id)) {
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
            state.leases.lock().unwrap_or_else(|e| e.into_inner()).detach_seed(&session_id, &close_seed);
            state.pending.lock().unwrap_or_else(|e| e.into_inner()).mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
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
            cmd @ (ControlCommand::SessionArchive { .. } | ControlCommand::SessionUnarchive { .. } | ControlCommand::SessionDelete { .. }),
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
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
            let result: Result<(), String> = match op {
                "archive" => state.service.archive_session(&target, Some(&env.command_id)).map_err(|e| e.to_string()),
                "unarchive" => state.service.unarchive_session(&target).map_err(|e| e.to_string()),
                "delete" => {
                    let _ = state.service.close_session(&target, Some(&env.command_id));
                    state.service.delete_session(&target, Some(&env.command_id)).map_err(|e| e.to_string())
                }
                _ => unreachable!(),
            };
            if let Err(error) = result {
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
                state.leases.lock().unwrap_or_else(|e| e.into_inner()).detach_seed(&session_id, &target);
            }
            state.pending.lock().unwrap_or_else(|e| e.into_inner()).mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
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
                        state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
                    state.leases.lock().unwrap_or_else(|e| e.into_inner()).attach_seed(&session_id, seed);
                    publish_session_created(&state.hub, seed, &env.command_id);
                } else if let Some(seed) = created.get("seed").and_then(|v| v.as_str()) {
                    state.leases.lock().unwrap_or_else(|e| e.into_inner()).attach_seed(&session_id, seed);
                    publish_session_created(&state.hub, seed, &env.command_id);
                }
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
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
                if let Err(e) = state.service.handle("session.resume", &serde_json::json!({"seed": seed})) {
                    state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
                state.leases.lock().unwrap_or_else(|e| e.into_inner()).attach_seed(&session_id, seed);
                state.pending.lock().unwrap_or_else(|e| e.into_inner()).mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
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
            state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
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
            state.pending.lock().unwrap_or_else(|e| e.into_inner()).rollback(&env.command_id);
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("dispatch_failed".into()),
                message: Some(format!("{e}")),
                retry_after_ms: None,
            };
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&ack).unwrap_or_default(),
            )
                .into_response();
        }
        state.pending.lock().unwrap_or_else(|e| e.into_inner()).mark_running(&env.command_id);
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
        let snapshot = state.hub.timeline_snapshot(&seed).unwrap_or(qaqh_domain::TimelineSnapshot {
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
                br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#.to_vec(),
            )
                .into_response();
        }
        match state.hub.get_content(&seed, &content_id) {
            Some(entry) => (StatusCode::OK, [(header::CONTENT_TYPE, entry.media_type)], entry.bytes).into_response(),
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
        let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
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
        let content_id = state.hub.put_content(&seed, &media_type, content.clone(), false);
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

    async fn handle_query(
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
        let Some(method) = query_method(&name) else {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"unknown_query","message":"unknown typed query"}"#.to_vec(),
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
                        serde_json::to_vec(&serde_json::json!({"code":"invalid_body","message":format!("{e}")})).unwrap_or_default(),
                    )
                        .into_response();
                }
            }
        };
        if query::requires_seed(method) && params.get("seed").and_then(|v| v.as_str()).is_none() {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"invalid_envelope","message":"seed is required"}"#.to_vec(),
            )
                .into_response();
        }
        if query::requires_seed(method) {
            let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or_default();
            let owns = state
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owns_seed(&session_id, seed);
            if !owns {
                return (
                    StatusCode::UNAUTHORIZED,
                    [(header::CONTENT_TYPE, "application/json")],
                    br#"{"code":"lease_required","message":"attach the session seed before querying"}"#.to_vec(),
                )
                    .into_response();
            }
        }
        match query::query(&state.service, method, &params) {
            Ok(value) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&value).unwrap_or_default(),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&query::error_response(&e)).unwrap_or_default(),
            )
                .into_response(),
        }
    }

    async fn handle_action(
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
        let method = name.trim_matches('/').replace('/', ".");
        if !is_allowed_action(&method) {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "application/json")],
                br#"{"code":"unknown_action"}"#.to_vec(),
            )
                .into_response();
        }
        let params: serde_json::Value = if body.is_empty() {
            serde_json::json!({})
        } else {
            match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        [(header::CONTENT_TYPE, "application/json")],
                        serde_json::to_vec(&serde_json::json!({"code":"invalid_body","message":format!("{e}")})).unwrap_or_default(),
                    )
                        .into_response();
                }
            }
        };
        // seed ownership check if params contains seed
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
                    br#"{"code":"lease_required"}"#.to_vec(),
                )
                    .into_response();
            }
        }
        // handle via service
        match state.service.handle(&method, &params) {
            Ok(value) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&value).unwrap_or_default(),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_vec(&serde_json::json!({"code":"action_failed","message":e.to_string()}))
                    .unwrap_or_default(),
            )
                .into_response(),
        }
    }

    // SSE stubs for P1 (P2 will replace)
    async fn handle_events_stub(State(state): State<AppState>, headers: HeaderMap, Path(_channel): Path<String>) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        (StatusCode::NOT_IMPLEMENTED, "sse not yet migrated to axum (P2)").into_response()
    }
    async fn handle_timeline_events_stub(State(state): State<AppState>, headers: HeaderMap, Path(_seed): Path<String>) -> Response {
        if !is_authorized(&headers, &state.token) {
            return unauthorized();
        }
        (StatusCode::NOT_IMPLEMENTED, "timeline sse not yet migrated (P2)").into_response()
    }

    pub fn build_router(state: AppState) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/ringing/v1/clients/open", post(handle_open))
            .route("/ringing/v1/leases/renew", post(handle_renew))
            .route("/ringing/v1/commands/{id}", post(handle_command).get(handle_command_status))
            .route("/ringing/v1/sessions/{seed}/bootstrap", get(handle_bootstrap))
            .route("/ringing/v1/sessions/{seed}/timeline", get(handle_timeline_snapshot))
            .route("/ringing/v1/content/{content_id}", get(handle_content_get))
            .route("/ringing/v1/content", post(handle_content_upload))
            .route("/ringing/v1/queries/{name}", post(handle_query))
            .route("/ringing/v1/actions/{name}", post(handle_action))
            // SSE P2 stubs
            .route("/ringing/v1/events/{channel}", get(handle_events_stub))
            .route("/ringing/v1/sessions/{seed}/timeline/events", get(handle_timeline_events_stub))
            .fallback(not_found)
            .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
            .layer(ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    }

    pub async fn run_axum_with(
        config: crate::server::ServerNetworkConfig,
        state: AppState,
    ) -> Result<(), String> {
        let bind = (config.bind_ip, config.port);
        let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        log::info!("[axum] listening on {addr} (P1 REST migrated, SSE stubs)");
        let app = build_router(state);
        axum::serve(listener, app).await.map_err(|e| e.to_string())
    }
}
pub use axum_impl::{AppState, build_router, run_axum_with};

#[cfg(test)]
mod axum_tests {
    use super::*;
    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::util::ServiceExt; // for oneshot

    static TEST_SERVICE: std::sync::OnceLock<qaqh_runtime::QaqhService> = std::sync::OnceLock::new();
    fn test_state() -> AppState {
        let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::with_persistence(
            String::from("test-epoch"),
            std::env::temp_dir().join("qaqh-axum-test"),
        ));
        let leases = std::sync::Arc::new(std::sync::Mutex::new(qaqh_runtime::ringing::RingingLeaseStore::new()));
        let pending = std::sync::Arc::new(std::sync::Mutex::new(qaqh_runtime::ringing::PendingCommandStore::new()));
        let service = TEST_SERVICE.get_or_init(|| qaqh_runtime::QaqhService::init()).clone();
        AppState {
            hub,
            leases,
            pending,
            service,
            token: String::from("test-token"),
            epoch: String::from("test-epoch"),
        }
    }

    #[tokio::test]
    async fn health_ok() {
        let app = build_router(test_state());
        let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
    async fn open_rejects_missing_capabilities() {
        let app = build_router(test_state());
        let body = serde_json::json!({
            "schema":"qaqh.Ringing","version":1,"client_instance_id":"ci-1","capabilities":["Ringing_v1"]
        });
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn open_success() {
        let app = build_router(test_state());
        let body = serde_json::json!({
            "schema":"qaqh.Ringing","version":1,"client_instance_id":"ci-1",
            "capabilities":["Ringing_v1","Ringing_batch_v1","Ringing_bootstrap_v1","Ringing_command_status_v1"]
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
}
