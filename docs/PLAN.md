# Crate 边界整改执行计划（boundary-reform PLAN）

> 状态：**ACTIVE**（2026-08-30 制定，基于当日对 main 分支的全量实证核查）。
> 上游文档：`docs/crate-boundary-proposal.md`（PROPOSAL）——问题证据与目标边界的**唯一权威来源**。
> 本文档职责：把提案翻译为可执行的 PR 批次 / 逐项步骤 / 可 grep 的验收流程。
> **偏离规则**：执行中发现与提案不符，先改提案文档再改本 PLAN；本 PLAN §9 勘误表已收录
> 制定当日发现的全部偏差并经实证修正，视为提案的勘误附件。
> 顺序纪律（继承提案）：先搬家 → 再合并 → 再拆全局 → 周边归位；每阶段独立全绿、独立可回退。

---

## 0. 执行上下文（2026-08-30 实测快照）

- 分支 `main`，HEAD `86625a7`。第一轮 PLAN（`docs/PLAN.md` 旧文件）已完成并删除，本 PLAN 接棒。
- 实测 LOC：`qaqh-msgloop` 12,028 / `qaqh-runtime` 12,378 / `qaqh-message` 3,510 /
  `qaqh-workspace` 24,285 / `qaqh-session` 1,765。合并后 runtime ≈ 24.4k 行，与提案"≈25k"一致。
- 提案 §1 证据清单全部逐条复验：**结论全部成立**，部分行号/范围有漂移，见 §9 勘误表。
  关键前提复核：`qaqh-skills` 当前零内部依赖（仅 serde / serde-saphyr），R-5 前提成立；
  msgloop 唯一消费方为 runtime（C2 成立）；`Loop::new_ipc` 已退役（C3 成立，头注释在
  `crates/qaqh-ringing/src/worker.rs:4` 与 `crates/qaqh-msgloop/src/ringing_v1/wire.rs:4`）。

### 0.1 基线现实（与提案 R-6 假设的重大出入）

提案假设入口基线为"workspace lib 测试全绿（706/706）"。**当日实测该假设不成立**：

| # | 失败项 | 症状 | 初判根因 | 处置 |
|---|---|---|---|---|
| V1 | `qaqh-config` lib：`prompt::tests::system_prompt_for_mode_minimal_dsh_is_verbatim` | 期望 minimal:dsh 逐字 prompt，实际返回完整 prompt | `qaqh_types::is_minimal_dsh` 已硬编码 `false`（`tool_mode.rs:69-73`，minimal:dsh 已废弃下线），测试未同步 | 过期测试，PR-0-2 改断言 |
| V2 | `qaqh-runtime` lib：`service::tool_mode_tests::optional_tool_mode_parses_minimal_dsh` | 同上 | 同 V1 | PR-0-2 改断言 |
| V3 | `qaqh-workspace` lib 3 个：`backend::tests::process_kill_preempts_long_running_task_in_serve`、`execution::tests::process_tools_route_to_workspace_backend`、`registration::tests::default_registry_exposes_the_formal_tool_vocabulary` | serve `/execute` 返回 `http status: 404` | 待三诊（怀疑近期 RPC 归一/死代码清理提交引入回归，或本机环境） | PR-0-3 |
| V4 | `qaqh-msgloop` 集成 `permission_lifecycle` 2 个：`llm_approval_forwards_exec_via_http_backend`、`llm_four_pending_execs_defer_execution_until_all_resolved` | "approved exec did not run via Http backend / serve" | 与 V3 同路径（serve HTTP 后端） | PR-0-3 |
| V5 | `cargo clippy --workspace --all-targets`：error 1 + warning ~9 | `qaqh-daemon/src/axum_server.rs:380` `unwrap()` 触犯 workspace `unwrap_used=deny` | 预存违规 | PR-0-1 |

注意：V3/V4 中 2 个失败测试恰是 **Z6/Z8 契约锚点**（`registration::default_registry_*` 是 Z8 工具
词表守卫；permission_lifecycle 是 Z6 serve 行为守卫）。在它们回绿或被明确隔离之前，
后续所有阶段的"契约测试无 diff"出口门不可信——这就是 Phase 0 扩容的理由。

**Phase 0 出口硬条件：`cargo test --workspace --no-fail-fast` 0 failed（或失败项全部带
`#[ignore]` + 书面理由登记于本节），clippy 0 error 0 warning（新增）。**

### 0.2 Phase 0 完成登记（2026-08-31 复验与闭环）

- **V1–V5 全部闭环**，根因统一为提交 `0946afe`（feat: Anthropic Messages 网关接入与
  bash/pwsh 工具拆分，minimal:dsh 下线）**改了生产代码但漏同步测试**，并非环境/回归：
  - V1/V2：`is_minimal_dsh` 桩化后断言未同步 → 改为"废弃模式守卫"断言（PR-0-2）。
  - V3/V4：exec 已拆分至 bash/pwsh 且不再注册（`exec.rs:1350` 注释为刻意设计），测试载体
    仍是 exec → 词表守卫去 exec（19→18 追认）、execution 路由测试与 permission_lifecycle
    审批载体 exec → bash。**V3 的"serve /execute 404 回归"初判不成立**（勘误见 §9）。
  - V5：clippy error 0；axum_server.rs 同文件 warnings（unused import / useless format /
    lazy eval / collapsible if ×3 / redundant closure / items_after_test_module）全部清零。
