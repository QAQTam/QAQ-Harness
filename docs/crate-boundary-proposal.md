# Crate 边界整改提案（boundary-reform 提案）

> 状态：**PROPOSAL——待制成标准 PLAN.md**（2026-08-30 提出）。
> 定位：本文档是**问题证据与目标边界的唯一权威来源**；后续 PLAN.md 负责将其翻译为
> 可执行的批次 / PR 切分 / 逐项验收流程，实现偏离需先改文档。
> 动机：第二轮全仓 crate 职责边界审查（CodeGraph 依赖图 + 全量 grep 实证）发现：crate 划分与
> 各自声明职责大面积脱节——msgloop 实为"半个 agent"、message 兼职持久层与工具执行器、
> workspace 兼职 HTTP 服务端、runtime 与 msgloop 之间存在已被架空的假边界。
> 风格：**激进（aggressive）**——宁可多拆，不留"可辩解"的灰色地带；但**顺序保守**——
> 先搬家、后合并、再拆全局，每阶段独立全绿、独立可回退。
> 契约约束：`docs/frontend-contract.md`（冻结，不触碰）、`docs/config-revamp-plan.md` §2
> （K1–K3 继续有效）、`qaqh-ringing` wire 协议（`schema: "qaqh.Ringing", version: 1`，冻结）。

---

## 0. 总览

**核心判断（一句话）：先搬家，再合并。**

- 合并（msgloop 收编进 runtime 作为 `agent/` 模块组）是正确终局；
- 但直接合并会把四个"寄居者"（授权管线 / 技能运行时 / 投影 / 冲突检测）连同 message 的
  两处越界一起搬进新家，之后更难拆；
- 因此顺序固定为：**Phase 1 搬家 → Phase 2 合并 → Phase 3 拆全局单例 → Phase 4 周边归位**。

结构病（本轮审查归纳，编号供 PLAN.md 引用）：

| # | 结构病 | 一句话 |
|---|---|---|
| S1 | **假边界** | runtime 是 msgloop 的唯一消费方，却天天伸手进它内部（构造 `AgentState`、消费 `WorkerCommand`/`WriterEvent`）——被唯一邻居翻抽屉的 crate 不是边界，是文件夹 |
| S2 | **loop 即垃圾抽屉** | 授权审批、技能状态机、timeline 投影、dashboard、会话簿记、配置磁盘 IO 全部长在 msgloop 里，因为它没有别的地方可去 |
| S3 | **Effect 自破** | message 自我声明"push_* 返回 Effect，副作用归调用方"，但持久化（5 处）与工具执行（闭包）绕过了 Effect |
| S4 | **全局单例当跨 crate 通道** | `SessionManager::global()` 与 `qaqh_workspace::runtime::*` 进程级全局让 crate 之间不需要显式接口就能互相伸手——几乎每条越界行为的共同机制 |

工作量粗估（供 PLAN.md 校准）：Phase 1 ≈ 3–4 天、Phase 2 ≈ 2 天、Phase 3 ≈ 3–5 天、
Phase 4 ≈ 2–3 天；合计 **10–14 天**（含测试迁移），与第一轮 PLAN（42 项 / 9.5 天）量级相当。

---

## 1. 证据清单（全部 file:line 实证，grep/CodeGraph 可复现）

> 复现方式：`grep -rn` 命令逐条列出在 §4 各项"验收标准"中；依赖关系由
> `crates/*/Cargo.toml` 与 CodeGraph `callers` 确认（索引 242 文件 / 7144 节点）。

### 1.1 qaqh-msgloop（声明职责："message-loop driver"，实际 ~12k 行半全仓）

