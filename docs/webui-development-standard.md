# webUI 前端开发标准（SDK 协议 v0）

> **效力声明**：本文档对一切为 QAQ-Harness 开发前端/renderer 的主体生效——
> 人类与 LLM agent 同等约束。`MUST` = 违反即拒绝合并；`NEVER` = 出现即打回。
> 目的：renderer 是"讲 Ringing V1 的静态产物"，任何偏离本标准的实现
> （自造协议、绕过 lease、本地造历史权威等）都会破坏多前端架构。
>
> **裁决顺序**（冲突时）：后端代码 > `docs/frontend-contract.md` > 本文 > 前端注释。

---

## 0. 架构铁律（NEVER 清单）

| # | 规则 |
|---|---|
| N1 | **NEVER** 引入第二协议：WebSocket / socket.io / 轮询替代 SSE / 自造 RPC。唯一通道是 Ringing V1 HTTP/SSE，同源直连 daemon |
| N2 | **NEVER** 使用浏览器原生 `EventSource`：它无法设置自定义头（daemon 要求 `Authorization` + `X-QAQH-Client-Session-Id`），且 daemon 禁止 query string 传 token。必须 fetch + ReadableStream |
| N3 | **NEVER** 将 token 写入 URL、query、日志或 localStorage/sessionStorage。token 仅内存持有，来源见 §2 连接提供者 |
| N4 | **NEVER** 在未 open 协商前发送命令；所有命令必须携带 open 签发的 `client_session_id` |
| N5 | **NEVER** 用 queries/actions 发送会话生命周期命令——daemon 会以 `invalid_envelope` 拒绝。会话命令只走三频道 command envelope |
| N6 | **NEVER** 实现前端侧"历史权威"：bootstrap/timeline 是唯一真源，前端只做视图缓存；禁止 `ConversationLoadMore`（daemon 设计性拒绝 422） |
| N7 | **NEVER** 假设可访问文件系统/子进程/跨域 API——一切经 Ringing 面；跨 seed 访问会被所有权校验拒绝（403） |

## 1. 连接生命周期状态机

```
[boot] → 解析连接配置(provider) → OPENING → READY(leased) → ATTACHED(seed…)
              ↑                                        │
              │          401 lease_required / renew 失败│
              └──────────── RECONNECTING ←──────────────┘
                            │ server_epoch 变化(daemon 重启)
                            └→ 全量重建：重新 OPEN → bootstrap → 重放 timeline
```

1. **OPENING**：`POST /ringing/v1/clients/open`，`schema`/`version`
   必须与服务端一致（代差返回 426 `unsupported_version`）。
2. **READY**：保存 `client_session_id / server_epoch / lease_ttl_ms /
   renew_interval_ms`；启动续租循环（间隔用 `renew_interval_ms`）。
3. **ATTACH**：`SessionCreate` 后新 seed 自动归属本 lease；恢复已有会话
   也必须先经该 lease 发出 `SessionResume`。
4. **RENEW 失败 / 任意请求 401 `lease_required`** → 回 OPENING；
   成功后按 `server_epoch` 决定增量恢复（同 epoch → timeline 断点续传）
   或全量重建（epoch 变了）。
5. **关闭**：`SessionClose` 命令（幂等）；页面卸载无需 stop 端点
   （`/control/v1/stop*` 是安装器专用，UI 不得调用）。

### 连接配置提供者（provider 抽象，三实现选一）

| 模式 | endpoint | token 来源 |
|---|---|---|
| 浏览器（/debug/ 托管） | `window.location.origin` | `window.__QAQH_DEBUG__.token`（桥脚本注入） |
| 桌面壳（Tauri/Electron/WinUI3） | 宿主 IPC 提供 | 宿主 IPC 提供（读 discovery 或注入） |
| LAN server 直连 | `http://<ip>:<port>` | 用户输入，内存持有 |

## 2. 硬约束速查表

| 约束 | 级别 | 原因 |
|---|---|---|
| 所有 Ringing 请求带 `Authorization: Bearer <token>` | MUST | 统一鉴权层，缺失一律 401 |
| open 之后所有请求带 `X-QAQH-Client-Session-Id: <client_session_id>` | MUST | lease 三层校验之一 |
| 前端构建 base 相对路径（vite `base:'./'`），无内联脚本 | MUST | 页面挂在 `/debug/` 子路径；CSP `script-src 'self'` |
| `command_id` 用 UUID v4，同一逻辑命令重试**复用同一 id** | MUST | daemon 按 fingerprint 幂等；换 id = 可能重复执行 |
| envelope 先本地校验再发送（§4 校验规则） | MUST | 快速失败，避免占用幂等记录后回滚 |
| 收到非本 lease 的 seed 数据立即丢弃并告警 | SHOULD | 所有权边界 |
| token 存活期 = 页面存活期 | MUST | 见 N3 |

## 3. 端点契约速查