- **基线漂移**：PLAN 制定日（86625a7）实测 4 target / 7 test 失败；执行日（0d6cb12 起点）
  复验为 3 target / 5 test 失败——`backend::process_kill_preempts_long_running_task_in_serve`
  已自行回绿，其余漂移原因即上述测试同步状态差异。
- **本轮修复后基线（新 HEAD c725499）：54 targets，789 passed / 0 failed，
  clippy 0 error**。Phase 0 出口硬条件达成，Phase 1 解锁。
- commit 映射：PR-0-1 = `a67bf35`；PR-0-2 = `0f09737`；PR-0-3 = `c725499`；PR-0-4 = 本文档提交。

---

## 1. 总览：阶段与 PR 切分

```text
PR-0-1..0-4 ──► PR-1-6,1-7,1-5 ──► PR-1-1..1-4,1-8..1-10 ──► PR-2-1..2-4 ──► PR-3-1..3-4 ──► PR-4-1..4-4
  基线+冻结       message 纯度先行        其余搬家               合并收编        拆全局单例       周边归位
                 （P1-5 依赖 Effect 通道）
```

| 阶段 | PR | 内容 | 粗估 | 依赖 |
|---|---|---|---|---|
| 0 | PR-0-1 | clippy 修复 | 0.5d | — |
| 0 | PR-0-2 | 过期 minimal:dsh 测试清理 | 0.25d | — |
| 0 | PR-0-3 | serve 路径 5 失败三诊 | 0.5–1d | — |
| 0 | PR-0-4 | 基线登记 + Z1–Z8 契约锚点映射评审 | 0.25d | 0-1..0-3 |
| 1 | PR-1-6 (A1) | message 持久化 Effect 化 | 1d | 0 |
| 1 | PR-1-7 (A2) | 工具执行编排移出 message | 0.5d | 1-6 |
| 1 | PR-1-5 (B6) | loop 簿记走 Effect / 注入 | 0.5d | 1-6 |
| 1 | PR-1-1 (B1) | 授权审批门面入 workspace | 0.75d | 0 |
| 1 | PR-1-2 (B2) | 技能状态机入 skills | 0.5d | 0 |
| 1 | PR-1-3 (B3) | 冲突检测入 workspace + 修 M1 | 0.5d | 0 |
| 1 | PR-1-4 (B4) | 投影入 runtime/ringing | 0.5d | 0 |
| 1 | PR-1-8 (B5) | reload 收敛 config 单写口 | 0.5d | 0 |
| 1 | PR-1-9 (B7) | endpoint 一次性解析入 AgentState | 0.25d | 0 |
| 1 | PR-1-10 (D2) | workspace 能力快照注入 | 0.5d | 0 |
| 2 | PR-2-1 | msgloop 机械搬家（git mv） | 0.5d | Phase 1 全部 |
| 2 | PR-2-2 | feature 传递链收敛 | 0.1d | 2-1 |
| 2 | PR-2-3 | actor/registry 构造并入 agent 入口 + 模块规则 | 1d | 2-1 |
| 2 | PR-2-4 | F1/F2 搭车（prompt / guard 迁入 agent） | 0.4d | 2-1 |
| 3 | PR-3-1 (G1) | SessionManager 注入化 | 1d | Phase 2 |
| 3 | PR-3-2 (D5) | workspace 会话作用域 ToolCtx | 1.5d | Phase 2 |
| 3 | PR-3-3 (D3) | cwd 由宿主注入 | 0.5d | 3-2 |
| 3 | PR-3-4 | cancel 残留复查 + 扩展用例 | 0.5d | 3-2 |
| 4 | PR-4-1 (E1) | WorkspaceStore → session::grouping | 0.5d | 3-3 |
| 4 | PR-4-2 (F3) | 删 subagent legacy fallback | 0.75d | Phase 2 |
| 4 | PR-4-3 (D1) | serve 形态确认 + Z6 走查清单 | 0.25d | 0-3 |
| 4 | PR-4-4 | 目录与注释卫生 | 0.5d | Phase 2 |

合计 ≈ **11–13 天**（提案 10–14 天区间内；Phase 0 因基线变脏扩容 +1d）。

**通用 PR 纪律**（全阶段适用，源自 R-1）：
- 搬家类 PR 一律两段 commit：`git mv` 纯移动（零逻辑改动）→ 接口收窄/修 use。评审用
  `git diff --color-moved=dimmed-zebra`。
- 每个 PR 出口 = §10 出口门（对应 grep 空 + `cargo test --workspace --no-fail-fast` 不低于
  基线 + clippy 0 error 0 warning 新增）。

---

## 2. Phase 0 —— 基线修复与契约冻结

### PR-0-1 clippy 修复
- `crates/qaqh-daemon/src/axum_server.rs:380`：`duplicate_check.unwrap()` → `expect("...")`
  或改用 `?`/match 传播（按上下文语义选，禁止 `#[allow]` 绕过）。
- 顺带清理同文件 ~9 个 warning 中的一行级修复（unused import、redundant closure）。
- 验收：`cargo clippy --workspace --all-targets 2>&1 | grep -c "^error"` → 0。