| ID | 位置 | 越界行为 |
|---|---|---|
| B1 | `ringing_v1/engine_tool.rs`（1018 行）；`engine_tool.rs:68` | 完整工具**授权审批管线**长在循环引擎里：构造 `ToolInvocation`、`authorization::admit`、审批 challenge 发放、`ApprovalError::{Rejected,Expired,MissingOrReplayed}` 分类、`ToolCategory`×`PermissionRisk`→信任级别映射；循环启动时还直接 `TrustedFolderSet::load("")` 读磁盘 |
| B2 | `state/skill_context.rs`（~600 行） | 技能**运行时状态机**（catalog 快照 / 激活去激活 / token 预算 `MAX_TOTAL_SKILL_TOKENS=64KB`）整体在 loop crate，而 `qaqh-skills` 本体只有加载与纯函数 |
| B3 | `services/conflict.rs` | 工具**写冲突检测**在 loop crate（第一轮 PLAN M1 的 patch/apply_patch 匹配 bug 也在这里，搬家时顺带修） |
| B4 | `util::project_turns_from_messages`；`services/dashboard.rs` | **投影逻辑**住在 loop：前者被 runtime 的 `ringing/conversation_snapshot.rs:21` 调用（runtime 自己的 `ringing/projection.rs` 才是投影层的家）；后者把 `workspace::runtime::files_read()` 组装成 `qaqh_proto::DocInfo`（UI 投影） |
| B5 | `ringing_v1/engine_session.rs:80` | 循环引擎直接 `qaqh_config::Config::load()` **读磁盘**（绕过 config-revamp P2-D1 建立的 watch 广播），随后还调 `qaqh_workspace::workspace::load_session_workspace()` 操作 workspace 全局 |
| B6 | `engine_title.rs:51,86`、`engine_misc.rs:70,100`、`engine_compact.rs:397`、`turn_lap/gate.rs:447`、`types.rs:453` | 循环引擎 6 处直接写 `SessionManager::global()`（title / context_stats / usage / mode）——loop 在管会话簿记 |
| B7 | `engine_title.rs:183`、`engine_compact.rs:224`、`turn_lap/gate.rs:636` | 三处重复 `qaqh_config::registry::find_endpoint`——provider 解析逻辑在 loop 内发散 |

### 1.2 qaqh-message（声明职责："消息状态机，Effect 归副作用"，实际兼职两层）

| ID | 位置 | 越界行为 |
|---|---|---|
| A1 | `store.rs:260,270,273,985,987`（`flush_meta`/`snapshot_full`） | `MessageStore` 5 处直接调 `SessionManager::global()` 落盘（`save_append`/`save_full`/`update_meta`/`update_compact_context`/`save_compact_context`）；`Effect` 枚举中**没有任何持久化变体**——S3 的实锤 |
| A2 | `store.rs:126,808,972` | 消息层持有并调用 `ToolExecutorFn`（`Box<dyn Fn(ToolExecRequest)->ToolExecReport>`），`execute_tools_batch()` 由 MessageStore **直接驱动工具批量执行**——工具执行编排是 loop 的核心职责 |

### 1.3 qaqh-runtime ↔ qaqh-msgloop（假边界本体）

| ID | 位置 | 越界行为 |
|---|---|---|
| C1 | `runtime/src/actor.rs:40,75,103–135,163,170,229`；`registry.rs:107–108` | daemon 侧直接构造 `AgentState::new(...)`、调 `agent_tool_registrars()`、`Loop::from_channels`，actor 协议类型（`WorkerCommand`/`WriterEvent`/`CancelToken`）**就是** msgloop 的 `ringing_v1::types` |
| C2 | 全仓 `Cargo.toml` grep | **msgloop 的唯一消费方是 runtime**——单消费方 + 内部伸手 = S1 |
| C3 | `ringing/worker.rs` 头注释、`ringing_v1/wire.rs` 头注释 | `Loop::new_ipc`（独立进程 + stdin/stdout）已退役，生产路径为 `from_channels` 进程内 actor——**msgloop 作为独立 crate 的存在理由（子进程隔离）已消失** |
| C4 | `git log --oneline`（近期） | `refactor(ringing)!: worker 边界消息去除 wire 线格式`、`服务 RPC 三面归一`、`open 握手移除能力矩阵`——假边界的协调成本在提交史中持续可见 |

### 1.4 qaqh-workspace（声明职责："ToolManager 工具框架"，实际兼职服务端）

