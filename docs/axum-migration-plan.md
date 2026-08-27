# Ringing HTTP -> axum 0.8.9 迁移规划

> 基线：`docs.rs/axum 0.8.9 (2026-07-09)` 已校验，`Router` 路径 `/{id}/{*rest}`、`axum::serve(TcpListener)`、`response::sse::{Sse,Event,KeepAlive}`、`middleware::from_fn`。
> 稳定锚点：主 UI `G:\qaqh-winui-app`（`qaqh-client` path 依赖），API 冻结以 winui 为准，webui 随 axum 成功后默认拉起 `GET /debug/`。

## 1. 目标/非目标

- **目标**：用 `axum 0.8 + hyper 1.1 + tower 0.5 + tower-http 0.6 + tokio 1.44` 替换 `crates/qaqh-daemon/src/{http.rs,server.rs,ringing_http.rs:2866,debug_http.rs}` 手写 `TCP peek+read_request/write_response`，后续 `GET /api/config | PATCH /api/config | GET /api/config/events`（`config-revamp-plan.md §6`）复用同一 `Router`。
- **非目标**：不改 Ringing V1 线协议（`envelope/ack/16M帧/三频道 SSE/batch/能力协商`）、不改 `RingingHub/LeaseStore/PendingCommandStore 4096 TTL` 语义、不改 `frontend-contract.md` 冻结的 `code` 集合。

## 2. 现状

`3914 行`：`http.rs 155`（`16M body/64K header/graceful_close 300ms`）+ `server.rs 513`（`TcpListener+Semaphore128+peek分流+daemon.json/lock`）+ `ringing_http.rs 2866`（`open/renew/commands/queries/actions/content/events/timeline`）+ `debug_http.rs 215`（`loopback 403+safe_join+__qaqh_bridge__.js`）。鉴权 `Bearer + X-QAQH-Client-Session-Id`，`lease TTL 30s`。

## 3. 目标架构

```
TcpListener -> axum::serve(listener, Router<()>)
  layer: TraceLayer, RequestBodyLimitLayer(16M), ConcurrencyLimit(128)
  middleware::from_fn(auth_bearer), from_fn(loopback_guard) // debug
  /ringing/v1/* -> ringing_router(State<AppState>)
  /debug/{*path} -> ServeDir + fallback inject
  AppState { hub, leases, pending, service, token, epoch } // 共享 Arc，与 server.rs 一致
```

## 4. 依赖

```toml
# crates/qaqh-daemon/Cargo.toml [features] axum = ["dep:axum", "dep:tower", ...]
axum = { version = "0.8", optional = true } # default = http1,json,query,tokio
tower = "0.5"
tower-http = { version = "0.6", features = ["trace","fs","limit","cors","set-header","util"] }
tokio-stream = "0.1"
futures-util = "0.3"
```
已在 P0 落地：`--features axum` 时编译，默认 `memory` 不影响 winui。

## 5. API 映射（0.8 防旧）

| 旧手写 | 新 0.8 写法 | 备注 |
|---|---|---|
| `if path=="/ringing/v1/clients/open"` | `.route("/ringing/v1/clients/open", post(handle_open))` | `0.8` 路径用 `/{id}` 非 `:id`，通配 `/{*path}` |
| `read_request` | `Json<T>/Bytes/Query<Q>/Path<P>/HeaderMap` | `Json` 需 `Content-Type`，走 `JsonRejection` |
| `write_response` | `-> impl IntoResponse { (StatusCode, Json(v)) }` | `Infallible` 模型 |
| `write_all(": keepalive")` | `Sse::new(stream).keep_alive(KeepAlive::new().interval(15s))` | |
| `peek分流 debug` | `nest("/debug", debug_router) + ServeDir` | |
| `Server::bind` | `tokio::net::TcpListener::bind; axum::serve(l, app).await` | 0.8 已删 Server |

## 6. SSE 设计

旧：`id: {epoch}:{channel}:{seq}\nevent: ...\ndata: ...\n\n` + `broadcast Lagged => close`。
新：`Sse<Stream<Item=Result<Event, Infallible>>>` 桥 `BroadcastStream`，`replay + live` 合并，`KeepAlive 15s`，`Lagged` 时结束流触发客户端 `Last-Event-ID` 重播，`timeline` cursor `epoch:timeline:seq` 独立。

## 7. 鉴权/中间件

`middleware::from_fn(auth_bearer)` 统一 `401`，白名单 `open/renew`，其余校验 `leases.is_active_session`；`loopback_guard` 用 `ConnectInfo<SocketAddr>` 判 `is_loopback()`，LAN `0.0.0.0` 下非回环 `403`，与 `debug_http.rs` 一致。

## 8. 分阶段

- **P0 搭架**（本 commit）：`axum_server.rs` 骨架 + `build_router / AppState / run_axum_with` + `health` 探针，`cargo check --workspace` 与 `cargo check -p qaqh-daemon --features axum` 双绿，零行为变更。
- **P1 无状态 REST**：`open/renew/commands/{channel}/commands/{id}/queries/actions/content/timeline snapshot`，`RequestBodyLimitLayer(16M)`。
- **P2 SSE**：`events/{channel}` + `timeline/events`，`BroadcastStream` + `KeepAlive`。
- **P3 静态**：`ServeDir` + `safe_join` + `index.html` 注入 `__qaqh_bridge__.js`。
- **P4 收口**：下线 `http.rs`，`stop/stop-if-idle` 进 `Router`，`Semaphore128` -> `ConcurrencyLimitLayer`，web 默认拉起，winui 回归。

一次性方案不采纳（回滚面大）。

## 9. 验证

- 单测：`Router::oneshot` 直测 `JsonRejection/401/幂等4096`。
- 集成：现有 `daemon_ws --ignored` + `qaqh-client` 长连。
- 手工：`curl -H "Authorization: Bearer $token" http://127.0.0.1:$port/ringing/v1/events/control -H "Last-Event-ID: ..."` 断点续传；`GET /debug/` 非回环 `403`。

## 10. 风险

体积 `+1~2M`、`RSS +10M` 内（`hyper` 已在 `reqwest` 依赖树），任何阶段失败 `cargo --no-default-features` 切回手写，`daemon.json` 端口不变。

## 附：P0 代码位置

`crates/qaqh-daemon/src/axum_server.rs`（`#[cfg(feature="axum")]`），`crates/qaqh-daemon/src/main.rs` 增加 `mod axum_server`。