### PR-0-2 过期 minimal:dsh 测试清理
- 根因：`qaqh-types/src/tool_mode.rs:69-73` 已将 `is_minimal_dsh` 桩化为 `false`
  （PTY bash_v2 + str_replace_editor 下线时 minimal:dsh 被移除），但两处测试仍断言旧行为：
  - `crates/qaqh-config/src/prompt.rs:149` `system_prompt_for_mode_minimal_dsh_is_verbatim`
  - `crates/qaqh-runtime/src/service.rs` `optional_tool_mode_parses_minimal_dsh`
- 处置：改断言为新语义（`is_minimal_dsh` 恒 false ⇒ `system_prompt_for_mode("minimal:dsh")`
  返回完整 prompt；runtime 侧同理），非直接删除——保留"废弃模式必须走完整词表"的守卫价值。
- 验收：`cargo test -p qaqh-config --lib -p qaqh-runtime --lib` 全绿。

### PR-0-3 serve 路径失败三诊（V3/V4，共 5 个失败测试）
同一根因的五张面孔：`qaqh-workspace` lib 3 个 + `permission_lifecycle` 2 个，全部经由
serve HTTP 后端（`HttpToolExecutionBackend` → `spawn_serve()` → `POST /execute`），症状为 404。
诊断决策树（按序执行，结论写入 §0.1 表格替换"初判"列）：

1. **单独复现**：`cargo test -p qaqh-workspace --lib backend -- --nocapture`，抓 serve 子进程
   stderr 与实际命中的路由。排除本机因素（已确认无 `HTTP_PROXY` 等代理变量）。
2. **嫌疑提交二分**：近期 `9c946e7`（服务 RPC 三面归一）、`86625a7`（删 parse_sse_frame）
   直接触碰 serve/HTTP 面；`git bisect` 或直接 revert-try 验证。
3. **分类处置**：
   - 真回归 → 在 main 上修复（它同时是 Z6/Z8 契约破坏），**阻塞 Phase 1 开始**；
   - 环境依赖（防火墙/AV 拦截 localhost 等）→ 测试加 `#[ignore = "env: reason"]`，
     登记到 §0.1，并要求在另一台机器/CI 跑一次留证；
   - 间歇性（端口竞争/时序）→ 修测试隔离（固定端口策略改为 socket 竞争重试）。
- 验收：5 个测试绿，或带 ignore + 书面理由；`permission_lifecycle` 整文件回绿。

### PR-0-4 基线登记与契约锚点映射
- 在本 PLAN §0.1 追加当日实测通过数（`cargo test --workspace --no-fail-fast | grep "test result"` 汇总）。
- 评审确认 §10.2 的 Z1–Z8 → 测试映射表完备；缺锚点的（见 §10.2 标注）在本 PR 补最小契约测试。

---

## 3. Phase 1 —— 搬家（含 message 纯度）

> 出口 = 下表 10 条 grep 全空（白名单：各 crate `tests/` 目录与 `#[cfg(test)]` 模块，
> 白名单明细见 §10.3）+ workspace lib / 关键集成全绿 + clippy 0 error + Z1–Z8 无 diff。

### PR-1-6（A1）message 持久化 Effect 化 —— Phase 1 首个 PR
**现状**（实证）：`crates/qaqh-message/src/store.rs:260,270,273,985,987` 五处直接调
`SessionManager::global()`（save_append / update_compact_context / update_meta /
save_compact_context / save_full）；`Effect` 枚举仅 `None | CallGate | TurnComplete`。
**设计**（Q5a 的落地形式——持久化表达为 Effect 家族，但采用**批量队列**而非逐点返回）：
1. 新增 `pub enum PersistOp { Append{..}, UpdateMeta{..}, UpdateCompactContext{..},
   SaveCompactContext{..}, SaveFull{..} }`（`effect.rs`，与 `Effect` 同居）。
2. `MessageStore` 增加内部 `pending_persist: Vec<PersistOp>`；`flush_meta` / `snapshot_full`
   **签名不变**，把原 5 处磁盘写改为 enqueue（实测 flush_meta 在 msgloop 有 17 处调用、
   snapshot_full 3 处，另 message 内部 `context_flow.rs:326` 自调 1 处——逐点改返回值
   会放大 diff 且易漏接）。
3. 新增 `pub fn take_persist_ops(&mut self) -> Vec<PersistOp>`——这就是宿主侧 Effect 面。
4. 宿主执行：loop_core 在**每条命令 dispatch 结束后**（与原先同步落盘同线程同时序）drain
   并调用注入的 `SessionFlush` 服务（`&SessionManager` 句柄）顺序执行。单线程下磁盘写序
   与现状逐字节一致（R-3 影子验证锁定）。
5. **Z5 红线**：磁盘格式与写序不变；新增集成测试"旧 daemon 写入的会话目录被新代码原样回放"
   （复用 `qaqh-session` 的 migrate 测试装置）+ 影子验证：新旧 flush 对同一消息序列产出
   逐字节一致的 JSONL。
**验收**：`grep -rn "SessionManager" crates/qaqh-message/src` → 0（含 `use` 行）。