| ID | 位置 | 越界行为 |
|---|---|---|
| D1 | `src/main.rs`、`src/serve.rs` | crate 自带二进制：CLI runner + **HTTP tool service**（路由 / 端口 / Bearer 鉴权 / 单线程串行队列）。**注意：serve 是生产路径**——`runtime/workspace_supervisor.rs:135,227` 以 `Child` 拉起 serve 子进程（local / WSL 两模式），subagent 经 `POST /subagent` 向其注册进程记录。故处置是"约束"而非"删除" |
| D2 | `runtime.rs:250–261`、`read_image/mod.rs:79` | 工具运行时直接 `Config::load()` **读磁盘**判断 image 能力，绕过 watch 广播——每次工具调用重读配置文件 |
| D3 | `workspace.rs:36`、`file_query.rs:91` | 工具的 cwd 由 `qaqh_session::workspace::session_workspace_*` 决定——工具 crate 反向依赖会话注册表 |
| D4 | `execution.rs:389`、`todo.rs:112`、`code_delta.rs:24` | 工具 crate 组装 `qaqh_proto::{SkillEffect 之外的投影记录 CodeDeltaRecord/TaskInfo}`——协议投影构建散落在工具层 |
| D5 | `runtime.rs`（`set_context`/`set_mode`/`files_read`）、`workspace.rs`（`set_process_workspace`） | 进程级全局状态组；serve 单线程串行的根因，也是 msgloop（`engine_tool.rs:151`）能伸手进来的洞 |

### 1.5 qaqh-session 及小项

| ID | 位置 | 越界行为 |
|---|---|---|
| E1 | `session/src/workspace.rs`（`WorkspaceStore`，写 `{data_dir}/workspaces.json`） | "会话工作区"概念三方各持一份全局：session 持久化、msgloop `engine_session` 加载、workspace 消费 cwd——无唯一属主；且与 `qaqh-workspace` crate 命名直接冲突 |
| F1 | `config/src/prompt.rs`（含 `detect_shells()` 环境探测） | 系统提示词编译住在配置 crate；消费方仅 msgloop `lifecycle.rs:198,229,267`（读）+ runtime `registry.rs:67,87`（写 `OS_INFO`/`TOOLS_INFO`） |
| F2 | `gate/src/guard.rs` | 用户输入**合规过滤**（blocked keywords）住在"LLM API 网关"；唯一消费方是 msgloop `engine_input.rs:91` |
| F3 | `subagent/src/lib.rs:642`；`host.rs:18,22` | legacy HTTP/SSE fallback 依赖完整 daemon 客户端；host trait 的 `ContentRef`/`EventBatch` 从 `qaqh_client` re-export——工具层与传输类型绑定（in-process host 已是生产主路径） |

### 1.6 守住边界的 crate（本轮不动，作为对照基线）

`qaqh-types`、`qaqh-domain`（硬规则：不依赖 wire/proto）、`qaqh-ringing`（纯 wire 类型零 IO）、
`qaqh-proto`（已自我收缩为投影模型 + DaemonDiscovery）、`qaqh-config`（除 F1 外）、
`qaqh-client`、`qaqh-daemon`（纯装配）、`qaqh-gate`（除 F2 外）、`qaqh-skills`（待接收 B2）、
`qaqh-config-api`。

---

## 2. 目标边界（终局态）

```text
                        ┌────────────────────────────────────────────┐
                        │ qaqh-runtime（合并后，两个模块组）           │
                        │                                            │
  RingingCommand ──────►│  agent/   ← 前 msgloop：loop_core、engines、 │
  （wire 层解码后）      │            turn_lap、AgentState、           │
                        │            paced_emitter、input guard( F2 ) │
                        │            prompt 编译( F1 )                │
                        │     规则：不得 import ringing/ 内部，        │
                        │     只经 agent 入口 handle 收发命令/事件     │
                        │                                            │
                        │  ringing/ ← hub、router、outbox、journal、   │
                        │            projection(+dashboard B4)、lease │
                        └────────────────────────────────────────────┘
                              │ 生成 Effect            ▲ 事件流
                              ▼                        │
  ┌──────────────┐  Effect::Persist   ┌──────────────────────┐
  │ qaqh-message │───────────────────►│ 宿主（runtime/agent） │
  │ 纯内存状态机  │  ToolExecRequest   │ 执行落盘与工具执行     │
  └──────────────┘◄───────────────────┤                      │
                                     └──────────┬───────────┘
                                                │ authorize() 单一门面
                                                ▼
                                     ┌──────────────────────┐
                                     │ qaqh-workspace       │
                                     │ 策略+审批流程+冲突检测 │
                                     │ (+ B3；M1 顺带修)     │
                                     └──────────────────────┘
```

**两条纪律**（终局态的全部判据，比"几个 crate"重要）：

1. 只有 `agent/loop_core` 里有 while 循环；message 是纯内存状态机 + Effect；组装是纯函数；
   gate / workspace 是"请求进 / 结果出"的边界。
