# QAQ-Harness

AI 编码代理的跨平台 **Rust 后端核心**(monorepo,19 个 workspace 成员)。单个常驻 daemon 承载多会话对话循环、LLM 网关、22 个内置工具、Agent Skills 与子代理隔离执行;Windows 桌面壳(WinUI3)/ TUI / Web 壳位于独立仓库,通过统一的 **Ringing V1** HTTP/SSE 协议接入。

- Edition 2024 · License MIT · 状态:alpha
- 无 axum/框架依赖:HTTP/SSE 为手写 tokio TCP 实现,release 产物为静态 CRT 单文件 exe(`opt-level=z` + LTO + strip)

## 架构总览

```
 外部壳(独立仓库):WinUI3 桌面 / TUI / Web
        │  qaqh-client(HTTP/SSE,Bearer token + client lease)
        ▼
 ┌─ qaqh-daemon ──── 常驻单实例进程(discovery: {data_dir}/daemon.json)─┐
 │   ringing_http:POST /clients/open · /commands/{control|conversation|tool} │
 │                SSE 事件三频道 + per-session timeline 流                  │
 │   QaqhService:JSON 方法分发(session.* / workspace.* / fs.* ...)          │
 │   AgentRegistry ── spawn/close ── actor(每会话一个线程,含子代理沙箱)     │
 │   RingingHub:事件双投(fanout 给所有订阅者,带 causation)                 │
 │        │                                                                 │
 │   qaqh-msgloop TurnEngine:用户输入 → gate → 工具环 → 回合完成 → compact  │
 │        ├─ qaqh-gate      LLM 网关(OpenAI Chat / Responses,SSE 流式+重试) │
 │        ├─ qaqh-workspace 22 个工具执行 + 四级权限准入 + 审计              │
 │        │      └─ serve 子进程(local 原生 或 WSL,HTTP 工具后端,可回退)    │
 │        └─ qaqh-skills / qaqh-subagent / qaqh-lsp                          │
 └──────────────────────────────────────────────────────────────────────────┘
        │
        ├─ {data_dir}/  全局数据根(Windows: %USERPROFILE%\.deepx;
        │               Linux/macOS: ~/.config/qaqh;可用 QAQH_DATA_DIR 重定向)
        │     ├─ config.toml + secrets.toml(API key 不落明文,Windows DPAPI 加密)
        │     ├─ daemon.json / daemon.lock(发现 + 单实例锁)
        │     └─ sessions/{8位hex seed}/ meta.json · messages.jsonl · todo.json …
        └─ <workspace>/.deepx/  项目级目录:PLAN.md · trash/ · skills/
```

## Workspace 成员

| 分层 | Crate | 职责 |
|---|---|---|
| 领域/线协议 | `qaqh-domain` | 中立 DomainCommand/DomainEvent(不依赖 wire 类型) |
| | `qaqh-ringing` | Ringing 线协议:envelope / ack / batch / snapshot / content ref / worker frame / 能力协商 |
| | `qaqh-proto` | DaemonDiscovery(daemon.json)+ 回合投影等共享模型 |
| 运行时 | `qaqh-runtime` | daemon 应用运行时:`QaqhService` 方法分发、AgentRegistry、actor、RingingHub |
| | `qaqh-msgloop` | 对话循环引擎:输入处理 → gate 快照 → 工具审批/执行 → 回合完成 → 自动压缩 |
| | `qaqh-message` | 消息存储状态机(Turn/Step 结构、Effect 驱动、ContextFlow 摄取编排) |
| | `qaqh-daemon` | headless 入口二进制(`run` / `server` / `status` / `stop`) |
| 会话/配置 | `qaqh-session` | SessionManager 单例:index/meta/消息 JSONL 持久化、归档、临时会话、WorkspaceStore |
| | `qaqh-types` | 共享类型、平台路径(data_dir/marker)、tool_mode 定义 |
| | `qaqh-config` | Config 加载/保存事务、provider 注册表、system prompt、secrets |
| LLM | `qaqh-gate` | LLM API 网关:OpenAI Chat Completions 与 Responses 双协议、自研 SSE 解码器(~143MB/s)、429/5xx 指数退避重试、reasoning/tool-call 流提取 |
| 工具 | `qaqh-workspace` | 工具执行框架 + 19 个内置工具 + 权限/审计 + `serve` HTTP 工具后端二进制 |
| | `qaqh-subagent` | `spawn_subagent`:派生隔离 Ringing 子会话(in-process 守护线程,ephemeral,结果异步注入父会话) |
| | `qaqh-lsp` | LSP 客户端库(hover/definition/symbols/diagnostics,默认 rust-analyzer);纯库,尚未暴露为工具 |
| | `qaqh-skills` | Agent Skills 发现/解析/激活(SKILL.md + YAML frontmatter,catalog 渐进披露) |
| 客户端/周边 | `qaqh-client` | daemon HTTP/SSE 传输层:discovery → open 协商 → 三频道 SSE + timeline 流 + lease 自愈;供外部壳复用 |
| | `qaqh-update` | 更新目录/规划/应用引擎(full / 文件级增量 / 组件 artifact,.previous 回滚) |
| | `qaqh-gate-testui` | gate 可视化测试 UI(mock OpenAI SSE 服务 + 内嵌网页,6 个场景) |
| 极简模式 | `dsh-minimal-mode` | deepseek-harness minimal-mode 复刻:`bash_v2`(持久 PTY)+ `str_replace_editor`,输出逐字对齐 |

