# qaqh-mcp 设计：MCP 客户端支持

状态：**设计稿（待评审）** · 2026-09-07 · 作者：Omen Alpha（QAQ-Harness Agent）· 决策人：项目 owner

## 1. 背景与目标

QAQ-Harness 当前 19 个内置工具为静态注册，无法消费 MCP（Model Context Protocol）生态的
动态工具与资源。本设计为 daemon 增加 **MCP 客户端能力**：把外部 MCP server 声明的
**Tools** 接入既有工具环，把 **Resources** 接入会话上下文，使 Agent 无需改代码即可
获得任意外部工具（context7、GitHub、数据库、浏览器控制……）。

### 已拍板的决策（owner 确认，2026-09-07）

| # | 决策点 | 选择 |
|---|--------|------|
| D1 | SDK 路线 | 基于 **rmcp 3.2 官方 SDK**（`default-features = false`，只开 client + 必要 transport） |
| D2 | Phase 1 能力 | **Tools + Resources**（prompts/sampling/elicitation/tasks → Phase 2） |
| D3 | 方向 | **仅 MCP 客户端**（消费外部 server；反向 server → Phase 2，复用 `qaqh-workspace serve`） |
| D4 | 权限模型 | **配置声明即信任**：`config.toml` 白名单里的 server 免审批（类比 trust folder） |
| D5 | 权限档位交互 | **全档位默认放行 MCP**（owner 2026-09-07 拍板）：临时豁免已登记 §5.5；未来按 workspace 隔离重构权限体系时重新收敛 |
| D6 | Resources 形态 | **`mcp` 聚合工具**（owner 2026-09-07 确认）：不新增 per-server 资源工具，防模型面膨胀（§5.4） |
### 本文补充的建议决策（待 owner 复核）

| # | 决策点 | 建议 | 理由 |
|---|--------|------|------|
| S1 | Phase 1 传输 | **仅 stdio**（子进程） | npx 系 server 是绝对主流；跳过 `reqwest` 依赖面；streamable HTTP 放 M3 |
| S2 | 工具命名 | `mcp__{server}__{tool}` 前缀 | 与内置 19 工具零碰撞（`responses_search_function_alias` 已有先例教训）；server 名校验 `[a-z0-9_-]+` |
| S3 | MCP 工具的 `ToolCategory` | stdio server 的工具 → **`Exec`**；http server → `Net` | 复用"子代理沙箱 exec/net 自动拒绝"机制，**无需为子代理写任何特判**；审计语义准确（调用最终在 server 进程内执行代码/发起网络请求） |

## 2. 依赖策略

```toml
# 根 Cargo.toml [workspace.dependencies]
rmcp = { version = "3.2", default-features = false, features = [
    "client",
    "transport-child-process",   # stdio（内含 transport-async-rw）
    # M3 追加: "transport-streamable-http-client-reqwest"
] }

# qaqh-mcp/Cargo.toml [dev-dependencies]（仅测试构建启用）
rmcp = { version = "3.2", features = ["transport-io", "server"] }  # in-memory 双工 + mock server
```

- rmcp 3.2.0（2026-08-31 发布）：tokio 原生、**edition 2024**、MSRV **1.88**（本机 1.98 ✅）、
  Apache-2.0（与 MIT 兼容）、实现 spec **`2026-07-28`**，向下兼容 `2025-11-25` 及更早
- ⚠️ rmcp `default` feature 是 `server`——**必须 `default-features = false`**，否则把 server
  侧全量编译进 daemon
- ⚠️ feature unification：dev-deps 的 `server` feature 只影响 test/clippy 构建，
  release 产物不受影响；clippy `--all-targets` 会看到合并后的 feature 集，属预期
- rmcp 3.x 有破坏性变更史（迁移指南 discussion #969）→ 第 5 节的 adapter 层隔离 SDK 类型

## 3. 架构总览