### PR-1-7（A2）工具执行编排移出 message
**现状**：`store.rs:126` 持有 `tool_executor: Option<ToolExecutorFn>`；`store.rs:809`
`execute_tools_batch()`；`store.rs:972` `set_tool_executor()`；执行器在
`msgloop/src/state/agent.rs:628` 注入。
**步骤**：MessageStore 只暴露 pending 工具队列视图（`pending_tools()`）与
`push_tool_result(...)`；`execute_tools_batch` 编排逻辑移至 loop（engine_tool 侧）驱动，
调用 `qaqh_workspace` 后逐个 `push_tool_result`。`ToolExecutorFn` 类型从 `effect.rs`/`lib.rs`
re-export 面删除。
**验收**：`grep -rn "ToolExecutorFn\|execute_tools_batch\|set_tool_executor" crates/qaqh-message/src` → 0。

### PR-1-5（B6）loop 簿记走 Effect —— 依赖 PR-1-6
**现状**（勘误后，比提案范围大）：global() 直写 **7 处**——`engine_title.rs:51,86`、
`engine_misc.rs:70,100`、`engine_compact.rs:397`、`turn_lap/gate.rs:447`、`types.rs:453`
（提案漏计 types.rs）；另有 `state/lifecycle.rs:20,28,261` 三处 global() **读**
（exists / load_for_resume / load_meta），及静态助手 `generate_seed` / `now_epoch`
（`lifecycle.rs:172,179,209,210`、`engine_compact.rs:190`）。
**步骤**：
1. 写路径（title/context_stats/usage/mode）→ 走 PR-1-6 的 PersistOp 通道扩展
   （`PersistOp::UpdateTitle{..}` 等四变体），或独立 `Effect` 变体，由同一 flush 服务执行。
2. 读路径（lifecycle 会话装载）→ `AgentState` 构造时注入 `&'static SessionManager`（Phase 2
   前的过渡形态；Phase 3 收敛为实例注入）。
3. 静态助手 → 在 `qaqh-session` 顶层新增自由函数 `generate_seed()` / `now_epoch()`
   re-export，msgloop 改引自由函数（`SessionManager::generate_seed` 内部转调，API 不删）。
**验收**：`grep -rn "SessionManager" crates/qaqh-msgloop/src` → 0。

### PR-1-1（B1）授权审批门面入 workspace
**现状**：`engine_tool.rs`（1,018 行）内联完整审批管线——`:61,68` `TrustedFolderSet::load("")`
读磁盘、`:156` 构造 `ToolInvocation`、`:167` `authorization::admit`、`:179-212`
`ToolCategory`×`PermissionRisk`→信任级别映射、`:344+` `ApprovalError` 三分类。
**步骤**：
1. `qaqh-workspace` 新增 `authorize(ToolInvocation) -> Decision` 单一门面（策略 + 审批流程 +
   风险映射全部内聚；`Decision` 携带 Authorized / ApprovalRequired{challenge} / Denied{reason}）。
2. `TrustedFolderSet::load` 移入 workspace 初始化（serve 启动 / daemon 注入两路径），
   loop 工具调用路径零磁盘读。
3. loop 侧 `engine_tool.rs` 收缩为"调门面 + 按 Decision 分发"，目标 <400 行。
**验收**：`grep -rn "authorization::\|permission::" crates/qaqh-msgloop/src` → 0；
`grep -rn "TrustedFolderSet" crates/qaqh-msgloop/src` → 0。

### PR-1-2（B2）技能状态机入 skills
**现状**：`state/skill_context.rs` 实测 **830 行**（提案记 ~600）：catalog 快照、激活/去激活、
token 预算 `MAX_TOTAL_SKILL_TOKENS`。
**步骤**：整体移入 `qaqh-skills` 新增 `runtime` 子模块；msgloop 只在注入点调用。
**R-5 守护**：移动时 token 计数与持久化经 trait/回调注入——skills 保持零内部依赖
（当前 Cargo.toml 仅 serde/serde-saphyr，移动后**不得**新增 qaqh-* 依赖；CI grep：
`grep -c "qaqh-" crates/qaqh-skills/Cargo.toml` → 0）。
**验收**：`grep -rn "skill_context\|SkillCatalogSnapshot" crates/qaqh-msgloop/src` → 0；
skills 单测随迁并全绿。

### PR-1-3（B3）冲突检测入 workspace + 顺带修第一轮 M1
**现状**：`services/conflict.rs`（218 行）在 loop crate；`:50` 处含 patch 目标解析。
**步骤**：移入 `qaqh-workspace`（工具编排语义）；**顺带修 M1**——`"patch"` 与实际
`apply_patch` 的工具名匹配表错误（第一轮遗留），修复 + 回归用例同 PR。
**验收**：`grep -rn "file_write_paths\|conflict" crates/qaqh-msgloop/src` → 0（`services/`
目录本 PR 后仅剩 dashboard，待 P1-4 清空）。

