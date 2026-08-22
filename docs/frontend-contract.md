# Ringing V1 前端契约与协同规则

> 本文档是 daemon ↔ 前端（WinUI3 / Tauri / Electron / TUI / 浏览器 webUI）的
> 契约声明与变更治理规则。**契约真源在本仓库**；前端仓库对着本文档实现，
> 变更走本文档第 3 节流程。

## 1. 冻结契约（breaking change 即拒绝合并）

| 契约项 | 定义位置 | 说明 |
|---|---|---|
| Ringing V1 端点集 | `qaqh-daemon/src/ringing_http.rs` | open/leases/renew、commands/{control,conversation,tool}、events/{channel} SSE、timeline、content 上传下载、queries、actions |
| envelope / ack 载荷形状 | `qaqh-ringing`（`RingingCommandEnvelope` 等） | 字段名、判别式、错误 code 集合 |
| 能力协商 | `POST /ringing/v1/clients/open` | schema=`"qaqh.Ringing"`, version=1；现有四能力：`Ringing_v1 / Ringing_batch_v1 / Ringing_bootstrap_v1 / Ringing_command_status_v1` |
| 会话头 | `X-QAQH-Client-Session-Id` | 所有 lease 校验端点依赖；名称冻结 |
| discovery 文件 | `{data_dir}/daemon.json` (`DaemonDiscovery`) | 字段集合与语义；注意 endpoint 当前值格式为遗留的 `ws://host:port/control/v1`（见 issue 待办）|
| 生命周期端点 | `POST /control/v1/stop(-if-idle)` | 安装器断电协议：200 = 可安全关闭，409 = 忙碌 |
| qaqh-client 公开 API | `crates/qaqh-client` | 外部壳直接依赖的 Rust SDK |

## 2. 演进规则

- **加法兼容**：新增 capability 名称、新增只读 query/action 方法、envelope 新增
  带 `#[serde(default)]` 的可选字段——允许单向合入，前端按需跟进。
- **破坏性变更**：必须 bump `RINGING_VERSION` 或 `CONTROL_PROTOCOL_VERSION`
  并保持旧版本可解析（或经能力协商分流），双侧 PR 同步合入后才可发版。
- **SSE 传输约束（对浏览器前端至关重要）**：事件端点要求
  `Authorization: Bearer <token>` + `X-QAQH-Client-Session-Id` 自定义头，
  **且禁止 query string 传 token**。浏览器原生 `EventSource` 无法设置自定义
  头 → **前端必须用 fetch + ReadableStream 消费 SSE**，不得使用 EventSource。

## 3. 变更流程

1. 后端侧在本仓库开 RFC issue，注明影响的契约面与新 capability 名；
2. 实现合入本仓库（capability 协商保证旧客户端不受影响）；
3. 前端仓库对着 RFC issue 开 PR，全部条目勾完后关闭；
4. 双侧 tag 同步发版。

## 4. webUI 托管规范（daemon ↔ 浏览器）

- 入口 `GET /debug/`：静态托管 renderer 产物，**仅限 loopback 来源**
  （403 否决其他来源）；LAN 场景用 SSH 隧道（`ssh -L`），不开公网口子。
- token 注入：入口页注入 `<script src="./__qaqh_bridge__.js">`，其内容为

  ```js
  window.__QAQH_DEBUG__ = {"token":"<daemon-token>","nonce":"<hex>"};
  ```

  **前端连接配置必须抽象出 provider**：桌面壳模式从 IPC/preload 取
  endpoint+token；浏览器模式读 `window.__QAQH_DEBUG__` 且以同源为 endpoint。
- 路径前缀 `/debug/` 下运行 → 前端构建产物必须使用相对 base
  （vite `base: './'`），资源引用不得写绝对路径。
- 生产布局：daemon 与匹配版本的 renderer 产物同目录分发
  （`resources/out/renderer`），避免版本漂移触发能力协商拒绝。