```
                    ┌──────────────────────────────────────────────┐
 agent actor 线程    │              qaqh-mcp（新 crate）              │
 ┌───────────┐      │  ┌────────────┐   mpsc req/resp   ┌────────┐ │
 │ tool ring  │─ctx─▶│  │ bridge.rs  │ ────────────────▶ │ conn   │ │   stdio 子进程
 │ (sync fn)  │      │  │ 同步桥接    │  per-server 串行  │ actor  │─┼─▶ npx -y @some/mcp-server
 │ handler    │◀─result── │ (阻塞recv) │ ◀──────────────── │(tokio) │ │   rmcp client
 └───────────┘      │  └────────────┘   轮询 cancel     └────────┘ │
                    │        ▲ tools/list 缓存        ▲ 生命周期     │
                    │        │                        │             │
                    │  ┌─────┴────────┐        ┌──────┴──────────┐  │
                    │  │ projection   │        │ McpManager      │──┼── config.toml [mcp.servers]
                    │  │ 模型面投影    │        │ daemon 级单例    │  │   （声明即信任）
                    │  └──────────────┘        └─────────────────┘  │
                    └──────────────────────────────────────────────┘
```

核心原则：**rmcp 客户端住在专属 actor（tokio runtime 线程）里，同步工具环通过
mpsc 通道桥接**——与仓库"每 actor 一个线程"哲学一致，且避免在 handler 线程
`block_on`（嵌套 runtime 风险）。`ToolCallCtx.cancel` 由桥接层轮询，命中即向
server 发 `notifications/cancelled`（best-effort）并放弃等待。

## 4. crate 设计

```
crates/qaqh-mcp/
├── Cargo.toml
├── tests/
│   ├── m0_spike.rs                    # M0：SDK 可行性（in-memory 握手）
│   ├── lifecycle.rs                   # M1-2：生命周期用例（PR-M1-2 出口）
│   └── fixtures/mcp_stdio_server.mjs  # 子进程 fixture（normal/deaf 两模式）
└── src/
    ├── lib.rs          # 模块导出 + manager_slot()/install_manager()（OnceLock 槽位模式）
    ├── manager.rs      # McpManager：连接表 + shutting_down 闸 + get_or_connect/shutdown_all（M1-2 ✅）
    ├── connection.rs   # ServerConnection：连接状态机 + idle watchdog + CallGuard（M1-2 ✅）
    ├── error.rs        # McpErrorKind（M1-2 七码；M1-5 扩全量八码→ToolResult JSON）
    ├── adapter.rs      # rmcp/process-wrap 隔离层 + 进程组组装（pub(crate)；M1-2 ✅）
    ├── bridge.rs       # PR-M1-5：同步 handler ↔ 连接；cancel 轮询；超时（默认 60s，封顶 3600）
    ├── projection.rs   # PR-M1-4：ToolDef 生成：前缀命名、schema 直通、allowed 过滤、碰撞拒绝
    └── resources.rs    # PR-M2：list/read 映射 + 系统提示注入块
```

> 清单修订（PR-M1-2，2026-09-07）：原 `config.rs`（[mcp] 解析与校验）已由
> PR-M1-1 落到 `qaqh-config::config::{McpConfig, McpServerConfig, McpTransportKind}`，
> qaqh-mcp 只消费；`bridge.rs`/`projection.rs`/`resources.rs` 为后续 PR 目标文件。

依赖方向（单向）：`qaqh-runtime` → `qaqh-mcp` → `{qaqh-workspace, qaqh-config, rmcp}`；qaqh-mcp **不反向依赖** runtime/msgloop——`register_dynamic` 与 refresh 由 runtime 侧在回合边界调用（§5.3）。
## 5. 核心机制

### 5.1 连接生命周期（connection.rs + manager.rs）

