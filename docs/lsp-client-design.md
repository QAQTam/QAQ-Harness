# qaqh-lsp 设计：LSP 客户端支持

状态：**M1 已实施**（关联补齐：opencode 导入 + MCP `cwd`）· 2026-09-08 · 决策人：项目 owner（L1–L6 全按建议批）

> 实施记录：M1（本 PR）——见 `docs/PLAN.md` §4 对应 PR 行。

## 1. 背景与目标

codegraph 是"索引兜底"（零配置跨语言），LSP 是"编译器级精确"（实时语义）。
两者共存：LSP 精确实时，codegraph 零配置跨语言。本设计为 daemon 增加
**LSP 客户端能力**：把 language server 的**精确导航**接入既有工具环。

M1 五操作：`definition / references / hover / documentSymbol / workspaceSymbol`。
M2（另立项）：增量 `didChange` 跟随 edit、`implementation/callHierarchy`、
diagnostics 推送注入、多 root 并存。

### 已拍板的决策（owner 确认，2026-09-08）

| # | 决策点 | 选择 |
|---|--------|------|
| L1 | 模型面 | 单个聚合工具 `lsp`，`action` 枚举（mcp `mcp` 同款） |
| L2 | 坐标系 | 模型面 **1-based**，内部转 0-based |
| L3 | 连接键 | `(server, workspace_root)` 双键，root 取会话 cwd |
| L4 | 文档同步 M1 | 调用时读磁盘 → `didOpen` → 查；常驻 open，不做增量镜像 |
| L5 | 诊断 | M1 不做 `publishDiagnostics` 注入（M2 加开关，默认关） |
| L6 | 权限 | `category=Read` 走常规审批，不进 D5 快路径 |
| L-选型 | Rust 库 | `async-lsp 0.2 + lsp-types 0.95`（tower-lsp 无 client、lsp-server 同步 server 脚手架，均不选） |

## 2. 依赖策略

```toml
# 根 Cargo.toml [workspace.dependencies]
async-lsp = { version = "0.2", default-features = false, features = ["tokio", "omni-trait"] }
lsp-types = "0.95"   # 与 async-lsp 0.2.4 的 pin 对齐，不追 0.97
```

- async-lsp 0.2.4（oxalica，MIT/Apache-2.0）：tower Layer 可插拔、通知同步执行
  （tower-lsp 异步通知 out-of-order Issue 反面）、`MainLoop::new_client` 双端对称；
- `tracing` feature 按需追加（示例 `client_builder.rs` 同款层叠）；
- `tokio-util/compat`：tokio IO ↔ futures IO 桥接（inspector.rs 同款）；
- ⚠️ `lsp-types` 必须与 async-lsp 的 `^0.95.0` 对齐（实测 0.95.1），勿手写 0.97；
- 本机 rust-analyzer 不可用（断掉的 rustup 垫片）——E2E 门控 `QAQH_LSP_E2E=1`，
  缺二进制即 skip（M2 接真机时启用）。

## 3. 架构总览

```
 agent actor 线程    │              qaqh-lsp（新 crate）              │
 ┌───────────┐      │  ┌────────────┐  block_on 专属   ┌────────┐ │
 │ tool ring  │─ctx─▶│  │ tool.rs    │ ──runtime──────▶ │ conn   │ │   stdio 子进程
 │ (sync fn)  │      │  │ 聚合分发    │  tokio::select  │ actor  │─┼─▶ rust-analyzer
 │ handler    │◀─result── │ (didOpen+ │  cancel/超时   │(tokio) │ │   async-lsp mainloop
 └───────────┘      │  │ 请求+渲染)  │                └────────┘ │
                    │        ▲ 路由               ▲ 生命周期     │
                    │        │                    │             │
                    │  ┌─────┴────────┐    ┌──────┴──────────┐  │
                    │  │ projection   │    │ LspManager      │──┼── config.toml [lsp.servers]
                    │  │ 行式渲染      │    │ daemon 级单例    │  │   （声明即信任，D4 同 mcp）
                    │  └──────────────┘    └─────────────────┘  │
                    └──────────────────────────────────────────┘
```

核心原则（mcp §3 同构）：**async-lsp 会话住在专属 runtime 里，同步工具环
`block_on` + `tokio::select` 做 cancel/超时**——无 mpsc 桥接层（LSP 请求是
直连 future，比 mcp 的通道转发更薄）。

## 4. crate 设计

