# QAQ-Harness qaqh-mcp · PR-M1-2 开工前移交报告（handoff）

| 项 | 值 |
|---|---|
| 日期 | 2026-09-07（UTC+8 下午，本轮会话结束点） |
| 交接范围 | 上一份 handover（`docs/handover-2026-09-07-mcp-client.md`，M1-1 收口）→ **PR-M1-2 准备阶段**：现状复验 + 设计/PLAN 重读 + rmcp/process-wrap API 探针 + 仓库模式摸底，**止于 M1-2 编码开工前（零代码改动）** |
| 起始 HEAD | `468cf59`（structure-simplification Phase 0-4 + D2 收官） |
| 结束 HEAD | `468cf59`（**未动**；MCP 改动仍全部在工作区未提交，与上一份 handover 一致：8 个修改 + 3 类新增路径） |
| 本轮代码改动 | **无**。本轮只读 + `cargo test/check/clippy/fmt --check` 验证，未写任何 `.rs/.toml/.md` |
| 质量基线（本轮实测） | `cargo test -p qaqh-config --test mcp_config` **10 passed** · `cargo test -p qaqh-mcp` **1 passed**（m0_spike）+ 2 个空 target 0 passed · `cargo check -p qaqh-mcp -p qaqh-config -p qaqh-types -p qaqh-config-api` **0 error** · `cargo clippy … --all-targets` **0 警告** · `cargo fmt --all --check` **有 1 处 diff（见 §三，需接手者跑 `cargo fmt`）** |
| 权威输入 | `docs/mcp-client-design.md`（270 行，设计权威）· `docs/PLAN.md`（130 行，路线图；M1-2 出口见 §4）· 上一份 handover（背景/决策链/D1–D6/E-1–E-8 全量记录，本文件不再复述，只写增量） |
| 执行会话 | QAQ-Harness Agent 主会话；owner 中途指示“终止、移交其他 LLM”；本文件即移交物 |

---

## 一、本轮做了什么（增量）

1. **现状复验**：`git status --short` / `git diff --stat` / `git ls-files --others` 确认工作区与上一份 handover §三完全一致（8M + `crates/qaqh-config/tests/mcp_config.rs` + `crates/qaqh-mcp/{Cargo.toml,src/lib.rs,tests/m0_spike.rs}` + `docs/mcp-client-design.md`；另有 `docs/handover-2026-09-07-mcp-client.md` 本体也是 untracked——上一份 handover 写“3 个新增路径”是把两份 docs 合并计为一类，此处如实列出 4 个 untracked 路径）。
2. **基线复测**：M1-1 出口（mcp_config 10 用例）与 M0 出口（m0_spike 1 用例）重跑全绿；`check`/`clippy` 干净；`fmt --check` 发现 `crates/qaqh-config/src/config.rs:818` 起有格式漂移（M1-1 遗留，`.then_some` 链式缩进），接手第一步跑 `cargo fmt` 即可。
3. **M1-2 开工探针（只读 vendor + 仓库源码，未落码）**：
   - rmcp 3.2.0（vendor 路径见 §六）`service`/`transport/child_process`/`service/client`/`model` 全链路签名摸清；
   - process-wrap 9.1.0 `CommandWrap`/`ProcessGroup`/`JobObject` 的 feature 门控与构造方式摸清；
   - 仓库侧 `OnceLock` 槽位、`ToolManager` 三阶段、`process_group(0)` + `killpg`/`taskkill /T`、`cancel-contract` 红线、`McpConfig` 类型全貌摸清。
4. **todo 状态**：T1（核验基线）completed；T2（研读设计/PLAN）completed；T3（实现 M1-2）in_progress 但**零代码**；T4（测试验证与交付）idle。另有 T5–T9 是本轮误建的重复 todo，已全部 cancelled——接手者忽略 T5–T9，只认 T1–T4。

## 二、M1-2 任务（下一位直接开工，按 PLAN §4 原文）