| 阶段 | 行为 |
|------|------|
| **lazy connect** | 首次调用该 server 的工具/资源时才拉起子进程并 `initialize`（ClientLifecycleMode::Auto：优先 discovery，10s 无响应回退 legacy——rmcp 内建）。**daemon 启动不做网络操作**，配置仅做静态校验（fail-fast） |
| **connect 超时** | 10s；失败 → `MCP_CONNECT_FAILED`，进入 5s 重连冷却，期间调用直接报错不重启 |
| **idle shutdown** | `inflight == 0` 连续 `idle_shutdown_secs`（默认 300，原子计数器判定）→ 优雅关闭（`service.cancel()` + 等待 `waiting()`）并 kill 子进程；下次调用重新拉起。否决“无调用即计时”：会把慢响应的活跃连接误判为空闲 |
| **执行中失败** | transport 断连/子进程退出 → `MCP_SERVER_CRASHED`，**当前调用不重试**（副作用语义沿用 HttpToolExecutionBackend 先例：只有"请求从未到达"才可安全重试，此处连重试都不做，交还模型决策） |
| **崩溃恢复** | 下一次调用触发单次重启；重启失败保持报错，冷却期内不再尝试 |
| **退出清理** | daemon 退出时 RAII 关闭全部连接并 reap 子进程（rmcp `transport-child-process` 内含 process-wrap） |
| **daemon 关闭闸门** | 关闭路径置 `shutting_down` 闸（对齐 AgentRegistry 同名字段语义）：冷却期内禁止 lazy connect / reconnect / idle 重启；RAII 依次 `cancel()` → `waiting()` → reap 子进程 |
- **tools/list 缓存**：connect 后拉取一次；收到 `notifications/tools/list_changed`
  仅置脏标记，**回合边界**统一刷新（不在工具执行中途改模型面——三阶段 PreparedCall
  流水线不可被中途变更破坏）
- **并发**：per-server 串行（actor 单队列），`max_concurrent_calls` 配置可放开（默认 1）——
  MCP server 并发耐受度参差，宁可保守

### 5.2 同步桥接（bridge.rs）—— 关键难点

`ToolHandler.handler` 是 **`fn(ToolCallCtx) -> ToolResult` 同步函数指针**（lib.rs:706），
无法携带 per-server 状态。方案：**全局单例 + 通道转发**，复用 `backend_slot()` 的
`OnceLock<RwLock<Arc<_>>>` 槽位模式：

```rust
// 注册进 ToolManager 的唯一 dispatcher fn 指针：无状态、永不失效
// （PreparedCall 在飞安全——动态替换 defs 不影响已捕获的 fn 指针）
fn mcp_dispatch(ctx: ToolCallCtx) -> ToolResult {
    let (server, tool) = parse_mcp_name(&ctx.name)?;  // "mcp__{server}__{tool}" 前缀解析
    let mgr = qaqh_mcp::manager_slot();               // 全局 McpManager（OnceLock 槽位模式）
    let rx = mgr.submit(server, tool, &ctx)?;         // mpsc 发送，立刻拿到 resp 接收端
    bridge::wait_response(rx, &ctx)                   // 阻塞 recv + 轮询 ctx.cancel + 超时
}
```

- 桥接层持有 `cancel: Arc<AtomicBool>`，等待期间每 ~250ms 轮询一次；命中 → 发
  `notifications/cancelled`、丢弃结果、返回 `MCP_CANCELLED`
