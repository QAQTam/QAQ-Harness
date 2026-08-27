//! axum 迁移 P0 搭架：feature-gated，不改变现有手写 TCP 行为。
//! 以 winui 为稳定锚点，API 冻结，axum 仅 --features axum 时编译

#[cfg(feature = "axum")]
mod axum_impl {
    use std::sync::{Arc, Mutex};
    use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
    use qaqh_runtime::{QaqhService, RingingHub};
    use crate::ringing_http::{PendingCommandStore, RingingLeaseStore};

    #[derive(Clone)]
    pub struct AppState {
        pub hub: Arc<RingingHub>,
        pub leases: Arc<Mutex<RingingLeaseStore>>,
        pub pending: Arc<Mutex<PendingCommandStore>>,
        pub service: QaqhService,
        pub token: String,
        pub epoch: String,
    }

    async fn health(State(state): State<AppState>) -> impl IntoResponse {
        (StatusCode::OK, format!("ok epoch={} token_len={}", state.epoch, state.token.len()))
    }
    async fn not_found() -> impl IntoResponse {
        (StatusCode::NOT_FOUND, "not found")
    }
    pub fn build_router(state: AppState) -> Router {
        Router::new().route("/health", get(health)).fallback(not_found).with_state(state)
    }
    pub async fn run_axum_with(config: crate::server::ServerNetworkConfig, state: AppState) -> Result<(), String> {
        let bind = (config.bind_ip, config.port);
        let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        log::info!("[axum] listening on {addr} (P0 health only)");
        let app = build_router(state);
        axum::serve(listener, app).await.map_err(|e| e.to_string())
    }
}
#[cfg(feature = "axum")]
pub use axum_impl::{AppState, build_router, run_axum_with};