2. 持久化、队列、多客户端、可靠投递**一概不在 agent 里**——agent 对外只暴露
   "Effect + 事件流"，落盘与排队由宿主（runtime 侧）执行。

**crate 增删表**：

| 动作 | crate | 说明 |
|---|---|---|
| 删除 | `qaqh-msgloop` | 收编进 `qaqh-runtime/src/agent/`（C3：子进程隔离理由已消失；C2：唯一消费方） |
| 不新增 | — | B1/B3 归 workspace、B2 归 skills、B4 归 runtime/ringing，皆有现成家，不造新 crate |
| 可选（开放决策 Q1） | `qaqh-tool-server` | 仅当 PLAN.md 决定把 D1 的 serve/CLI 从 lib 剥离时新增（workspace 加 `[[bin]]` 依赖 lib 即可，无需新 crate） |

---

## 3. 对外 API 冻结清单（红线，违反即打回）

> 本提案全部改动为**内部结构重排**；以下对外可见面一律不动。PLAN.md 须把每一项
> 映射为具体的回归测试 / 契约测试。

| # | 冻结面 | 冻结内容 | 关联方 |
|---|---|---|---|
| Z1 | Ringing wire 协议 | `qaqh-ringing` 全部公共类型、envelope 形状、`RINGING_SCHEMA="qaqh.Ringing"` / `RINGING_VERSION=1`、`RingingWorkerCommandEnvelope` 字段 | daemon / client / 前端 |
| Z2 | daemon HTTP/SSE surface | `/service/{method}` 归一面（commit `9c946e7`）、`/api/config/events` SSE、`CONTROL_PROTOCOL_VERSION`、header 集合 | web 前端 / client |
| Z3 | frontend 契约 | `docs/frontend-contract.md`：envelope 形状与错误 code 集合冻结，行为变更走 RFC | 前端 |
| Z4 | config 契约 | 磁盘格式 + `qaqh-config-api` DTO（camelCase）+ `ConfigPatch` merge-patch 守卫语义（config-revamp K1–K3） | 多端设置页 |
| Z5 | session 磁盘格式 | `{sessions_dir}/{seed}/meta.json` + `messages.jsonl`（逐行 Message）+ 根 `index.json`；**含旧会话回放兼容** | 用户体验（历史不丢） |
| Z6 | workspace serve 协议 v1 | `/health` `/tools` `/execute` + Bearer + `POST /subagent`（WSL 路径在用，二进制与 daemon 同仓分发但**可能跨版本**） | workspace 子进程 |
| Z7 | qaqh-client 公共 API | TUI / desktop 壳消费的传输面（`Client`/`ClientHandlers`/discovery/lease/SSE 三频道 + timeline 流） | 壳层 |
| Z8 | 工具名与参数 schema | 模型可见的工具定义集合（`tool_def`）不变 | 模型行为 / 提示词缓存前缀 |

---

## 4. 改动项全量清单（激进范围）

> 每项给出：现状 → 目标 → 验收标准（grep 可测）。PLAN.md 负责切 PR 与排批次。

### Phase 1 —— 搬家（含 message 纯度）