```
crates/qaqh-lsp/
├── Cargo.toml
├── tests/
│   └── memory_duplex.rs   # mock server + 内存双工全链（无外部二进制）
└── src/
    ├── lib.rs          # 模块导出 + manager_slot()/install_manager()（OnceLock 槽位）
    ├── manager.rs      # LspManager：(server,root) 连接表 + 扩展名路由 + apply_config
    ├── connection.rs   # ServerConnection：lazy/initialize/索引门/idle/crash
    ├── error.rs        # LspErrorKind（九码）+ async-lsp::Error 映射
    ├── adapter.rs      # 进程组隔离 + secret 插值 + spawn（mcp 同款）
    ├── bridge.rs       # 专属 runtime + E-5 dispatcher + 投影批次（钉底）
    ├── projection.rs   # 行式渲染 + 计数头 + 2KB 截断
    └── tool.rs         # `lsp` 聚合工具定义 + 分发（category=Read）
```

依赖方向（单向）：`qaqh-runtime` → `qaqh-lsp` → `{qaqh-workspace, qaqh-config,
async-lsp, lsp-types}`；不反向依赖 runtime/msgloop。不塞进 `qaqh-mcp`
（MCP 是工具总线，LSP 是工作区语义面，连接键不同）。

## 5. 核心机制

### 5.1 连接生命周期（connection.rs + manager.rs，mcp §5.1 同款状态机）

| 阶段 | 行为 |
|------|------|
| **lazy connect** | 首次调用该 (server, root) 时拉起子进程 → `initialize(workspace_folders=[root])` → `initialized` → 索引门 |
| **索引门** | RA 系 `rustAnalyzer/Indexing`/`cachePriming` 的 `WorkDone::End` 到达即放行；60s 内无 token 则放行（非 RA server 无此 token，不能硬等） |
| **connect 超时** | `startup_timeout_secs`（默认 30s）与 settings 取大；失败 → `LSP_CONNECT_FAILED`，进入 30s 重连冷却 |
| **idle shutdown** | `inflight == 0` 连续 `idle_shutdown_secs`（默认 120s）→ 优雅关闭（`shutdown`+`exit`+停 loop+组杀）；下次调用重连 |
| **执行中失败** | mainloop 退出/IO 错 → `LSP_SERVER_CRASHED`，**当前调用不重试**（mcp 同款语义）；下次调用单次重启 |
| **退出清理** | daemon 退出时 RAII 关闭全部连接并组杀子进程 |
| **daemon 关闭闸门** | `shutting_down` 闸：禁止 lazy connect / reconnect / idle 重启 |

- **文档同步 M1 薄版**：每次查询前读磁盘 → `didOpen`（version 自增；已 open
  则先 `didClose` 再重开，保证查的是落盘态）；常驻 open 不做增量（M2 跟随 edit 写穿）；
- **并发**：`ConcurrencyLayer::default()`（async-lsp 内建请求复用）；
  `connect_serializer` 防并发双 spawn。

### 5.2 路由（manager.rs）

`server_for_extension(path)`：取 `.` 后小写后缀 → 首个声明该扩展名的 server
（BTreeMap 字母序确定性）。`workspaceSymbol` 无文件轴：显式 server 优先，
否则首个已配置 server。

### 5.3 工具投影（tool.rs + bridge.rs，mcp §5.3/§5.4 同款）

- **注册方式**：`lsp` 聚合工具经 `take_projection_batch` 批次钉底（enabled 即
  在场）；回合边界 `merge_dynamic_tools` **增量合并**（不清 MCP 层——mcp
  `replace_dynamic_tools` 是 clear 全量重建，两者语义分离）；
- **模型面**：`action` 枚举 M1 六项（含 `list_servers`）+ `filePath/line/
  character/query/server` 五参数；`description` 短文本无截断（结果侧才截断）；
- **结果渲染**：`resultCount/fileCount` 计数头 + `path:line:col` 行式 +
  2KB 截断（Claude 计数位 + mcp 截断同款）。

### 5.4 权限与审计（M1 决策 L6）

| 关注点 | 处理 |
|--------|------|
| 审批 | `category=Read` 走常规审批（level≥2 直通，level 1 确认）——**不进 D5 快路径**（LSP 是本地只读导航，信任模型干净） |
| 子代理沙箱 | Read 类别 workspace 内自动批准；跨区拒绝（既有语义，零特判） |
| 审计 | 调用经既有 audit 通道；env 值永不落审计与日志（mcp E-6 同款） |
| 取消 | 250ms 轮询 `ctx.cancel`（mcp 同款粒度）；`LSP_CANCELLED` |

## 6. 配置 Schema（config.toml）