> `McpManager + connection actor：lazy connect（10s 超时）、inflight==0 idle 回收、重连冷却 5s、shutting_down 闸、RAII reap（killpg/taskkill /T）`
> 出口：`cargo test -p qaqh-mcp --test lifecycle` 全绿（connect/timeout/crash-reconnect/idle/shutdown 五用例，in-memory + 子进程各一）

设计锚点：`docs/mcp-client-design.md` §5.1（生命周期表）+ §5.2（桥接/取消契约对齐）+ §4（crate 文件清单，**注意 §4 已过期**：`src/config.rs` 职责已由 PR-M1-1 落到 `qaqh-config`，qaqh-mcp 侧只消费 `qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind}`——M1-2 开工时顺手把 §4 文件清单改成 `error.rs/manager.rs/connection.rs/adapter.rs/lib.rs` + 测试）。

## 三、当前仓库状态（git，接手者照此核对）

```
HEAD: 468cf59（未动）
M  Cargo.toml                        # +成员 qaqh-mcp + [workspace.dependencies] rmcp
M  Cargo.lock                        # rmcp 3.2.0 + process-wrap 9.1.0 + 传递依赖
M  crates/qaqh-types/src/config.rs   # PersistentMcp* + mcp 字段
M  crates/qaqh-types/src/lib.rs      # re-export +2
M  crates/qaqh-config/src/config.rs  # McpConfig 三类型 + map_mcp_config + load/save（⚠️ fmt 有 1 处漂移）
M  crates/qaqh-config/src/dto.rs     # to_dto 补 mcp
M  crates/qaqh-config-api/src/lib.rs # McpDto/McpServerDto + ConfigDto.mcp
M  docs/PLAN.md                      # qaqh-mcp 专用 PLAN（130 行）
?? crates/qaqh-config/tests/mcp_config.rs
?? crates/qaqh-mcp/Cargo.toml
?? crates/qaqh-mcp/src/lib.rs        # 13 行占位（CRATE_PURPOSE）
?? crates/qaqh-mcp/tests/m0_spike.rs
?? docs/handover-2026-09-07-mcp-client.md
?? docs/mcp-client-design.md         # 270 行
```

`crates/qaqh-mcp/Cargo.toml` 现状（M1-2 必须改）：

```toml
[dependencies]
rmcp = { workspace = true }
log = "0.4"
serde_json = "1"
[dev-dependencies]
rmcp = { workspace = true, features = ["transport-io", "server"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util", "time"] }
```

M1-2 预计要加的依赖（探针结论，还未加）：`qaqh-config`（path，消费 McpConfig）、`process-wrap`（直依赖，版本对齐 9.1.0，features 需要 `tokio1` + Unix `process-group` / Windows `job-object`——注意 rmcp 只开了 process-wrap 的 `tokio1`，feature 并集后 Unix `ProcessGroup` 才可用；`tokio` 需补 `sync/process/time` 给 manager/actor 用）。

## 四、探针结论（M1-2 实现直接用，不用重探）

### 4.1 rmcp 3.2.0 客户端 API（vendor 实测，非训练记忆）

