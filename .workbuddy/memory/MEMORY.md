# QAQ-Harness 项目长期记忆

## 仓库与工作流

- 主仓库 `/home/qaqtamsy/Projects/QAQ-Harness`（Rust，15 crates，Edition 2024，alpha）。
- 外部壳与配套仓库（均 github.com/QAQTam，public）：
  - `qaqh-electron`（Electron 44 + SolidJS 2 RC + Vite 8，2026-09-09 初始导入 fcff0e5）
  - `qaqh-js-sdk`（Ringing V1 TS SDK，2026-09-09 初始导入 205c8f2；**bundled bindings 是 9/8 快照，缺六态 TimelineToolState 与 session_attach，待从主仓再生成同步**）
  - `qaqh-tui-app`（Rust TUI，子代理实时观测 f85c33d）
  - `qaqh-winui-app` 本地已不存在（记忆曾记有此仓库，已过时）；`qaqh-ohos`/`qaqh-android` 为空占位目录，无内容。
- 系统重置前备份（2026-09-09）：主仓 main 已合并推送至 54f391b；WorkBuddy 状态（身份/记忆/mcp.json/dev-env/skills 清单）在 secret gist https://gist.github.com/QAQTam/81fc8df2ce38c9c78db4da96d7a8f598
- **契约真源在本仓库** `docs/frontend-contract.md`；前端对着它实现，变更走文档第 3 节的 RFC 流程（后端先开 RFC issue → 合入 → 前端开 PR → 双侧 tag 同步发版）。
- 无 CI。质量门禁是本地 just recipe 链：`just check` / `clippy` / `fmt` / `test`。clippy 全仓 deny `unwrap_used`、`string_slice`。
- 提交信息用 conventional commits + scope，重大变更带 `!` 与 `BREAKING CHANGE:` 脚注（例：`refactor(edit)!: kind 6→3 + strict-only`）。
- 测试约 820 个；触碰全局状态的测试统一走 `TEST_RUNTIME_SERIAL` 互斥串行。测试/多实例用 `QAQH_DATA_DIR` 重定向数据根。

## 已知既有问题（未完成）

- ~~todo_contract 两条测试失败~~ **已解决**（2026-09-09，提交 `1e15111`）：根因是 7f5a1d7 v3 拆分漏更新集成测试——测试仍断言已退役的聚合 `todo` 工具，"missing todo tool" / "[PERMISSION_REQUIRED]" 均由此而来（未知工具名 → 保守 Write 回退 → Level 1 全确认），非注册缺失。已重写对齐三件套契约。
- 工具返回管线五项全部完成（提交 `42e27e1` + `4446264` + `1e15111` + `54f391b`，**已 ff 合入 main 并推送（main=54f391b）**；worktree 与本地分支已清，远端分支保留（与 main 同指））：
  1. ~~折叠策略进程级全局污染~~ 已修：thread-local + `ActorToolScope` 搬运，API 更名 `set_thread_policy`。
  2. ~~output_ref 语义误导~~ 已修：文档/注释明确「仅 NoFold + >10MiB 触发，传输保护阀」；契约文档第 5 节澄清字段语义。
  3. ~~ToolResult 投影字段可漂移~~ 已修：`summary`/`model`/`output_ref` 私有化 + 受控改写接口（`push_hint`/`rewrite_text`/`externalize_output`）。
  4. ~~TimelineToolState 缺 cancelled/backgrounded~~ 已修（`54f391b`）：枚举补两态 + `From<ToolStatus>` 映射；归档改 `push_tool_result_canonical` 保真五态；backfill 的 ToolFinished 不再伪造 ok/error。ADR：`docs/adr-toolstate-cancelled-backgrounded.md`。**cancelled 现仅由 orphan seal 产生；backgrounded 暂无生产者，枚举先行**。
  5. ~~v3 拆分漏网~~：dashboard 即时刷新仍匹配旧名 `"todo"` 已改三件套。
- **bindings 同步陷阱**：ts-rs 再生落到 `crates/*/bindings/`（gitignored），根 `bindings/` 靠手动 cp 同步，历史上漏过（根 ControlCommand 曾缺 `session_attach`，2026-09-09 补上）。核对 TS 契约必须对比 crate 级再生输出，不能只看根目录；ts-rs **包含私有字段**（serde 线上格式与 TS 成员不受私有化影响，仅声明顺序/注释会变）。

## 架构要点 · per-actor 状态约定

- `qaqh-workspace::runtime`：**per-actor 状态一律 thread-local，不用进程级 static**（模块文档明载，Knife-1 step 2）。已迁入：`RUNTIME_CTX`、`ACTOR_TOOL_MANAGER`、`AGENT_MODE`、sandbox 标志、工具结果折叠策略。新增 per-actor 状态时照此办理，并记得同步进 `ActorToolScope`（capture/install/drop）以便传递到工具工作线程。
- 每会话一个 actor 线程；工具执行在 actor 派生的 OS 线程上，**不继承 thread-local**，必须靠 `ActorToolScope::capture()/install()` 搬运。
- 写这类「全局 vs 作用域」的测试时，**判据别用布尔 truncated 标志**——`ToolResult::ok` 因 24K 硬顶本身就会把它置 true。用模型可见文本的字符数（8K vs 24K 差异明显）。

## 架构要点（易忘）

- daemon 是唯一协议面，Ringing V1 HTTP/SSE。浏览器前端**必须用 fetch + ReadableStream 消费 SSE**，不能用原生 `EventSource`（需自定义头且禁止 query 传 token）。
- 浏览器 webUI 由 daemon 内置静态托管 `GET /debug/`，限 loopback；token 经 `__qaqh_bridge__.js` 注入 `window.__QAQH_DEBUG__`。
- `bindings/qaqh/*.ts` 是 ts-rs 生成的**前端实际可见类型**，改 Rust 类型后需重新生成；`crates/*/bindings/` 已 gitignore，真相在根 `bindings/`。
- 工具结果有两个平面，**刻意不等同**：`project_for_model()` / `render_xml_envelope()` 给模型（不含 diff，省 token），`TimelineTool` 给前端（含 diff、permission）。