### PR-1-4（B4）投影入 runtime/ringing
**现状**：`util/mod.rs:151` `project_turns_from_messages`（及 `:141,163` 两个同族函数）；
消费方实测**三处**（提案只列一处）：`runtime/src/ringing/conversation_snapshot.rs:21`、
`runtime/src/ringing/timeline_rebuild.rs:24`、`runtime/tests/timeline_rebuild.rs`；
`services/dashboard.rs`（68 行）组装 `qaqh_proto::DocInfo` 读 `workspace::runtime::files_read()`。
**步骤**：project_turns 三函数移入 `runtime/src/ringing/projection.rs`；dashboard 移入
runtime（与 projection 同居）；`services/` 目录清空删除；`lib.rs` 模块表更新。
**验收**：`grep -rn "project_turns\|DocInfo\|dashboard" crates/qaqh-msgloop/src` → 0。

### PR-1-8（B5）reload 收敛 config 单写口
**现状**（勘误后，3 处而非提案的 1 处）：`ringing_v1/engine_session.rs:80`、
`state/agent.rs:312`（`AgentState::init`）、`state/agent.rs:328`（`init_subagent`）。
**步骤**：`engine_session` reload 改调 `qaqh_config::watch::latest()`；`AgentState::init*`
改为**接收 `Config` 参数**由调用方（runtime actor，Phase 2 后为 agent 入口）注入——
config 权威读只在 config crate 的 reload/watch 服务（与 config-revamp P2-D1 同向，
两文档交叉引用，不重复施工）。
**验收**：`grep -rn "Config::load()" crates/qaqh-msgloop/src` → 0。

### PR-1-9（B7）endpoint 一次性解析
**现状**：`engine_compact.rs:224`、`engine_title.rs:183`、`turn_lap/gate.rs:636` 三处
`qaqh_config::registry::find_endpoint`。
**步骤**（Q6a）：turn 开始时一次性解析 endpoint/protocol 存入 `AgentState` 字段
（`endpoint: ResolvedEndpoint`），engines 只读字段。
**验收**：`grep -rn "find_endpoint" crates/qaqh-msgloop/src` → 0。

### PR-1-10（D2）workspace 能力快照注入
**现状**：`workspace/src/runtime.rs:250-261` 两次 `Config::load()` 判 image 能力；
`read_image/mod.rs` 经 `crate::runtime::image_model_supported` 间接触发——每次工具调用重读盘。
**步骤**：daemon 启动 / config watch 推送能力快照（`image_tool_enabled` 布尔组）注入
workspace runtime 全局；工具调用路径零磁盘读。serve 子进程模式下快照经环境/启动参数下发，
serve 存活期间随 config watch 重启策略维持一致（与 workspace_supervisor 现有重启语义对齐）。
**验收**：`grep -rn "Config::load()" crates/qaqh-workspace/src` → 0（`main.rs` CLI 侧白名单除外，见 §10.3）。

---

## 4. Phase 2 —— 合并（msgloop 收编进 runtime/agent）

### PR-2-1 机械搬家（两段 commit）
- `git mv crates/qaqh-msgloop/src crates/qaqh-runtime/src/agent`，模块映射：
  | msgloop 旧路径 | runtime 新路径 |
  |---|---|
  | `ringing_v1/loop_core.rs` | `agent/loop_core.rs` |
  | `ringing_v1/engine_*.rs`、`injection.rs`、`paced_emitter.rs` | `agent/`（平级） |
  | `ringing_v1/turn_lap/` | `agent/turn_lap/` |
  | `ringing_v1/types.rs`、`wire.rs` | `agent/types.rs`、`agent/wire.rs`（wire 仅测试 harness 用，标 `#[cfg(test)]` 收缩） |
  | `state/`（agent / lifecycle / token_calibration；skill_context 已迁） | `agent/state/` |
  | `util/` 余量（calendar / token log / display fmt） | `agent/util/` |
  | `services/` | 已在 Phase 1 清空 |
- 第二段 commit：修 use 路径（`qaqh_msgloop::` → `crate::agent::`）、合并 dev-dependencies
  （tempfile / os_pipe / tiny_http 并入 runtime）、迁移 5 个集成测试
  （`ask_user_lifecycle` / `session_lifecycle` / `permission_lifecycle` / `inprocess_loop` /
  `concurrent_read_stress` + `common/`）到 `crates/qaqh-runtime/tests/`（无重名冲突，实测确认）。
- workspace `members` 删除 `qaqh-msgloop`；`crates/qaqh-msgloop` 目录删除。
- **验收**：`grep -rn "qaqh-msgloop\|qaqh_msgloop" crates --include=*.rs --include=*.toml` → 0
  （含 `qaqh-types/src/tool_mode.rs:4` 文档注释中的提及，随本 PR 更新）。

### PR-2-2 feature 收敛
- msgloop 的 `memory = []` 兼容 no-op 特性由 runtime 接管：`qaqh-runtime` `memory = []`；
  `qaqh-daemon` 的 `memory = ["qaqh-runtime/memory"]` 不变。
- **验收**：`cargo tree -e features -p qaqh-daemon | grep -c msgloop` → 0。

### PR-2-3 构造逻辑并入 agent 入口 + 模块纪律
- `WorkerCommand` / `WriterEvent` / `CancelToken`（现 `agent/types.rs`）降级 `pub(crate)`。
- `actor.rs:40-284` 与 `registry.rs:107-108,229,263-264,349-350,447-451,626` 的构造散点
  （`AgentState::new` / `agent_tool_registrars()` / `Loop::from_channels` / `LoopChannels::new`）
  收敛为 agent 入口：`spawn_agent(config, cmd_rx, event_tx) -> AgentHandle`；registry 只持
  handle 与 channel 端。
