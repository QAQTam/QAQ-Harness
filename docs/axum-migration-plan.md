# Ringing HTTP -> axum 0.8.9 迁移规划

> 基线：`docs.rs/axum 0.8.9 (2026-04-14)` 已校验，`Router` 路径 `/{id}` + `/{*path}`、`axum::serve(TcpListener)`、`response::sse::{Sse,Event,KeepAlive}`、`middleware::from_fn`。
> **修正（2026-08-28）：取消 `main` 冻结，大力推进**——不再 `feature-gated` 回滚，不再以 winui 冻结为锚点；`axum` 作为 `qaqh-daemon` 主 HTTP 栈直接替换手写 TCP，`webui` 默认拉起 `GET /debug/` 与 `qaqh-client` 同步切换到 axum 端。

## 1. 目标/非目标

- **目标**：用 `axum 0.8 + hyper 1.1 + tower 0.5 + tower-http 0.6 + tokio 1.44` **直接替换** `crates/qaqh-daemon/src/{http.rs,server.rs,ringing_http.rs:2866,debug_http.rs}` 手写 `TCP peek+read_request/write_response`；同一 `Router` 承载 Ringing V1 + `GET /debug/*` 静态 + 后续 `GET /api/config | PATCH /api/config | GET /api/config/events`（`config-revamp-plan.md §6`）。
- **非目标**：不改 Ringing V1 线协议（`envelope/ack/16M帧/三频道 SSE/batch/能力协商`）、不改 `RingingHub/LeaseStore/PendingCommandStore 4096 TTL` 语义、不改 `frontend-contract.md` 冻结的 `code` 集合；但**运输层实现不再保留双栈**，旧 `http.rs` 在 P4 直接下线。

## 2. 现状

`3914 行`：`http.rs 155`（`16M body/64K header/graceful_close 300ms`）+ `server.rs 513`（`TcpListener+Semaphore128+peek分流+daemon.json/lock`）+ `ringing_http.rs 2866`（`open/renew/commands/queries/actions/content/events/timeline`）+ `debug_http.rs 215`（`loopback 403+safe_join+__qaqh_bridge__.js`）。鉴权 `Bearer + X-QAQH-Client-Session-Id`，`lease TTL 30s`。

## 3. 目标架构（主干直切，不再 feature-gated 双跑）

```
TcpListener -> axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
  ServiceBuilder: TraceLayer, RequestBodyLimitLayer(16M), ConcurrencyLimit(128), DefaultBodyLimit::disable()
  route_layer: from_fn_with_state(auth_bearer) // 白名单 open/renew，其余 401；X-QAQH-Client-Session-Id 校验
  layer: from_fn_with_state(loopback_guard) // ConnectInfo<SocketAddr> 判 is_loopback()，/debug 非回环 403
  /ringing/v1/* -> ringing_router(State<AppState>) // 12 REST + 2 SSE + 1 timeline SSE
  /debug/{*path} -> ServeDir + index.html 注入 __qaqh_bridge__.js + safe_join
  /control/v1/stop, /stop-if-idle -> axum handler // 取代 server.rs peek 分流
  AppState { hub, leases, pending, service, token, epoch } // Arc 共享，与 server.rs 一致，Mutex 建议 tokio::sync::Mutex
```

## 4. 依赖（主干直依赖，不再 optional）

```toml
# crates/qaqh-daemon/Cargo.toml — 2026-08-28 起 axum 为主路径，去掉 optional/feature-gated
axum = { version = "0.8", features = ["http1","json","query","tokio"] }
tower = { version = "0.5", features = ["limit"] } # ConcurrencyLimitLayer 128
tower-http = { version = "0.6", features = ["trace","fs","limit","cors","set-header","util"] }
tokio-stream = "0.1"
futures-util = "0.3"
tokio-util = "0.7" # BroadcastStream
```
已落地：P0 骨架（`696aca5`）、P1 无状态 REST（`40e6b20`）在 `axum_server.rs` 验证通过；**下一步去 `#[cfg(feature="axum")]` 与 `optional`，`cargo check --workspace` 即 axum 路径**。

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

## 8. 分阶段（进度 2026-08-28，main 直推，不再 feature-gated）

- **P0 搭架** ✅ `696aca5`：`axum_server.rs` 骨架 + `build_router / AppState / run_axum_with` + `health` 探针，双绿。
- **P1 无状态 REST** ✅ `40e6b20`：`open/renew/commands/{id}(POST+GET)/queries/actions/content(POST+GET)/sessions/{seed}/bootstrap/timeline` 12 路由，`RequestBodyLimitLayer(16M) + ConcurrencyLimitLayer(128) + TraceLayer`，`Router::oneshot` 4 例。
- **P1.5 去门控** ⏳ **(立即)**：去 `#[cfg(feature="axum")]`/`optional`，`axum` 进主依赖；`qaqh-daemon/src/main.rs` 与 `server.rs` 切 `run_axum_with` 为默认 `run_with`，旧 `http.rs` 标记 `deprecated` 待 P4 删除。
- **P2 SSE** ⏳：`events/{channel}` + `timeline/events`，`Sse<Stream<…>> + BroadcastStream + KeepAlive 15s + Lagged=>close`，`timeline` cursor 独立。当前 `501 stub` 占位，**P1.5 后立即替换为真实流**（不再保留手写 SSE 回退）。
- **P3 静态** ⏳：`ServeDir + safe_join + loopback 403 + index.html 注入 __qaqh_bridge__.js`，`ConnectInfo` 落地。
- **P4 收口** ⏳：删除 `http.rs`，`stop/stop-if-idle` 进 `Router`，`Semaphore128` 完全由 `ConcurrencyLimitLayer` 接管，`peek` 分流整段删除；`qaqh-client` 与 `webui` 切 axum 端点验证后即视为完成。