前缀：业务面 `RINGING_BASE_PATH = /ringing/v1`；
timeline 面 `RINGING_TIMELINE_BASE_PATH = /ringing/v1`（与业务面同前缀，路由为 `/ringing/v1/sessions/{seed}/timeline` 与 `/ringing/v1/sessions/{seed}/timeline/events`）。

### 3.1 open 协商
```
POST /ringing/v1/clients/open
{ "schema": "qaqh.Ringing", "version": 1,
  "client_instance_id": "<uuid-v4>" }
→ 200 { accepted:true, client_session_id,
        server_epoch, lease_ttl_ms, renew_interval_ms }
→ 426 { code:"unsupported_version" }
```
版本协商由 `schema`/`version` 承担（能力矩阵已于 2026-08 移除：客户端与
daemon 同链路发布，无部分能力客户端存在）。

### 3.2 命令（唯一会话命令通道）
```
POST /ringing/v1/commands/{control|conversation|tool}
{ schema:"qaqh.Ringing", version:1, channel:"<同 path>",
  command_id:"<uuid-v4>", client_instance_id,
  client_session_id, seed? , expected_revision?,
  command: { channel:"<同 path>", type:"<命令名>", ...参数 } }
→ ack { command_id, status:"accepted"|"rejected"|"dispatch_failed"|"timed_out",
        code?, message?, retry_after_ms? }
```
- envelope 本地校验规则（不过则不发）：schema/version 匹配；
  `command.channel === path channel`；`command_id/client_instance_id` 非空；
  除 `control_session_create` 外必须带 `seed`。
- `accepted` 仅代表进入正确 actor——终态以事件/receipt 为准。

### 3.3 receipt 查询
```
GET /ringing/v1/commands/{command_id}
```
`dispatch_failed/timed_out` 或断线重连后用它取终态。

### 3.4 事件 SSE（三频道）
```
GET /ringing/v1/events/{control|conversation|tool}
```
帧为 batch 信封：
```json
{ "schema":"qaqh.Ringing","version":1,"channel":"conversation",
  "seed":"…","server_epoch":"…",
  "from_stream_seq":N,"to_stream_seq":M,
  "envelopes":[ { "event_id":"…","causation_id?":"…","stream_seq":N,
                  "channel_seq":n,"session_seq":m,
                  "event":{"type":"turn_started", …} } ] }
```
按 `stream_seq` 单调校验，跳号即触发 §1 的重建流程。

### 3.5 timeline（transcript 权威）
```
快照分页  GET /ringing/v1/sessions/{seed}/timeline?before_turn=<id>&limit=<n>
事件流    GET /ringing/v1/sessions/{seed}/timeline/events
断点续传  请求头 Last-Event-ID: "<server_epoch>:timeline:<timeline_seq>"
          （fetch 无法改头的场景可用 query 兜底 ?last_event_id=…）
```
- bootstrap（3.6）给全量历史，快照分页只用于向上翻页渲染。
- transcript 渲染顺序以 timeline writer 为准，不按事件到达序。

### 3.6 bootstrap
```
GET /ringing/v1/sessions/{seed}/bootstrap
```
返回该会话完整持久化历史。**替代**已废弃的 load_more（发它得 422
`unsupported_command`，这是设计而非缺陷）。

### 3.7 只读查询 queries
```
POST /ringing/v1/queries/{name}     body: { seed?, … }
```
白名单内方法（其余 404）：`daemon.version session.list session.meta
session.activity session.dashboard session.get_activity workspace.get
workspace.status workspace.list fs.list fs.read config.load
skills.list_tools todo.status plan.read plan.context_stats
stats.token_usage git.diff git.branch git.branches git.file_diff`
其中 `session.meta/dashboard/get_activity workspace.get todo.status
plan.* git.*` 必须带 `seed`（否则 400）。错误统一
`{"code":"query_failed","message":…}`。

### 3.8 辅助动作 actions
```
POST /ringing/v1/actions/{name}     body: { action_id:"<uuid-v4>", seed?, … }
```
允许前缀：`git. workspace. config. profile. skills. stats. plan. todo.
subagent.` 及单条 `session.set_tool_mode`。缺 `action_id` → 400。
**会话/交互命令出现在这里会被显式拒绝**（见 N5）。

### 3.9 内容引用 content（附件）
```
上传 POST /ringing/v1/content      multipart: seed / media_type / content(file)
下载 GET  /ringing/v1/content/{content_id}?seed=<seed>
```
响应 `{content_id, media_type, sha256, truncated}`；`sha256 === content_id`。
命令里只传 `ContentRef{content_id, sha256, media_type}`，**绝不传本地路径**；
哈希不符报 `attachment_mismatch`，跨 seed 读取 403 `content_forbidden`。

## 4. 错误码字典与 HTTP 映射