- **P2-3 模块规则**（R-4 缓解，编译期不可强制的已知残留）：
  1. `ringing/` 对外类型集中在 `ringing/mod.rs` re-export 白名单；
  2. `agent/` 内禁止 `use crate::ringing::`（仅允许经 handle 类型）；
  3. 评审检查单：`grep -rn "crate::ringing::" crates/qaqh-runtime/src/agent/` 输出仅 handle 相关。
- **验收**：上述 grep 合规 + 全绿。

### PR-2-4 F1/F2 搭车
- `config/src/prompt.rs`（186 行，含 `detect_shells()` 与 `OS_INFO`/`TOOLS_INFO` OnceLock）
  → `agent/prompt.rs`；消费方改引：`agent/state/lifecycle.rs:198,229,267`（读）与
  `registry.rs:67,87`（写）——后者改为构造 agent 前经 `agent::prompt::init_env(os, tools)`。
- `gate/src/guard.rs`（130 行）→ `agent/input_guard.rs`；消费方 `engine_input.rs:91`；gate
  回归纯"HTTP streaming + 格式转换"。
- **验收**：`grep -rn "prompt::\|guard::" crates/qaqh-config/src crates/qaqh-gate/src` → 0；
  `grep -n "prompt\|OS_INFO" crates/qaqh-config/src/lib.rs` → 0；gate 对外 API 无 guard 项
  （Z 面无涉及，安全）。

---

## 5. Phase 3 —— 拆全局单例（S4 根治）

### PR-3-1（G1）SessionManager 注入化
**现状**（实测分布）：Phase 1 后 `global()` 调用方仅剩 runtime 侧——`host_impl.rs:29,32`、
`service.rs:34,75,76,100,107,115,199,237`（含 `init`）、`ringing/conversation_snapshot.rs:13`、
`tests/timeline_rebuild.rs`；生产 `init` 在 `service.rs:34`（daemon main 目前不碰 SessionManager）。
**步骤**：
1. `SessionManager::init` 上移至 daemon `main` 装配点；`QaqhService::init` 改收
   `Arc<SessionManager>` 注入。
2. service / host_impl / conversation_snapshot 全部改经注入句柄。
3. `global()` 保留但仅 daemon main 可调（`#[doc(hidden)]` + PLAN 约定；彻底删除留待全仓
   无调用后单独小 PR）。
**验收**：`grep -rn "SessionManager::global()" crates --include=*.rs` → 仅 `qaqh-session`
定义处、daemon main（若用）、以及 §10.3 白名单测试。

### PR-3-2（D5/G2）workspace 会话作用域 ToolCtx
**现状**：`workspace/src/runtime.rs` 的 `set_context`/`set_mode`/`files_read`、
`workspace.rs` 的 `set_process_workspace` 进程级全局组。
**步骤**：新增 `ToolCtx`（session_id / cwd / mode / files_read 视图）显式传参贯穿
`execute` 路径；serve 模式串行队列**保留**（Q3a——WAL 语义简单，Z6 行为不变），
但保留理由已从"全局互斥刚需"降级为"顺序性选择"。
**验收**：`grep -rn "runtime::set_context\|set_process_workspace" crates/qaqh-runtime/src/agent` → 0；
workspace lib + serve 集成全绿（含 PR-0-3 修复的那批）。

### PR-3-3（D3 搭车）cwd 由宿主注入
**现状**（勘误：引用在 `code_delta.rs:91` 与 `workspace.rs:35-36`，提案误记 file_query.rs:91）。
**步骤**：宿主构造 `ToolCtx` 时解析 cwd 注入（解析权威见 PR-4-1 / Q2a）；workspace 不再
直读 `qaqh_session::workspace`。serve 子进程模式下 cwd 随 `/execute` 请求体下发（现有
`host_workspace` 字段通道，行为不变）。
**验收**：`grep -rn "session_workspace\|qaqh_session" crates/qaqh-workspace/src` → 0
（Cargo.toml 依赖移除）。

### PR-3-4 cancel 残留复查
- 第一轮 PR-6 已修主路径；本项为结构性复查：全仓 grep 进程级 cancel flag，确保全部走
  per-session `CancelToken`。
- **验收**：`session_inprocess` 扩展用例"跨会话 cancel 不互相影响"落地并全绿。

---

## 6. Phase 4 —— 周边归位（激进扫尾）

### PR-4-1（E1）会话工作区单一属主
- `session/src/workspace.rs`（407 行，`WorkspaceStore`，OnceLock 单例，写
  `{data_dir}/workspaces.json`）——**Q2a**：留在 session 但模块更名 `session::grouping`，
  消除与 `qaqh-workspace` crate 的命名冲突；PR-3-3 之后它成为 cwd 解析唯一权威
  （workspace 不再直读）。
- 行为不变，仅归属与命名；`qaqh-session` 对外 re-export 同步更名（旧路径 `pub use` 别名
  过渡一个版本）。