- **取消契约对齐**（`docs/cancel-contract.md`，红线级）：桥接只读 `ctx.cancel`（L1/L2 已接线的 `Arc<AtomicBool>`），禁止触碰全局 `qaqh_workspace::CANCEL`；任何路径禁用 `set_cancel(false)` 字面调用（契约红线1，静态验收零命中）；MCP server 子进程 spawn **必须设独立进程组**（契约 L3：Unix `killpg` / Windows `taskkill /T`）——M0 实测 rmcp 不默认启用，adapter 层 `ProcessGroup::leader()` 补齐
- **超时链**：dynamic tool 的 `default_timeout`（来自 server 配置）作为 `ctx.timeout_secs`
- **PR-M1-5 落地差异（2026-09-07）**：①桥接宿主为 qaqh-mcp **专属 tokio runtime**（2 worker，OnceLock 惰性；工具裸线程无 ambient runtime，Handle::block_on 被否）②`max_concurrent_calls` 在 `begin_call` 执行，超限报 `MCP_BUSY` ③连接成功即拉取 tools/list 缓存并置脏；crash/idle 回收清缓存置脏——模型面重建（回合边界 `run_lap` 钩子）与连接状态一致 ④`notifications/cancelled` 以 `request_id=None` best-effort 发送（rmcp 内部生成请求 id 拿不到；结果本就丢弃，取消语义不依赖 server 遵从）⑤**rmcp `graceful_shutdown` 漏杀缺口**：其只等直接子进程——server 自行 spawn 的孙进程在 graceful 退出后成孤儿（npx→node→server 三层树必现）。补丁：adapter spawn 登记 pgid，连接层 close/crash 后组杀清扫（`connection.sweep_group`，orphan_reap 用例验证）
  ⑥**投影预热（prime，2026-09-07 冒烟修正）**：装配点 `prime_all_async()` fire-and-forget 逐 server `get_or_connect`（幂等，失败仅告警不短路）——lazy 连接唯一触发点是工具执行，而工具要先投影才会被调用，不预热则全新 daemon 上模型首回合永远看不到 MCP 工具（冒烟实证的鸡生蛋死锁）；预热不阻塞启动，未启用时 no-op，长期无调用的连接由 idle 回收收敛
  注入 → 桥接按 `tool_timeout_from_args` 同款语义取值（MCP schema 由 server 定义，
  QAQH **不**向其 inputSchema 注入额外参数）

### 5.3 工具投影（projection.rs）

- **注册方式**：`ToolManager` 新增一个公开方法 `pub fn register_dynamic(&mut self, name: String, tool: DynamicTool)`（碰撞拒绝后插入平行动态层，仅允许在**回合边界**由 actor 调用——无并发写入面）；`ToolManager` 另增 `clear_dynamic`（M2 全量重建入口）与 `category_of`（权限决策两层查询）。**PR-M1-4 落地修订**：原设计存 `ToolHandler`——但其 `description` 是 `&'static str`，无法承载 server 侧动态文本（`Box::leak` 会在每次 refresh 累积泄漏）；改为平行结构 `DynamicTool { def: ToolDef, handler_fn, placement, category, risk, default_timeout }`，E-5 单一 dispatcher fn 指针不变（PreparedCall 在飞安全），模型面合并/allowed 过滤/prepare 路由均覆盖动态层。`registration.rs` 的
  `extra_registrars`（fn 指针，构建期已知）不适合 MCP，因为工具集合要连接后才知道
- **模型面**：`to_tool_def()` 直通——MCP `inputSchema` 与 QAQH 的
  `ToolFunction.parameters` 同为 JSON Schema，零转换；`description` 若超长截断
  （上限 2KB + 截断标记，防上下文膨胀）
- **模型面体积治理**：per-server `tools = [...]` 白名单（不配 = 全暴露）；
  tool_mode `custom` 白名单用完整前缀名过滤。Phase 2 预留"按需加载 schema"
  （skills 渐进披露同款思路）
- **配置校验**（daemon 启动 fail-fast）：server 名合法字符、重名拒绝、
  command 非空、tools 白名单中的名字在连接后校验（未命中 → 日志 warn + 调用时
  `MCP_NOT_FOUND`，不阻止启动）

### 5.4 Resources 接入（resources.rs）

Phase 1 采用**最小侵入**方案：资源不进对话循环的状态机，通过两个既有概念落地：

1. **内置聚合工具**：一个由 qaqh-mcp 注册的工具
   `mcp` —— `action: "list_resources" | "read_resource" | "list_servers"`，
   参数 `{ server?, uri }`。模型显式读取；返回文本直通，二进制以
   `[blob mime=<mt> size=<n> uri=<uri>]` 占位
   **PR-M2-1 落地差异（2026-09-07）**：聚合工具**无 `mcp__` 前缀**（不经 D5
   快路径——它是 QAQH 内置只读工具而非 server 声明，D4"声明即信任"不适用，
   category=`Read` 走常规审批 level≥2 直通）；注册走 M1 投影管线**批次钉底**
   （`projection_batch_with` 头部固定携带，enabled 即在场——零 server 配置也
   能答 `list_servers`，与连接状态解耦）；E-5 第二根 dispatcher 指针
   （`resources::aggregate_dispatch`，与 per-server 工具的 `dispatch` 并列）。