- 入口：`use rmcp::{ServiceExt, ClientServiceExt, ClientLifecycleMode}`；`().serve(transport)` = legacy initialize；**M1-2 用 `().serve_with_lifecycle(transport, ClientLifecycleMode::Auto { preferred_versions: vec![ProtocolVersion::V_2026_07_28], legacy_version: Some(ProtocolVersion::V_2025_11_25) })`**（设计 §5.1 点名的 Auto：discover 优先、10s 回退 legacy；vendor `service/client.rs:628` 起，`DEFAULT_AUTO_DISCOVER_TIMEOUT = 10s` 内建）。
- `ProtocolVersion` 是 `struct Cow<'static,str>`（不是 enum）：`V_2026_07_28 / V_2025_11_25 / V_2025_06_18 / V_2025_03_26 / V_2024_11_05` 常量 + `LATEST = V_2025_11_25`（`model.rs:155` 起）。
- 返回 `RunningService<RoleClient, ()>`，`Deref<Target = Peer<RoleClient>>`：调 `client.list_all_tools().await`（`service/client.rs:1727`，自动翻页）做连通验收；`peer.is_transport_closed()`（`service.rs:1047`）可做 crash 判定；关闭用 `close(&mut self)`（非消费）/`cancel(self)`（消费）/`close_with_timeout`，`waiting(self)` 消费等待。M1-2 的 idle/shutdown 路径用 `close()` + 超时兜底。
- 错误：`ClientInitializeError`（connect 失败细分：transport/discover/legacy-fallback）与 `ServiceError`（`TransportClosed` = crash 信号之一）。M1-2 先做二分映射（`MCP_CONNECT_FAILED / MCP_CONNECT_TIMEOUT / MCP_SERVER_CRASHED`），全量八码是 M1-5 的事。
- 传输：任何 `Transport<RoleClient>` 经 `IntoTransport` blanket 自动适配；in-memory 测试继续用 `(read, write)` 元组（`transport/async_rw.rs:24`，M0 已验证）；子进程用 `TokioChildProcess::new(command: impl Into<CommandWrap>)` 或 `::builder(cmd).spawn()`（`transport/child_process.rs`）。**实锤缺口**：`child_process.rs` 全文件裸用 `CommandWrap`（无 `ProcessGroup` 引用），rmcp 对 process-wrap 只开 `["tokio1"]`——M1-2 必须在 qaqh-mcp 侧组装隔离后再交 `TokioChildProcess::new`（见 4.2）。
- `Peer::new` 是 `pub(crate)`——测试里**不能**自造 disconnected peer；lifecycle 测试的 transport 注入走 `serve_with_lifecycle` + in-memory 双工 + mock `ServerHandler`（M0 模式复用），子进程用例走真 `TokioChildProcess`。

### 4.2 process-wrap 9.1.0 隔离组装（M1-2 核心动作）

- `CommandWrap: From<tokio::process::Command>`，`CommandWrap::with_new(program, |cmd| …)` 亦可；`wrap(wrapper)` 链式；`spawn()` 返回 `Box<dyn ChildWrapper>`（`generic_wrap.rs` 宏生成，`tokio/core.rs`）。
- Unix：`use process_wrap::tokio::ProcessGroup; cmd.wrap(ProcessGroup::leader())` → `pre_spawn` 调 `command.process_group(0)`（`tokio/process_group.rs:87`），`wrap_child` 包 `ProcessGroupChild`（kill 走 `killpg` + `waitpid(-pgid)` 全组 reap，含僵尸回收循环）。feature 门：`process-group`（Unix-only）+ `tokio1`。
- Windows：`use process_wrap::tokio::JobObject; cmd.wrap(JobObject)`（`tokio/job_object.rs`，kill 走 job 全树）。feature 门：`job-object` + `creation-flags`（Windows-only）。本机 Linux 验证，Windows 路径照抄仓库既有 `taskkill /T /F` 语义，专项孤儿测试留到 M1-5（PLAN 明确）。
- 仓库既有同构先例：`qaqh-workspace/src/exec/direct.rs:54` `cmd.process_group(0)` + `process_registry.rs:439–467` `killpg / taskkill /T /F`（含句柄已回收时按 `os_pid` 快照尽力清树）。qaqh-mcp 的 RAII reap 照此抄：Unix `killpg` 整组、Windows `taskkill /T /F`，**禁单 pid kill**（E-1 红线）。

### 4.3 仓库模式（照抄，不要发明）