### PR-4-2（F3）删 subagent legacy fallback
- **Q4a**：实测生产唯一宿主是 runtime（`registry.rs:239,251` 全走 `spawn_subagent_inprocess`；
  `host.rs:12` 确认 daemon 装配 `install_host` 一次）。删除 `lib.rs` 内 legacy HTTP/SSE
  fallback（`Client::connect` 直连分支，~`lib.rs:600-680` 区段）；`SubagentHost` trait 的
  `ContentRef`/`EventBatch`（现从 `qaqh_client` re-export，`host.rs:18,22`）改用
  domain/ringing 类型，**解除 subagent→client 依赖**。
- 验收：`grep -n "qaqh-client\|qaqh_client" crates/qaqh-subagent/Cargo.toml` → 0；
  `subagent_inprocess` 全绿。

### PR-4-3（D1 收尾）serve 形态确认
- **Q1a**：维持 `[[bin]] name = "qaqh-workspace"`（实测现状，`workspace_supervisor.rs:227`
  以同目录 `Child` 拉起）。本 PR 只补 Z6 跨版本走查清单（§10.4）并确认 `[[bin]]` 依赖 lib
  的构建边界无泄漏。不新增 crate。

### PR-4-4 卫生项
- msgloop 残留引用清零复核；`agent/util` 瘦身；各 crate lib.rs 模块表与实际对齐；
  `tool_mode.rs:4` 等历史注释中的 crate 名更新（若 PR-2-1 未覆盖）。

---

## 7. 开放决策拍板（Q1–Q7，全部采纳推荐项）

| # | 决策 | **裁定** | 依据（实测） |
|---|---|---|---|
| Q1 | serve/CLI 形态 | **a) 维持 `[[bin]]`** | supervisor 依赖同目录二进制；拆 crate 只加数量不加边界 |
| Q2 | 会话工作区归属 | **a) 留 session、更名 `grouping`、cwd 唯一权威** | 三方持有中 session 已有持久层与迁移测试，移动放大 Z5 风险面 |
| Q3 | serve 串行队列 | **a) 保留** | Z6 行为冻结优先；并发化收益无实证需求 |
| Q4 | legacy fallback | **a) 删除** | 生产唯一宿主 runtime 全走 in-process；无 TUI 外嵌入场景证据 |
| Q5 | 簿记落盘通道 | **a) Effect 家族**，落地为 `PersistOp` 批量队列 + `take_persist_ops()` drain | flush_meta 20+ 调用点零改动；单线程写序逐字节保真（R-3） |
| Q6 | endpoint 解析时机 | **a) turn 开始一次性入 `AgentState`** | 三处收敛为一次；engines 变纯读 |
| Q7 | 测试白名单与用例集 | 见 §10.3（grep 白名单）、§3 PR-1-6（影子验证）、§10.4（WSL 走查） | — |

---

## 8. 风险登记落地（对应提案 §6）

| # | 落地方式 |
|---|---|
| R-1 | §1 通用 PR 纪律（两段 commit + color-moved 评审） |
| R-2 | PR-0-3 修复 serve 路径测试 + PR-4-3 走查清单；Z6 契约测试进每阶段出口门 |
| R-3 | PR-1-6 影子验证（逐字节 JSONL 对比）+ 旧会话回放集成测试 |
| R-4 | PR-2-3 模块三规则 + `ringing/mod.rs` re-export 白名单 + 评审检查单 |
| R-5 | PR-1-2 CI grep：skills Cargo.toml 零 qaqh-* 依赖 |
| R-6 | §0.1 实测基线（已修正提案假设）；每阶段出口重跑并追加当日数字 |

---

## 9. 提案勘误表（2026-08-30 实测 vs 提案断言）

| 提案断言 | 实测 | 影响 |
|---|---|---|
| B5/P1-8：`Config::load()` 1 处（engine_session.rs:80） | **3 处**：+ `state/agent.rs:312,328` | P1-8 范围扩 |
| B6/P1-5：直写 global() 6 处 | **7 处**（+ `types.rs:453`）；另有 lifecycle.rs:20,28,261 读 + `generate_seed`/`now_epoch` 静态助手 6 调用点 | P1-5 范围扩（含静态助手 re-export 方案） |
| B4：project_turns 消费方 1 处 | **3 处**（+ `timeline_rebuild.rs:24`、`tests/timeline_rebuild.rs`） | P1-4 范围扩 |
| D3：`file_query.rs:91` | 实为 **`code_delta.rs:91`**（file_query 无此引用；`workspace.rs:35-36` 另有） | 行号勘误 |
| D4：execution.rs:389 组装 CodeDelta/TaskInfo | `execution.rs:389` 实为 **SkillEffect 激活分派**；CodeDeltaRecord 组装在 `code_delta.rs:6-55`；TaskInfo 在 `todo.rs:112` ✓ | 表述勘误，结论不变（本 PLAN 不单列 D4 改动项，随 P3-2 ToolCtx 自然归位） |
| C3：worker.rs 头注释（暗示 runtime） | 位于 **`crates/qaqh-ringing/src/worker.rs:4`** | 引用勘误 |
| B2：skill_context ~600 行 | **830 行** | 工作量微调 |
| R-6：基线"706/706 全绿" | **4 target / 7 test 失败 + clippy 1 error**（§0.1） | Phase 0 扩容 +1d |
| flush 调用面（提案未量化） | flush_meta 17 处 / snapshot_full 3 处调用（+ message 内部各 1 处） | 决定 PersistOp 队列而非逐点改签名 |
| V3 初判"serve `/execute` 404 回归"（9c946e7/86625a7 嫌疑） | **不成立**：实为 `0946afe` exec 拆分后测试未同步（词表含 exec + 测试载体 exec）；`process_kill_preempts` 已自行回绿 | PR-0-3 处置从"修回归"改为"测试追认" |
| Z8 词表（登记时 19 工具含 exec） | **18 工具**（exec 拆分退役，`register_exec_for_compat` 不注册） | `default_registry_exposes_the_formal_tool_vocabulary` 期望 vec 已按 18 追认 |