2. **系统提示注入**：会话 Environment 快照追加一段封顶清单（默认列前 20 条：
   `name / uri / mime / description`），回合边界刷新，让模型"知道有什么可读"。
   资源模板列出 URI 模板串，由模型展开后调 read
   **PR-M2-2 落地差异（2026-09-07）**：落点不是 frozen `[Environment]` 注解
   （会话级冻结，不满足"回合边界刷新"）而是 **ContextFlow trailing developer
   消息**（`builtin::MCP_RESOURCES` source，skills envelope 同管线）；门控为
   **内容比对**（变更才物化，prefix cache 稳定）；封顶 20 条（server 头行不
   占额）/描述 120 字符；**list_changed 订阅**：adapter `NotifyBridge` 客户端
   handler（rmcp `ClientHandler::on_tool_list_changed`/`on_resource_list_changed`）
   → `CONN_NOTIFY` Weak 表（record_spawn_pid 同款 crate 内桥接，Weak 防环）
   → 重拉 tools+resources 清单并置脏；附带修复观察项①（`allowed_raw` +
   动态层重建后 `reapply_allowed_after_dynamic_change`）。

不做：资源订阅/更新推送（`subscriptions` → Phase 2）、自动内联资源内容。

### 5.5 权限与审计

| 关注点 | 处理 |
|--------|------|
| 审批 | **全档位默认放行**（D5，owner 2026-09-07 拍板）：MCP 调用不经 PermissionChallenge，与权限档位暂不交互；category 照常填写（S3：stdio=Exec / http=Net）供审计展示与 PermissionRisk 分级。**登记临时豁免**：MaxLockdown 档下 MCP 调用同样放行——该窗口由未来 workspace 隔离权限重构收敛。**PR-M1-5 落地**：`authorization::admit` 对 `mcp__` 前缀快路径直通 Authorized（绕过 needs_permission，故沙箱拦截在同一处显式兑现）；"沙箱零代码"修正为"沙箱零特判 + 快路径内 3 行显式拒绝"（S3 语义不变，测试在 authorization.rs） |
| 子代理沙箱 | **零代码**——"exec/net 自动拒绝"按 category 生效，MCP 工具天然被拒，符合"子代理最小面"原则 |
| 审计 | 每次调用经既有 audit 通道：`server/tool/args_summary/elapsed/success`；**env 值永不落审计与日志** |
| ToolStats | `calls_total/failures` 正常累计；`files_read/written` 不适用（留空） |
| 输出上限 | CallToolResult 全部 content blocks 拼接后按既有 output_size 上限截断（截断标记） |
| stderr | server stderr 默认丢弃（仅计数留痕）；可配置捕获但必须过脱敏钩子——env/secret 值出现在 stderr 时不得进 ToolResult、审计或模型上下文 || tool-level vs protocol 错误 | `isError=true` → `ToolResult::error`（模型可见，语义=工具跑了但失败）；协议层 `Err(McpError)` → 错误码细分（见 §7） |

## 6. 配置 Schema（config.toml）

```toml
[mcp]
enabled = true
idle_shutdown_secs = 300        # 空闲回收；0 = 常驻

# stdio server（Phase 1 主形态）
[mcp.servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
env = { CONTEXT7_API_KEY = "${secret:context7_key}" }   # ${secret:} 插值 → secrets.toml
# tools = ["resolve-library-id", "get-library-docs"]    # 可选白名单（缺省=全部）
# resources = true                                       # 暴露该 server 的资源（默认 true）
# default_timeout_secs = 60                              # 工具默认超时（封顶 3600）
# max_concurrent_calls = 1                               # per-server 并发（默认 1）

# streamable HTTP server（M3）
[mcp.servers.figma]
url = "https://mcp.figma.com/mcp"
headers = { Authorization = "Bearer ${secret:figma_pat}" }
```