## 核心概念

### Ringing V1 协议
客户端先 `POST /clients/open` 能力协商,获得 `client_instance_id / session_id / lease`;命令按 control/conversation/tool 三频道 POST,事件经对应频道 SSE 推送(batch 信封,16MB 帧上限);另有 per-session timeline SSE(快照页 + Last-Event-ID 断点续传)。鉴权三层:Bearer token + client-session lease + seed 所有权。worker 已收敛为 daemon 内线程,但保留完整 frame 边界语义,未来可无感切回子进程隔离。

### 会话与存储
- seed 为 8 位 hex;磁盘布局 `sessions/index.json` + `sessions/{seed}/{meta.json, messages.jsonl, compact-context.json, todo.json}`,全部 temp+rename 原子写
- 归档会话保留磁盘可恢复;**临时会话**(子代理)关闭即整目录删除,零残留
- 上下文超过 `auto_compact_threshold`(默认 context_limit × 0.75)自动摘要压缩;原始 JSONL 不可变归档,resume 走 compact-context 检查点链(fail-closed)

### 工具与权限
22 个工具分四类权限类别(Read/Write/Exec/Net),四级权限档位:

| Level | 名称 | 行为 |
|---|---|---|
| 1 | MaxLockdown | 一切调用需确认 |
| 2 | ReadFree | 读放行,写/exec/net 需确认 |
| 3 | WorkspaceFree | 工作区内写放行;跨区写一次性信任文件夹;exec/net 仍需确认 |
| 4 | Unrestricted | 默认,无检查 |

- 审批闭环:`PermissionChallenge`(一次性,TTL)→ UI 确认 → 不可伪造的授权凭证执行;支持 trust folder
- 写入防漂移:read/edit/write 维护文件 hash 账本,失配报 `STALE_FILE`;dry-run 暂存 pending_id 后 `confirm_apply` 直提
- 子代理沙箱:读写自动批准,exec/net 自动拒绝,无弹窗通道
- 工具模式档位:`standard` / `minimal` / `minimal:dsh` / `custom`(白名单 + 模型面投影双层闸门)

### Provider 与配置
内置 11 家 provider 注册表(deepseek/qwen/glm/kimi/mimo/minimax/doubao/openai/openrouter/deepseek-web/opencode-go),endpoint 级声明协议(openai/responses)、thinking 字段、缓存字段等能力,新 provider 只加配置不改网关代码。`config.toml` 支持命名 profiles;API key 存 `secrets.toml`(Windows DPAPI 加密,其余平台 0600 明文),config 中只留 `"set"` 标记。

### 技能系统
扫描项目 `.deepx/skills > .agents/skills > skills`,再用户级同名目录;SKILL.md frontmatter 必填 name/description。catalog 只注入元数据(progressive disclosure),正文仅在 `$mention` 或 `skills activate` 时经类型化 effect 通道注入 `<skill_context_envelope>`;allowed-tools 永不自行授予权限。

## 快速开始

```powershell
# 构建(release,产出 daemon 与 workspace serve 二进制)
just build-daemon

# 开发运行(headless daemon)
just dev

# 手动管理 daemon
cargo run -p qaqh-daemon -- run      # 默认启动
cargo run -p qaqh-daemon -- server   # 局域网 headless 模式(远端壳直连)
cargo run -p qaqh-daemon -- status   # 读 daemon.json 探活
cargo run -p qaqh-daemon -- stop

# 工具直调(CLI)
qaqh-workspace list                      # 列出全部工具定义
qaqh-workspace read '{"path":"src/lib.rs"}'
qaqh-workspace serve --port N --token T  # HTTP 工具后端(daemon 自动拉起)

# gate 测试 UI(mock OpenAI + 浏览器页面)
cargo run -p qaqh-gate-testui            # http://127.0.0.1:3000
```

## 开发工作流

| Recipe | 作用 |
|---|---|
| `just check` | `cargo check --workspace` |
| `just clippy` | `cargo clippy --workspace --all-targets` |
| `just fmt` | `cargo fmt --all --check` |
| `just test` | `cargo test --workspace` |
| `just build-daemon` | release 编译核心二进制 |
| `just status` / `clean` | 产物检查 / 清理(仅 Windows 的 status) |
| `just sync-version` | 从 `version.txt` 同步版本到 Cargo.toml + package.json |

- Clippy 全仓 deny `unwrap_used`、`string_slice`(少数 crate 局部豁免并注明理由)
- 测试规模约 **860 个**(123 个内联 cfg(test) 模块 + 23 个集成测试文件);触碰全局状态的测试统一走 `TEST_RUNTIME_SERIAL` 互斥串行
- 3 个重型 e2e 标记 `#[ignore]`,需手动触发,如 rust-analyzer 冒烟:
  `cargo test -p qaqh-lsp --test e2e -- --ignored --nocapture`
- 测试/多实例用 `QAQH_DATA_DIR` 环境变量整体重定向数据根
- 无 CI 配置,质量门禁即上述本地 recipe 链

## 版本管理

- `version.txt` 是唯一版本真源,经 `scripts/sync-version.ps1` 写入 Cargo.toml 与 package.json
- 对外 User-Agent 版本(`QAQH_USER_AGENT = "qaqharness/<ver>/`)在 `crates/qaqh-types/src/platform.rs` 宏内手工维护,与 cargo 包版本解耦(不带 rc/预发布后缀)