| HTTP | code | 含义 | 前端动作 |
|---|---|---|---|
| 401 | `lease_required` | 未 open / lease 过期 | 回 OPENING，重建后 resume |
| 401 | （无 code，Bearer 错） | token 错 | 提示用户，禁重试循环 |
| 400 | `invalid_envelope` / `channel_mismatch` / `missing_seed` | 我方 bug | 上报，不得原样重发 |
| 409 | `duplicate_command_mismatch` | 同 id 不同载荷 | 必然是实现 bug，上报 |
| 422 | `unsupported_command` | 如 load_more | 删掉调用方代码，走 bootstrap |
| 426 | `unsupported_version` | 版本代差 | 展示"需更新"，停重试 |
| 502 | `dispatch_failed`(ack) | worker 转发失败 | 可换 id 重试一次，再败上抛 |
| 403 | `content_forbidden` | 跨 seed | UI 提示，勿重试 |

## 5. fetch-SSE 参考实现（TypeScript，可直接抄）

```ts
export async function consumeSse(
  url: string,
  headers: Record<string, string>,
  lastEventId: string | undefined,
  onEvent: (id: string, data: string) => void,
  signal: AbortSignal,
): Promise<"eof" | "aborted"> {
  const resp = await fetch(url, {
    headers: { ...headers, Accept: "text/event-stream",
               ...(lastEventId ? { "Last-Event-ID": lastEventId } : {}) },
    signal,
  });
  if (!resp.ok || !resp.body) throw Object.assign(new Error(`sse ${resp.status}`), { status: resp.status });

  const reader = resp.body.pipeThrough(new TextDecoderStream()).getReader();
  let buf = "", id = "", data: string[] = [];
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return "eof";
      buf += value;
      let idx;
      while ((idx = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, idx); buf = buf.slice(idx + 1);
        if (line === "") {                       // 事件边界
          if (data.length) onEvent(id, data.join("\n"));
          id = ""; data = [];
        } else if (line.startsWith("id:"))       id = line.slice(3).trim();
        else if (line.startsWith("data:"))       data.push(line.slice(5).trimStart());
        // 注释行(:…) 与其他字段忽略；多行 data 按规范拼接
      }
    }
  } catch (e) {
    if (signal.aborted) return "aborted";
    throw e;
  } finally { reader.releaseLock(); }
}
```
外层连接循环职责：指数退避重连（尊重 `retry:` 字段如有）→ 每次把最新
`Last-Event-ID` 传回 → 收到 401 时切换到 §1 重建流程而不是继续重连。

## 6. SDK 分层与目录约定

```
renderer/src/sdk/
  connection/provider.ts    # 三模式连接配置（§1 表）
  transport/http.ts         # 统一 fetch 封装：双头注入、401 拦截、超时
  transport/sse.ts          # §5 参考实现 + 重连状态机
  protocol/types.ts         # envelope/ack/batch/event 类型——手工镜像
                            # qaqh-ringing，文件头注明"改动须对照后端 PR"
  protocol/version.ts       # RINGING_SCHEMA/VERSION 常量
  state/projection.ts       # timeline/event → 视图模型（唯一写入口）
  commands/*.ts             # 按频道封装的命令构造器（含幂等 id 管理）
```
- 协议类型**单一来源**：全部 import 自 `protocol/types.ts`，禁止散落字面量。
- 状态投影单向流：SSE/bootstrap → projection → UI；UI 不直接改缓存。

## 7. 验收自检清单（agent 合并前逐项执行）

```bash
TOKEN=…; BASE=http://127.0.0.1:<port>
# ① open 协商
curl -s -X POST $BASE/ringing/v1/clients/open -H "Authorization: Bearer $TOKEN" \
  -d '{"schema":"qaqh.Ringing","version":1,"client_instance_id":"t1"}'
# 期望 accepted:true 且拿到 client_session_id
# ② 无 lease 发命令 → 必须 401 {"code":"lease_required"}
# ③ SessionCreate → ConversationSendMessage → conversation SSE 上出现 turn_started
# ④ 刷新页面（丢 lease）→ 自动 re-open → timeline 断点续传不丢消息
# ⑤ 带 query token 访问 events → 必须 401（验证未违反 N2/N3）
# ⑥ 发 conversation_load_more → 必须 422（验证未违反 N6）
# ⑦ 非 loopback 访问 /debug/ → 403（部署形态检查）
```

## 8. 反模式黑名单（LLM 歧路拦截）

| 歧路 | 为什么错 |
|---|---|
| "加个 WebSocket 更实时" | 违反 N1；WS 数据协议已被 M3 拆除，SSE + batch 信封即权威通道 |
| "EventSource 简单" | 违反 N2；带不了头，daemon 直接 401 |
| "轮询 queries 代替 SSE" | 违反 N1；浪费且丢因果链（causation_id 无法还原） |
| "token 放 localStorage 方便热载" | 违反 N3；桥脚本每次注入新值，持久化只会放大泄漏面 |
| "load_more 翻旧消息" | 违反 N6；422 是设计，翻页用 timeline `before_turn` |
| "actions 里发 session.send_message" | 违反 N5；400 拒绝，命令只走 envelope |
| "前端存一份 messages.jsonl 缓存到磁盘" | 违反 N6；bootstrap 永远可得，磁盘副本必然漂移 |
| "直接 fetch file:// 或 localhost 其他端口" | 违反 N7/N1；同源之外的一切都是架构外 |