**不再一次性 big-bang 回滚**：任一阶段失败直接在 `main` 上修复前滚；P2 前的 §11-12 预研合并入 P1.5 同步做（协议抽离 + Linux 可编译）。

## 9. 验证（主干 axum，无 --features 区分）

- 单测：`Router::oneshot` 4 例（`health/auth/426/open_success`），`cargo test -p qaqh-daemon axum_tests` 4/4；`cargo check --workspace` 即 axum。
- 集成：`daemon_ws --ignored` + `qaqh-client` 长连（P1 已 REST 直通，P2 后覆盖 SSE）；`GET /debug/` 非回环 `403`（`ConnectInfo`）。
- 手工（P2 后）：`curl -H "Authorization: Bearer $token" http://127.0.0.1:$port/ringing/v1/events/control -H "Last-Event-ID: epoch:control:123"` 断点续传；`curl http://127.0.0.1:$port/debug/` 注入脚本校验。

## 10. 风险（直推模式）

体积 `+1~2M`、`RSS +10M` 内（`hyper` 已在 `reqwest` 依赖树）；**不再保留手写回退**，失败前滚修复；`daemon.json` 端口与 `discovery` 写入时序保持 `server.rs:268` 语义不变。

## 11. 合并冗余协议论证（2026-08-27 讨论）

三频道 `Control/Conversation/Tool` + `Timeline` 四套 `seq` 是历史长出来的隔离，非设计目标。合并为**单 `global_seq per seed` 单 `SSE /v2/stream`** 的显著收益非仅简化：
- **并发**：1客户端 `5连->1连`，`128` 连接上限从 32客户端打满降到 128客户端才满，`keepalive` 定时器 `/4`
- **一致性**：全序替代四序，`Lagged` 窗口单一，`replay_since(after)` 一次拉齐，消掉 `ToolFinished` 与 `TurnCompleted` 竞态
- **带宽/落盘**：`hub` 4份 `HashMap` + 3份 `bootstrap` 合并为 1份 `global log`，`40 turns 5.6MB` 问题在源头缓解
- **SDK**：`4 EventSource -> 1 EventSource`，`Last-Event-ID` 单游标

结论：`P2` 先按 `v1` 原样迁保证行为一致，`v2` 单流 `GET /ringing/v2/stream+GET /api/config` 在 `main` 上紧随其后面向 `webui`/`qaqh-client` 演进（不再开 `prep/ringing-merge` 长分支，避免双轨漂移）。

## 12. 工程前置（随 P1.5 同步落地，不再单分支）

- **协议抽离**：`RingingLeaseStore/PendingCommandStore 4096` 从 `ringing_http.rs:2866` 抽到 `qaqh-runtime/src/ringing`，`axum_server` 与旧 `http` 共用，消灭两份 `hydrate_attachment_previews` 拷贝
- **平台抽象**：`qaqh-types/src/platform.rs` 补 `#[cfg(unix)]`：`XDG ~/.local/share/qaqh`、`chmod 600`、`kill(pid,0)` 判活，现 `Linux` 为 `stub true` 会双 `daemon`
- **Linux 编译必绿**：`winresource` 已 `cfg(windows)`，`tower limit` 已补，`WSL` 内 `cargo check --target x86_64-unknown-linux-gnu -p qaqh-daemon` 预检，`CRLF/\\?\` 仅 `Windows` 走
- **测试隔离**：`QaqhService::init()` 全局 `Once`，`axum_tests` 已用 `OnceLock` 串行化，`daemon_ws` 同理加 `serial`
- **分支**：**不再开 `prep/ringing-merge` 长分支**，全部在 `main` 直推；`v2` 单流在 P2/P3 完成后直接在 `main` 上增量加路由 `GET /ringing/v2/stream`（`Ringing_v1` 能力协商保持兼容，新增 `Ringing_v2` 能力）

## 附：代码位置（2026-08-28 修订）

- P0: `crates/qaqh-daemon/src/axum_server.rs` + `main.rs: mod axum_server`
- P1: 同文件 12 路由 + `Cargo.toml: tower limit` + `axum_tests 4`
- P1.5: 去 `#[cfg(feature="axum")]`/`optional`，`Cargo.toml` 主依赖化，`server.rs:run_with` 切 `axum::serve(...into_make_service_with_connect_info)` 为默认
- P2: `handle_events`/`handle_timeline_events` 替换 `501 stub` 为 `Sse + BroadcastStream + KeepAlive 15s`
- P3: `debug_router` `ServeDir` + `safe_join` + `loopback_guard` ConnectInfo
- P4: 删除 `http.rs`，`peek` 分流与 `Semaphore 128` 下线；`qaqh-client` 与 `webui` 在 `main` 同步切 axum