- 全局槽位：`backend.rs:228` `static WORKSPACE_BACKEND: OnceLock<RwLock<Arc<dyn …>>>` + `get_or_init` + 读写锁中毒用 `unwrap_or_else(|p| p.into_inner())`。`McpManager` 全局槽（`manager_slot()`）照此写。`QaqhService.hub: OnceLock<Arc<RingingHub>>` 是 daemon 组装先例（McpManager 归属 QaqhService，设计 §10 已决）。
- `ToolManager`：`handlers: BTreeMap<String, ToolHandler>` + `PreparedCall.handler_fn: fn(ToolCallCtx) -> ToolResult`（`manager.rs:52`）；M1-2 **不碰** ToolManager（那是 M1-4），只把 `McpManager` 做成“可被 bridge 调用的生命周期容器”。
- 取消红线：全 crate 禁 `set_cancel(false)` 字面（M1-5 静态验收 `rg -n "set_cancel\(false\)"` 零命中；M1-2 起自觉遵守）；禁触全局 `qaqh_workspace::CANCEL`；只读 `ctx.cancel` 留给 M1-5 bridge。
- clippy：workspace 级 `unwrap_used = "deny" + string_slice = "deny"`（根 Cargo.toml:35）；测试豁免靠 `clippy.toml: allow-unwrap-in-tests = true`（`#[cfg(test)]` + `tests/`）。生产代码锁用 `unwrap_or_else(into_inner)`，不用 `expect/unwrap`；字符串不用 `&s[..n]` 切片。
- `McpConfig` 消费：`qaqh_config::config::{McpConfig, McpServerConfig, McpTransportKind}`（`config.rs:136/145/167`），`servers: BTreeMap<String, McpServerConfig>`，`idle_shutdown_secs: u64`（0=常驻），`default_timeout_secs/max_concurrent_calls` 在 M1-2 仅存档（调用链是 M1-5 的事）。

## 五、M1-2 建议实现草图（非定稿，接手者可调，但状态机语义按设计 §5.1）

```
qaqh-mcp/src/
  lib.rs        # CRATE_PURPOSE 保留 + pub mod + manager_slot()（OnceLock<RwLock<Arc<McpManager>>>）
  error.rs      # McpErrorKind { Disabled/ConnectFailed/ConnectTimeout/ServerCrashed/NotFound/Shutdown } + Display（M1-5 再扩八码）
  adapter.rs    # build_isolated_command(cfg) -> CommandWrap（Unix ProcessGroup::leader / Windows JobObject；env 注入；stderr inherit=丢弃计数）
  connection.rs # ServerConnection：Mutex<Option<RunningService<RoleClient, ()>>> + inflight(AtomicU64) + last_idle_from(Mutex<Instant>) + cooling_until(Mutex<Instant>)；ensure_connected()/mark_idle()/shutdown()
  manager.rs    # McpManager { cfg: McpConfig, conns: Mutex<BTreeMap<String, Arc<ServerConnection>>>, shutting_down: AtomicBool }；get_or_spawn/shutdown_all/handle_crash
tests/lifecycle.rs  # connect/timeout/crash-reconnect/idle/shutdown 五用例（PLAN 出口）
```

- connect 超时：`tokio::time::timeout(10s, serve_with_lifecycle(Auto))`；冷却 5s 内直接 `MCP_CONNECT_FAILED(cooling)`；`shutting_down` 闸在 manager 入口拒绝 lazy connect/reconnect/idle 重启。
- idle：`inflight == 0` 连续 `idle_shutdown_secs` 才回收（E-2；否决“无调用即计时”）；`idle_shutdown_secs == 0` 常驻。测试用小值 `McpConfig`（如 1s）+ `tokio::time::pause()` 或短 sleep，避免 300s 真等。
- crash：`is_transport_closed()` 或 `list_all_tools` 报 `TransportClosed` → 标记断连、当前调用 `MCP_SERVER_CRASHED` 不重试、下次调用走冷却后重启。
- RAII：`Drop for McpManager` 置闸 + `try_close`（spawn_blocking 或 `tokio::task::block_in_place` 视运行时而定；若拿不准，至少保证 `shutdown_all().await` 显式路径 + Drop 内尽力 `cancel` token）。