```toml
[lsp]
enabled = false   # M1 落地后默认关一版，codegraph 仍是默认语义源
# idle_shutdown_secs = 120

[lsp.servers.rust]
command = "rust-analyzer"
# args = []
# env = { RA_LOG = "${secret:ra_log}" }   # 占位符复用 [secrets.mcp] 段
extensions = ["rs"]                        # 路由键（去点小写去重；冲突先赢 + 日志）
# startup_timeout_secs = 30                # 1..=600（含索引门）
# default_timeout_secs = 30                # 1..=3600
```

- server 名校验与 mcp 同规（`[a-z0-9_-]+`，≤64 字符）；
- `qaqh-config-api` Phase 1 只读 DTO（`lsp` 段；写模型随 UI 管理另立）；
- 配置热重载：`watch` 广播 `[lsp]` 变化 → `apply_config` diff 保连（无外部源重扫）。

## 7. 错误模型

全部走 `ToolResult::error`（mcp §7 同款 JSON）：

| code | 场景 | hint 要点 |
|------|------|-----------|
| `LSP_DISABLED` | `[lsp].enabled=false` | 指向配置键 |
| `LSP_CONNECT_FAILED` / `LSP_CONNECT_TIMEOUT` | 拉起/握手/索引门失败 | 冷却中，检查 command 可执行 |
| `LSP_SERVER_CRASHED` | mainloop 退出/IO 错 | 下次调用自动重启；本次未重试 |
| `LSP_TIMEOUT` | 请求超时 | server 可能仍在执行，勿盲目重发 |
| `LSP_PROTOCOL_ERROR` | 参数错误/读文件失败/server 回 JSON-RPC 错误 | 检查 1-based 坐标 |
| `LSP_NOT_FOUND` | 未知 server/无路由扩展名 | 列出可用名单 |
| `LSP_CANCELLED` | cancel 命中 | 说明已放弃等待 |
| `LSP_SHUTDOWN` | daemon 关闭闸已落下 | 不再受理 |

## 8. 测试计划

| 层 | 内容 | 依赖 |
|----|------|------|
| 单测 | config 解析/校验/往返、路由仲裁、1↔0-based、渲染截断、action 白名单 | 无 |
| 集成（内存双工） | mock server + tokio duplex：hover/definition/references/symbols 全链 + 错误码 | dev-deps（async-lsp server 侧 Router + LifecycleLayer） |
| E2E（门控） | `QAQH_LSP_E2E=1` 时连真实 `rust-analyzer` | 有 RA 的 CI/dev 机（本机缺二进制，默认 skip） |

## 9. 里程碑

| 阶段 | 内容 | 状态 |
|------|------|------|
| **M1** | config schema + LspManager/连接/索引门 + 聚合工具 + 渲染 + 内存集成 + runtime 接线 | ✅ 本 PR |
| **M2** | 增量 didChange 跟随 edit、implementation/callHierarchy、diagnostics 开关、多 root 并存、真机 E2E | 另立项 |

## 10. 风险与开放问题

1. **lsp-types 版本钉死**：async-lsp 0.2.4 钉 `lsp-types 0.95`；升级只动 workspace 一行；
2. **RA 重**：常驻内存数百 MB + 首索引慢 → 30s 启动超时 + 索引门 + idle 120s 短回收；
3. **大结果**：`references` 大仓数千条 → 2KB 截断 + 计数头（模型按需收窄 query）；
4. **prompt injection（接受风险）**：hover 内容直通模型上下文（mcp §10-5 同款接受）；
5. **投影双源语义分离**：MCP 全量重建 vs LSP 增量合并——`lsp` 下线（disabled）时
   dynamic 残留条目由 MCP 下次全量重建清掉（最终一致）；反之 LSP 批次幂等跳过已在册。

## 11. 现有代码接触面（最小侵入清单）

| 文件 | 改动 |
|------|------|
| 根 `Cargo.toml` | workspace members + `async-lsp/lsp-types` |
| `qaqh-types/src/config.rs` | `PersistentLspConfig/PersistentLspServerConfig` + `PersistentConfig.lsp` |
| `qaqh-config/src/config.rs` | `LspConfig/LspServerConfig` + `map_lsp_config` + secret 校验 + save 回写 |
| `qaqh-config/src/dto.rs` + `qaqh-config-api` | `[lsp]` 只读 DTO（Phase 1；写模型另立） |
| `qaqh-workspace/src/runtime.rs` | `+merge_dynamic_tools()`（增量合并，MCP 全量语义不动） |
| `qaqh-runtime` | service 装配 + 热重载 + run_lap 钩子 + UI 直调同步 |
| **新 crate** `qaqh-lsp` | 全部新代码，不反向依赖 runtime |