| ID | 现状 | 目标 | 验收标准（出口 grep，应无输出） |
|---|---|---|---|
| P1-1 (B1) | 授权审批管线在 `engine_tool.rs` | workspace 暴露 `authorize(ToolInvocation) -> Decision` **单一门面**（策略 + 流程 + 风险映射都在 workspace）；loop 每工具调用门面一次；`TrustedFolderSet::load` 移入 workspace 初始化 | `grep -rn "authorization::\|permission::" crates/qaqh-msgloop/src` |
| P1-2 (B2) | 技能状态机在 `state/skill_context.rs` | 整体移入 `qaqh-skills`（新增 runtime 子模块）；msgloop 只在注入点调用 | `grep -rn "skill_context\|SkillCatalogSnapshot" crates/qaqh-msgloop/src` |
| P1-3 (B3) | 冲突检测在 `services/conflict.rs` | 移入 workspace（工具编排语义）；**顺带修第一轮 M1**（`"patch"` vs 实际 `apply_patch` 匹配表） | `grep -rn "file_write_paths\|conflict" crates/qaqh-msgloop/src` |
| P1-4 (B4) | 投影在 `util::project_turns_from_messages` + `services/dashboard.rs` | 移入 `qaqh-runtime/src/ringing/projection.rs`（与 dashboard 同居）；`conversation_snapshot.rs` 改为 crate 内调用 | `grep -rn "project_turns\|DocInfo\|dashboard" crates/qaqh-msgloop/src` |
| P1-5 (B6) | loop 6 处直写 `SessionManager::global()` | title/stats/usage/mode 簿记改走 message Effect（`Effect::Persist*` / 注入 sink），由宿主执行落盘 | `grep -rn "SessionManager" crates/qaqh-msgloop/src` |
| P1-6 (A1) | `MessageStore` 5 处直接落盘 | 新增 `Effect` 持久化变体（如 `Effect::FlushMeta{..}`/`Effect::SnapshotFull{..}`）；`flush_meta`/`snapshot_full` 变纯状态计算，磁盘写移至宿主侧单一 flush 服务；**Z5 磁盘格式与写序完全不变** | `grep -rn "SessionManager" crates/qaqh-message/src` |
| P1-7 (A2) | 消息层驱动工具执行 | `ToolExecutorFn`/`execute_tools_batch` 移出 message：MessageStore 只暴露 pending 队列视图与 `push_tool_result`；批量执行编排由 loop（后续 runtime/agent）驱动 | `grep -rn "ToolExecutorFn\|execute_tools_batch" crates/qaqh-message/src` |
| P1-8 (B5) | `engine_session::reload_config` 直接 `Config::load()` 读盘 | reload 语义收敛到 config 单写口：引擎只调 `qaqh_config::watch::latest()` / 专用 reload API；磁盘权威读由 config crate 的 reload 服务执行（与 P2-D1 方向一致） | `grep -rn "Config::load()" crates/qaqh-msgloop/src` |
| P1-9 (B7) | 三处重复 `find_endpoint` | turn 开始时一次性解析 endpoint/protocol 存入 `AgentState`，engines 只读字段 | `grep -rn "find_endpoint" crates/qaqh-msgloop/src` |
| P1-10 (D2) | workspace `runtime.rs` 直接 `Config::load()` 判 image 能力 | 能力快照由 daemon 启动 / config watch 推送注入 workspace，工具调用路径零磁盘读 | `grep -rn "Config::load()" crates/qaqh-workspace/src` |

**Phase 1 出口总验收**：workspace lib 测试全绿（基线见 §6）；`cargo clippy --workspace --all-targets`
0 error；上表 10 条 grep 全空（测试目录白名单另列）；Z1–Z8 契约测试无 diff。

### Phase 2 —— 合并（msgloop 收编）

| ID | 动作 | 验收标准 |
|---|---|---|
| P2-1 (C1) | `qaqh-msgloop` 全量移入 `qaqh-runtime/src/agent/`；`WorkerCommand`/`WriterEvent`/`CancelToken` 降级为 crate 内部类型；`actor.rs` 的构造逻辑并入 agent 入口 handle（`spawn_agent(config, cmd_rx, event_tx) -> AgentHandle` 形态） | `grep -rn "qaqh_msgloop\|qaqh-msgloop" crates/ --include=*.rs --include=*.toml` → 0；workspace `members` 少一项 |
| P2-2 | feature 传递链收敛：`qaqh-runtime` 的 `memory = ["qaqh-msgloop/memory"]` 变为本 crate feature；`qaqh-daemon` 的 `memory = ["qaqh-runtime/memory"]` 不变 | `cargo tree -e features` 无 qaqh-msgloop 节点 |
| P2-3 | 模块级规则落地：`agent/` 不 import `ringing/` 内部（hub/router/outbox/journal），只经 handle；以 `pub(crate)` 纪律 + clippy + PLAN.md 约定的模块评审规则保障（编译期无法跨模块强制，属已知残留风险 R-4） | 代码评审检查单 + `grep -rn "crate::ringing::" crates/qaqh-runtime/src/agent/`（内部仅允许 handle 类型） |
| P2-4 (F1/F2 搭车) | `config/src/prompt.rs` → `agent/prompt.rs`（`OS_INFO`/`TOOLS_INFO` OnceLock 一并迁入，runtime `registry.rs:67,87` 改 crate 内引用）；`gate/src/guard.rs` → `agent/input_guard.rs`；gate 回归纯"HTTP streaming + 格式转换" | `grep -rn "prompt::\|guard::" crates/qaqh-config/src crates/qaqh-gate/src` → 0；gate 对外 API 除 guard 外不变（Z 面无 guard 项，安全） |