- `${secret:name}` 插值需**扩展 `qaqh-config/secrets.rs`**：现实现为 per-slot（Main/Subagent）API key 仓库，无通用命名 secret——新增通用 map 段（`[secrets.mcp]`），沿用既有机制（Windows DPAPI 加密、其余 0600）；未注册的 secret 名 → 启动时校验报错（fail-fast，缺 key 不静默）。**禁止**把 env 值明文写进 config.toml（审计 P0-1 前车之鉴）。**PR-M1-3 落地补充**：load 只校验不解析——`Config.mcp`/DTO/save 始终只见占位符，真值在 qaqh-mcp 连接时才进子进程 env（改 secrets.toml 下次重连即生效，轮换友好）；args/headers 同样支持占位符与启动扫描
- `qaqh-config-api` Phase 1 增加只读 DTO（ConfigDto 展开 `[mcp]`）；写模型（UI 管理
  server）→ Phase 2
- 配置热重载（`watch.rs` 已有能力）：Phase 2——Phase 1 改配置需重启 daemon

## 7. 错误模型

全部走 `ToolResult::error`，JSON 结构沿用 `WORKSPACE_EXEC_FAILED` 风格（timeis /
status=error / code / message / hint）：

| code | 场景 | hint 要点 |
|------|------|-----------|
| `MCP_DISABLED` | `[mcp].enabled=false` 或工具不在白名单 | 指向配置键 |
| `MCP_CONNECT_FAILED` / `MCP_CONNECT_TIMEOUT` | 拉起/握手失败 | 冷却中，稍后重试；检查 command 可执行 |
| `MCP_SERVER_CRASHED` | transport 断连/子进程退出 | 下次调用自动重启；本次未重试 |
| `MCP_TIMEOUT` | 调用超时 | server 可能仍在执行，未取消成功时勿盲目重发 |
| `MCP_PROTOCOL_ERROR` | JSON-RPC / schema 错误 | 透传 server message |
| `MCP_TOOL_ERROR` | `isError=true`（工具跑完但业务失败） | 透传 content |
| `MCP_NOT_FOUND` | 工具/资源不存在 | 列出该 server 实际可用名单 |
| `MCP_CANCELLED` | ctx.cancel 命中 | 说明已尽力发送取消通知 |
| `MCP_BUSY` | server 在飞调用达 `max_concurrent_calls`（PR-M1-5 执行点新增，§7 原表未列） | 等在飞调用排空后重试 |
| `MCP_SHUTDOWN` | daemon 关闭闸已落下（闸门码；设计原表未列） | daemon 正在关闭，不再受理 |

## 8. 测试计划

| 层 | 内容 | 依赖 |
|----|------|------|
| 单测 | config 解析/校验（重名、非法字符、secret 插值）、前缀命名、schema 直通与截断、错误码映射 | 无 |
| 集成（in-memory） | `transport-io` 双工 + rmcp `ServerHandler` mock：connect→list→call→cancel→timeout→断连→重连→idle 回收 | dev-deps（test/clippy 构建启用 server feature） |
| 集成（子进程） | 仓库内 `examples/mcp_test_server.rs`（rmcp server 宏实现 echo/slow/fail 三工具）走真实 stdio，验证拉起/reap/超时抢占 | dev-deps |
| E2E（可选） | `QAQH_MCP_E2E=1` 时连接真实 `npx @upstash/context7-mcp`，跑 tools/list + 一次真实调用 | 网络（默认跳过） |
| 惯例 | 全部触碰全局状态的测试走 `TEST_RUNTIME_SERIAL`；子进程测试用 RAII guard 防孤儿（参照 backend.rs ServeGuard） | |

## 9. 里程碑