## 六、关键文件索引与探针锚点

| 路径 | 说明 |
|---|---|
| `docs/mcp-client-design.md`（270 行） | 设计权威；§5.1 生命周期、§5.2 桥接/E-1、§4（过期清单待修）、§10-6 归属决议 |
| `docs/PLAN.md`（130 行） | §4 M1-2 出口、§5 总闸+红线、§7 进程组结论（已回填） |
| `docs/handover-2026-09-07-mcp-client.md` | 上一份 handover（背景/D1–D6/E-1–E-8/坑与教训 §六必读） |
| `docs/cancel-contract.md` | 三层取消 + 红线（禁 `set_cancel(false)`、L3 组杀） |
| `crates/qaqh-config/src/config.rs:132–300/810–845` | McpConfig 类型 + 校验 + save 映射 |
| `crates/qaqh-workspace/src/backend.rs:228–248` | 槽位模式范本 |
| `crates/qaqh-workspace/src/exec/direct.rs:40–60` + `process_registry.rs:430–470` | 进程组 spawn + 组杀范本 |
| `crates/qaqh-workspace/src/manager.rs:1–120` | ToolManager/PreparedCall（M1-4 前只读） |
| rmcp vendor `…/mirrors.tuna.tsinghua.edu.cn-*/rmcp-3.2.0/src/{service.rs,service/client.rs:600–1050,transport/child_process.rs,transport/async_rw.rs,model.rs:150–220}` | serve/Auto/Peer/Transport 实测源 |
| process-wrap vendor `…/process-wrap-9.1.0/src/{lib.rs,greg…generic_wrap.rs,tokio.rs,tokio/process_group.rs,tokio/job_object.rs}` + `Cargo.toml [features]` | CommandWrap/ProcessGroup/JobObject + feature 门 |

## 七、风险与已知坑（接手者先读）

1. 上一份 handover §六 7 条坑全部有效（re-export 名单、BTreeMap/HashMap 不对称、编辑工具引号坑、rmcp API 漂移、`timeout` 嵌套 Result、`load` 静默回退）。本轮补充：`cargo fmt --check` 已脏（config.rs:818），开工先 `cargo fmt`。
2. `windows = 0.58`（仓库）vs process-wrap 要 `windows 0.62`——Windows 构建可能出现 windows crate 双版本；Linux 本机无感，M1-5 双平台验收时若冲突，以 process-wrap 侧为准升级或 `cfg(windows)` 隔离。
3. `Peer::new` 不可用（pub(crate)）；`ProtocolVersion` 不是 enum（别写 exhaustive match）。
4. 远端 API 不稳定（owner 原话）：本轮所有结论来自**本地 vendor 源码 + 本地 cargo 实测**，未依赖远端 ctx7；接手者同样优先本地 vendor，`npx ctx7` 只作辅助。
5. 全量 `cargo test --workspace` 本轮未跑（沿用上一份 handover 登记项）；M1-2 收口前按 PLAN §5 总闸补跑 `just check/clippy/fmt/test`（`justfile:78` 起，clippy 为 `--workspace --all-targets`）。

## 附录：快速核验命令（本轮实测通过）

```bash
git status --short && git log --oneline -1
cargo test -p qaqh-config --test mcp_config          # 10 passed
cargo test -p qaqh-mcp                               # m0_spike 1 passed
cargo check -p qaqh-mcp -p qaqh-config -p qaqh-types -p qaqh-config-api  # 0 error
cargo clippy -p qaqh-types -p qaqh-config -p qaqh-config-api -p qaqh-mcp --all-targets  # 0 警告
cargo fmt --all --check                              # 当前 1 处 diff（config.rs:818，先 fmt）
rg -n "set_cancel\(false\)" crates/qaqh-mcp/src      # 零命中（红线保持）
ls ~/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-*/rmcp-3.2.0/src/transport/
```