**Phase 2 出口总验收**：全绿 + P2 各 grep；合并后 `qaqh-runtime` ≈ 25k 行（两模块组）；
`qaqh-msgloop` 从磁盘删除。

### Phase 3 —— 拆全局单例（S4 根治）

| ID | 现状 | 目标 | 验收标准 |
|---|---|---|---|
| P3-1 (G1) | `SessionManager::global()` 单例（第一轮时调用方分布 3 crate；Phase 1 后仅宿主侧） | 注入化：宿主持有 `SessionStore` 实例传入 agent/flush 服务；`global()` 收敛为仅 daemon `main` 装配点可调 | `grep -rn "SessionManager::global()" crates --include=*.rs` → 仅 `qaqh-session` 定义处与 daemon 装配点 |
| P3-2 (D5/G2) | workspace 进程级全局（`set_context`/`set_mode`/`set_process_workspace`/`files_read`） | 会话作用域 ctx 对象（`ToolCtx`）显式传参；serve 模式串行队列**可保留但理由消失**（开放决策 Q3：保留 or 并发化） | `grep -rn "runtime::set_context\|set_process_workspace" crates/qaqh-runtime/src/agent` → 0 |
| P3-3 (D3 搭车) | 工具 cwd 经 `qaqh_session::workspace::session_workspace_cwd` 全局反查 | cwd 由宿主在构造 `ToolCtx` 时解析注入（E1 归属问题仍在，见 Q2） | `grep -rn "session_workspace" crates/qaqh-workspace/src` → 0 |
| P3-4 | C2 类全局 cancel flag 残留面复查 | cancel 全部走 per-session token（第一轮 PR-6 已修主路径；本项是 Phase 3 结构性复查，确保无进程级毒化路径残留） | 集成测试：跨会话 cancel 不互相影响（`session_inprocess` 扩展用例） |

### Phase 4 —— 周边归位（激进扫尾）

| ID | 现状 | 目标 | 说明 |
|---|---|---|---|
| P4-1 (E1) | `WorkspaceStore` 在 session crate，命名与 qaqh-workspace 冲突；"会话工作区"三方持有 | 单一属主：**推荐**留在 session 但模块更名 `session::grouping`（消歧义），并成为 cwd 解析的唯一权威（P3-3 之后 workspace 不再直读）；备选：整体移入 runtime（开放决策 Q2） | 行为不变，仅归属与命名 |
| P4-2 (F3) | subagent legacy HTTP fallback + 传输类型 re-export | in-process host 已覆盖生产：**激进项——删除 legacy fallback**，`SubagentHost` trait 的关联类型改用 domain/ringing 类型，解除 subagent→client 依赖 | 开放决策 Q4（若 TUI 之外存在无 host 嵌入场景则保留并标注 deprecated） |
| P4-3 (D1 收尾) | serve/CLI 二进制与工具 lib 同 crate | 按开放决策 Q1 执行：维持 `[[bin]]`（最小改动）或拆 `qaqh-tool-server`（激进）；无论哪种，Z6 协议冻结不动 | — |
| P4-4 | B4 之后 msgloop/services 目录清空、util 瘦身残留 | 目录清空，`calendar/token log/display fmt` 归 `agent/util` 或随 P2 已并入 | 卫生项 |

---

## 5. 阶段顺序与回退（顺序保守，是激进范围的安全阀）

```text
Phase 0 ──► Phase 1 ──► Phase 2 ──► Phase 3 ──► Phase 4
 基线+冻结    搬家(10项)   合并(1 crate)  拆全局      归位(4项)
             │可独立发布    │可独立发布     │可独立发布
             ▼             ▼              ▼
          每阶段出口 = 全部 grep 验收 + 全量测试绿 + Z1–Z8 契约测试无 diff
```

- **每阶段一个独立 PR 序列**，任一阶段中止，已合并的前序阶段依然净收益成立
  （Phase 1 尤其如此——即使永不合并，10 项搬家本身就是边界修复）。