实证复核通过（无修正）：A1 五处行号逐字命中；A2（store.rs:126,809,972 + agent.rs:628 注入）；
C1（actor.rs 全部行号 + registry.rs 七处散点）；C2 唯一消费方；D1（supervisor Child 拉起 local/WSL 双模式）；
D2（runtime.rs:250-261 + read_image 间接）；D5；E1（workspace.rs 407 行 OnceLock 单例）；
F1（lifecycle.rs:198,229,267 / registry.rs:67,87 逐字命中）；F2（engine_input.rs:91）；
F3（host.rs:18,22 + lib.rs:642 区段）；LOC（msgloop 12,028 ≈ 提案 ~12k）；skills 零内部依赖。

---

## 10. 统一验收门

### 10.1 每 PR / 每阶段出口命令
```bash
# 全量测试（不低于基线：0 failed 或失败项均已在 §0.1 登记）
cargo test --workspace --no-fail-fast
# 静态检查
cargo clippy --workspace --all-targets 2>&1 | grep -c "^error"   # → 0
# 阶段出口 grep（Phase 1 十连、Phase 2/3/4 各自条目，见对应 PR）
```

### 10.2 Z1–Z8 契约锚点映射（PR-0-4 评审确认）

| Z | 锚点测试（现状） | 备注 |
|---|---|---|
| Z1 | `qaqh-ringing` 单测 + `msgloop/tests/inprocess_loop.rs`（合并后随迁 runtime） | 常量 `RINGING_SCHEMA/VERSION` 在 `qaqh-ringing/src/protocol.rs` |
| Z2 | `qaqh-daemon/tests/daemon_ws.rs` + `qaqh-proto` control 单测 | `CONTROL_PROTOCOL_VERSION` 于 `proto/control.rs` |
| Z3 | `frontend-contract.md` 冻结面 ↔ client envelope 测试 | PR-0-4 已核对：sse_decoder ×8 / endpoint ×3 / types ×2 + **discovery 兼容解析锚点（PR-0-4 新补，ws://→http:// 冻结面）** |
| Z4 | `config_api` dto 单测（apply_patch_*）+ `base_url_preset_guard` + `theme_notifications` | 当日全绿 |
| Z5 | `qaqh-session` 单测 + `migrate.rs`；**PR-1-6 新增旧会话回放集成** | R-3 影子验证同 PR |
| Z6 | workspace serve 单测（backend/execution/serve::tests）+ supervisor 装置 | **PR-0-3 已回绿**（载体 exec→bash）；WSL 走查清单 §10.4 |
| Z7 | `qaqh-client` 单测 + `lease_renegotiation` | 当日绿 |
| Z8 | `registration::default_registry_exposes_the_formal_tool_vocabulary` + `schema_spot_check` | **PR-0-3 已回绿**；词表按 bash/pwsh 拆分追认为 18 工具（见 §9） |

### 10.3 grep 白名单（允许残留的位置）
- 各 crate `tests/` 目录（集成测试自带装配，如 `SessionManager::init`）。
- `#[cfg(test)]` 模块。
- `qaqh-workspace/src/main.rs`（CLI 入口的 `Config::load()`，P1-10 白名单）。
- `qaqh-session` 自身定义处；daemon main（P3-1 后）。

### 10.4 WSL 走查清单（R-2，PR-4-3 归档；CI 无 WSL 时每阶段出口手动过一遍）
1. daemon 以 WSL 模式拉起 serve（`workspace_supervisor.rs:246` wsl.exe 路径）→ `/health` 200。
2. exec / edit 工具经 serve 往返成功；Bearer 鉴权拒绝无 token 请求。
3. `POST /subagent` 注册进程记录成功。
4. serve 崩溃重启（端口变化）后已运行 worker 不受影响（supervisor 环境变量重注入语义）。
5. 跨版本：旧 serve 二进制 + 新 daemon 组合一次（Z6 向后兼容抽查）。

---

## 11. 里程碑与中止点

- **Phase 1 是净收益兜底**：即使后续中止，10 项搬家 + message 纯度本身即边界修复。
- Phase 2 依赖 Phase 1 全部合入（先合并后搬 = 把寄居者搬进新家再抠出来，禁止）。
- Phase 3/4 无硬依赖，可按人力并行或择机；但 PR-4-1 依赖 PR-3-3。
- 跨阶段无共享状态（全是移动 + 接口收窄，无数据迁移）；阶段内按 PR revert 即回退。