| 阶段 | 内容 | 预估 |
|------|------|------|
| **M0 spike** | crate 骨架 + feature 组合验证（`default-features=false` 编译通过）+ in-memory transport 连 mock server 跑通 tools/list | 0.5 天 |
| **M1 核心** | config schema + McpManager 生命周期 + 动态注册/投影 + 调用路径 + 审计 + 子代理拒绝验证 + 集成测试 | 2-3 天 |
| **M2 resources** | `mcp` 聚合工具 + 系统提示注入 + tools/list_changed 回合边界刷新 | 1-2 天 |
| **M3 打磨** | streamable HTTP client、unix socket、`${secret}` 插值、ToolStats 接入、docs 更新 | 1-2 天 |
| Phase 2（另立 RFC） | 反向 MCP server（包 serve）、prompts/sampling/elicitation/tasks、OAuth、资源订阅、图片 content → read_image、配置热重载、WSL stdio server | — |

## 10. 风险与开放问题

1. **rmcp 3.x API 漂移**（63 个版本/18 个月）→ adapter.rs 隔离 + 锁定 `3.2.x`；
   升级只动一个文件
2. **模型面膨胀**：用户挂 5 个 server 各 30 工具 → 上下文压力大 → 白名单 + description
   截断 + Phase 2 按需加载
3. **恶意 server**：信任模型是"配置即信任"，等同信任一个可执行程序——文档需明示；
   审计链保留全量 args（可事后追责）
4. **Windows 子进程清理**：process-wrap 兜底，但参考 incidents 里 fd-hold 教训，
   M1 必须包含"杀 daemon 后无孤儿 server 进程"的专项测试
5. **prompt injection（接受风险）**：MCP 工具结果/resource 内容直通模型上下文是协议固有面；既有防线=ToolResult JSON 结构包裹 + 输出截断 + stderr 脱敏（§5.5）；不做内容过滤（与内置工具同基线）
6. **开放问题（owner 复核）**：
   - ~~McpManager 归属~~ **已决议（2026-09-07，owner 确认）**：归属 `QaqhService`——daemon 级组装，`OnceLock<Arc<McpManager>>` 槽位（对齐 `hub` 字段模式，装配点在 `QaqhService::init`，与 `init_tools` 同层）。理由：MCP 连接是 per-server 跨会话共享资源，与 AgentRegistry 的 per-session 生命周期轴正交；桥接走全局槽位，无需 worker env 注入（区别于 workspace serve 的 attach_workspace 模式）
   - `register_dynamic` 的调用时机是否接受"仅回合边界"约束（中庸方案，Phase 2 再考虑
     mid-turn 投影）
   - resources 系统提示注入的预算（默认 20 条 / 每条截断 120 字符）是否合适

## 11. 现有代码接触面（最小侵入清单）

| 文件 | 改动 |
|------|------|
| 根 `Cargo.toml` | workspace members + `[workspace.dependencies]` 加 rmcp |
| `qaqh-workspace/src/manager.rs` | `+register_dynamic()`（一个公开方法） |
| `qaqh-workspace/src/manager.rs`（投影） | 模型面构建处合并 dynamic defs（一处） |
| `qaqh-msgloop` | 回合边界调用 `mcp::refresh_if_dirty()`（一处钩子） |
| `qaqh-config` / `qaqh-config-api` | `[mcp]` 解析 + 只读 DTO |
| `qaqh-runtime` | QaqhService 组装 McpManager，注入 actor 构建 |
| **新 crate** `qaqh-mcp` | 全部新代码，不反向依赖 msgloop/runtime |

**PR-M1-5 实际接线（2026-09-07，与上表差异已核对）**：本仓库无独立 qaqh-msgloop crate——回合边界钩子落 `qaqh-runtime/src/agent/engine_turn.rs::run_lap`（`take_projection_batch` → `runtime::replace_dynamic_tools` → `tool_defs` 重建）；`QaqhService::init` 装配（`install_manager`）+ daemon main 收尾 `shutdown_global`；workspace 侧实际改动 = `manager.rs`（动态层 + `MCP_DYNAMIC_PREFIX` pub）+ `runtime.rs`（`category_of` 两层 + `replace_dynamic_tools`）+ `authorization.rs`（D5 快路径 + 沙箱显式拒绝）。

权限系统、审计、子代理沙箱、tool_mode：**零改动**（全靠 category 复用与既有过滤链）。