- **Phase 2 必须在 Phase 1 之后**：先合并后搬 = 把四个寄居者搬进 runtime 后再从大 crate 里抠出来。
- **Phase 3/4 无先后硬依赖**，可并行或择机插入。
- 回退方式：阶段内按 PR revert；跨阶段无共享状态（搬家全是移动 + 接口收窄，无数据迁移）。
- **唯一有数据风险的点**：P1-6 改持久化触发路径——Z5 兼容性必须以
  "旧 daemon 写入的会话目录被新代码原样回放"集成测试锁定（参照第一轮 Step 0 的对抗性复核方法）。

---

## 6. 风险登记

| # | 风险 | 缓解 |
|---|---|---|
| R-1 | 搬家触及大量 `use` 路径与测试模块，表面 diff 巨大掩盖逻辑改动 | 移动与修改分离成两个 commit；`git diff --color-moved=dimmed-zebra` 评审 |
| R-2 | serve 子进程与 daemon **跨版本**共存（用户升级节奏不一），Z6 若意外破坏则 WSL 工具全灭 | Z6 契约测试 + serve 集成用例在 WSL 模式跑一遍（CI 无 WSL 则手动走查清单进 PLAN.md） |
| R-3 | P1-6 持久化路径重排引入写序回归（compact 上下文、pending_save 清空时机） | 影子验证：新旧 flush 实现对同一消息序列产出逐字节一致的 JSONL；随机回放测试 |
| R-4 | 合并后 runtime ~25k 行，模块边界只剩 `pub(crate)` 纪律 | PLAN.md 定模块评审规则；`ringing/` 对外类型集中在 `mod.rs` re-export，`agent/` 只准 import re-export 面 |
| R-5 | B2 技能状态机移入 skills 后出现 skills→config/session 反向依赖 | 移动时以 trait/回调注入 token 计数与持久化，skills 保持零内部依赖（当前 Cargo.toml 无内部依赖，须守住） |
| R-6 | 测试基线漂移 | 入口基线：workspace lib 测试全绿（第一轮基线 706/706，以当日实跑为准）+ 关键集成（ask_user_lifecycle / permission_lifecycle / session_inprocess / subagent_inprocess）全绿 + clippy 0 error，Phase 0 记录实测数字 |

---

## 7. 留给 PLAN.md 的开放决策（高级模型拍板）

| # | 决策 | 选项与推荐 |
|---|---|---|
| Q1 | D1 serve/CLI 部署形态 | a) 维持 `[[bin]]`（推荐，最小改动）；b) 拆 `qaqh-tool-server` bin crate（更彻底，crate 数 +1） |
| Q2 | E1 会话工作区归属 | a) 留 session、模块更名 `grouping`、成 cwd 唯一权威（推荐）；b) 移入 runtime；c) 独立小 crate（不推荐） |
| Q3 | P3-2 后 serve 串行队列 | a) 保留（WAL 语义简单，Z6 行为不变，推荐）；b) 拆除改并发（需重审 Z6 幂等性） |
| Q4 | F3 legacy fallback | a) 删除（推荐，生产已 in-process）；b) 保留 + deprecated 标注 |
| Q5 | P1-5 簿记落盘通道 | a) 扩展 message `Effect` 枚举（推荐，与既有架构一致）；b) 独立 `BookkeepingSink` trait 注入 |
| Q6 | B7 endpoint 解析时机 | a) turn 开始一次性解析入 `AgentState`（推荐）；b) 收敛为 gate 门面函数 |
| Q7 | 测试迁移与白名单 | grep 验收的测试目录白名单、影子验证用例集、WSL 走查清单的编制 |

---

## 8. 与既有文档 / 在途工作的关系

- **`docs/PLAN.md`（第一轮审查）**：M1（conflict 匹配 bug）在 P1-3 顺带根治；C2（全局 cancel
  毒化）主修复已在第一轮 PR-6 落地，P3-4 是其结构性复查；G4/H1/H2 等状态机项不受本提案影响。
- **`docs/config-revamp-plan.md`**：K1–K3 约束不变；P1-8（reload 收敛）与其 P2-D1（watch 单写口）
  是同一方向的本提案落地，两文档须交叉引用不重复施工。
- **`docs/frontend-contract.md`**：本提案不触碰其任何冻结项；若 Phase 3 后事件时序有微调
  （理论不该有），走其既定 RFC 流程。
- **`docs/cancel-contract.md`**：P3-2/P3-4 改动 workspace 全局面时须对照复查。
