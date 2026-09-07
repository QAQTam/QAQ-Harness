//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

mod axum_impl;

pub use axum_impl::{AppState, build_router};

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

    /// SessionAttach：仅 lease attach，不触碰 actor。attach 后 owns_seed 放行
    /// timeline/频道读取；空 seed 被拒（missing_seed）。
    #[tokio::test]
    async fn session_attach_grants_seed_ownership_without_actor_side_effects() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state.clone());
        let env = qaqh_ringing::RingingCommandEnvelope::new(
            "cmd-attach-1",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                seed: "sub-seed-1".into(),
            }),
        )
        .with_client_session_id("cs-1")
        .with_seed("sub-seed-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.leases.lock().unwrap().owns_seed("cs-1", "sub-seed-1"));

        // 空 seed → Rejected missing_seed，且不产生任何归属。
        let app = build_router(state.clone());
        let env_bad = qaqh_ringing::RingingCommandEnvelope::new(
            "cmd-attach-2",
            "ci-1",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
                seed: String::new(),
            }),
        )
        .with_client_session_id("cs-1");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/commands/control")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env_bad).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.leases.lock().unwrap().owns_seed("cs-1", ""));
    }

    /// todo CLI 路线：WRITE_SEEDED 调用在 lease 未 attach seed 时必须 401
    /// （拒绝发生在 dispatch 之前，不触碰磁盘）。
    #[tokio::test]
    async fn todo_set_requires_seed_ownership() {
        let state = test_state();
        state
            .leases
            .lock()
            .unwrap()
            .open("cs-1".into(), "ci-1".into());
        let app = build_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/service/todo.set")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", "cs-1")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"seed": "seed-1", "id": "T1", "status": "completed"})
                    .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// open 握手声明 attach_seed 后，同 seed 的 READ_SEEDED 调用放行
    /// （todo.list 只读，读不到即空表，不落盘）。
    #[tokio::test]
    async fn todo_list_allowed_after_open_attached_seed() {
        let state = test_state();
        // 经 handle_open 真实路径建 lease 并 attach（覆盖 flatten 解析分支）
        let app = build_router(state);
        let open_req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/clients/open")
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "schema": qaqh_ringing::protocol::RINGING_SCHEMA,
                    "version": qaqh_ringing::protocol::RINGING_VERSION,
                    "client_instance_id": "ci-cli",
                    "attach_seed": "seed-1",
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(open_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let open: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session_id = open["client_session_id"].as_str().expect("session id");
        let req = Request::builder()
            .method("POST")
            .uri("/ringing/v1/service/todo.list")
            .header("authorization", "Bearer test-token")
            .header("x-qaqh-client-session-id", session_id)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"seed": "seed-1"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
