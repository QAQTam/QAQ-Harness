# QAQ-Harness 第一轮代码审查修复计划（PLAN）

> 状态：**Step 0 已复核通过 → 修复批次已执行完毕（2026-08-24）**。
> 复核与逐项状态见 `docs/STEP0-verification.md`（含对抗性二验附录与修复进度日志）。
> 汇总：42 项中 **37 项已修/落地替代方案、3 项确认误报翻案（L-msgloop③ 部分成立降级、M-doc 与 M-low② 合并为相邻化 dedup 统一修复）、2 项经预研后显式推迟（H5-B 完整高水位方案、L-msgloop① Suspended phase 设计）**，均有记录与理由。全程 workspace lib 测试 706/706 绿、clippy --workspace --all-targets 0 error、关键集成测试（ask_user_lifecycle / permission_lifecycle / session_inprocess / subagent_inprocess 等）全绿。
> 审查日期：2026-08-23 · 分支基线：当前 main · 审查范围：全仓 15 crate / ~70k 行 Rust
> 契约约束：`docs/frontend-contract.md`（envelope 形状与错误 code 集合冻结；行为变更走 RFC）

---

## 0. 总览

| 指标 | 数值 |
|---|---|
| 发现总数 | **42**（🔴高 7 / 🟠中 22 / 🟢低 13） |
| 批次 | P0 核心 6 项 · P1 竞态/泄漏 23 项 · P2 清扫 13 项 |
| 跨 crate 项 | 6（H3、G1、G2、M1、W7 关联面、取消语义契约） |
| 需前端跟进 | 3 个 issue（RFC ×1、frontend-sync ×2），均落在本仓库 |
| 预估总工作量 | ~9.5 天（含测试） |

---

## 1. 审查方法与可信度说明

1. CodeGraph 全参数探索确认结构：`qaqh-daemon`(HTTP/SSE 入口) → `qaqh-runtime`(编排层：QaqhService/RingingHub/AgentRegistry) → `qaqh-msgloop`(agent 循环引擎) → `qaqh-workspace`(工具执行)，底层 `qaqh-message`/`qaqh-session`/`qaqh-ringing`/`qaqh-domain`。
2. 四个子代理分别深读：msgloop turn 状态机、RingingHub 事件总线、message 存储层、workspace 工具执行。每个子代理被要求给出 file:line + 代码摘录 + 具体触发时序。
3. **已知风险**：审查过程中 ripgrep `-r`（replace）曾被误用为递归搜索，部分摘录可能被替换污染；且子代理结论未经独立复现。因此 **Step 0 复核是一票否决关卡**：任何 P0 项若复核否决，对应 PR 取消并更新本计划。

---

## 2. 发现清单（按模块，42 项全量）

> 严重度定义：🔴 数据损坏/状态机破坏/安全边界绕过；🟠 特定时序或负载下触发的竞态、资源泄漏、取消失效；🟢 卫生问题与潜伏型契约缺陷。

### 2.1 qaqh-msgloop（agent 引擎，9 项）

| ID | 位置 | 问题摘要 | 级别 | 归属批次 |
|---|---|---|---|---|
| H1 | `engine_input.rs:169` + `loop_core.rs:1424` | 挂起 turn 期间收到新用户消息直接开新 turn；残留 suspension 被迟到 resume 命中：给旧 turn 发携带新 turn 工具的终态事件 + 幽灵 lap。四个 resume handler 只查 call-id 成员资格不查 turn/session 存活；`backfill.rs:54` 恒读 store 最后一步 | 🔴 | P0 |
| H2 | `loop_core.rs:1245` + `engine_turn.rs` 四 handler | `SessionCreate{close_current:false}` 绕过 `prepare_session_switch`/`reset_all_engines`，msg store 整包替换但 `TurnState`/`tool.pending` 存活指向死会话；仅无人调用的 `TurnEngine::resume:139` 校验过 session_id | 🔴 | P0 |
| M1 | `services/conflict.rs:14` ← workspace `permission.rs` | 写冲突检测匹配 `"patch"` 但模型实际调用 `apply_patch`（`copy_range` 同样漏匹配）→ 同文件两 patch 并行竞态丢更新 | 🟠 | P1 |
| M4 | `turn_lap/admit.rs:330` + `loop_core.rs:1762` | 重复 tool_call_id 错误返回 `Outcome::Handled`（no-op）：phase 卡死 `ToolsRunning`，timeline 不 seal，`complete_user_turn`/flush 跳过，前端永远转圈 | 🟠 | P1 |
| C1 | `admit.rs:464,95` | 并行批循环顶部无 cancel 检查：上一批观察到取消后仍继续 spawn 新工具线程并发 Running 事件（串行路径 :552 已有检查） | 🟠 | P1 |
| C2 | `loop_core.rs:1479-1560` | `ConversationCancel` 同时置 loop token 与全局 `set_cancel(true)`，但 UI 快捷 `ToolInvoke` 路径不清零 → 之后所有 UI 工具在 `execution.rs:63` 秒失败 | 🟠 | P1 | **〔已实证 2026-08-24〕** 最短复现：会话 A 流式中 → 切到 B（A 后台继续）→ B 点 Stop → daemon 处理 B 的 cancel 时毒化工作区级全局 flag → 此后 A 及一切新会话的工具全部秒拒 CANCELED，仅重启 daemon+workspace 恢复（全局 flag 无 TTL、无自动复位路径的铁证）。前端发送端已排查无辜：`composer_bar/view.rs:314` 发送作用域正确、seed 点击时快照、Stop 按钮按 per-seed is_streaming 显隐。修复落点全在后端 PR-6。
| T-low① | `loop_core.rs:1758` | `YieldToUser` 后 phase 不复位 Idle：`inject()` 吞注入、compact 后派发拒绝、仪表盘误报 tools running | 🟢 | P2 |
| T-low② | `engine_tool.rs:407,590` | `pending_plans`/`pending_todo_activation` 从不填充 → plan/todo review 全链路死代码；潜在陷阱：`handle_plan_response` 不排空 co-pending asks 会留悬空 tool_use | 🟢 | P2 |
| G4 | `loop_core.rs:1075,1523` + `:756` | `start_compact` 不检查 phase/suspension：挂起等权限期间 `check_pending_compact`（主循环每迭代轮询）可应用压缩；悬空 tool_use 所属 turn 若被折叠，迟到 grant 经 `handle_permission_resolved`（engine_turn.rs:159 只查 call-id 成员资格）执行出孤儿结果（与 H1、H4 组合）→ 工具静默失效、前端零提示 〔排查补充 2026-08-24，源自"压缩后工具静默失效"排查〕 | 🟠 | P1 |

*msgloop 干净区：PacedEmitter 即时发送不重排；round 编号一致；deferred_ringing FIFO 恰一次；serial 冲突序在权限挂起前后保持正确（approve/deny/mixed 排列已验证）。设计取舍（非缺陷，勿当吞错修）：`done_seen` 之后的 gate 错误仅 log::warn（engine_turn.rs:952-965），避免否定已完整流式输出的作答。*

### 2.2 qaqh-runtime（RingingHub/registry/projection，9 项）

| ID | 位置 | 问题摘要 | 级别 | 归属批次 |
|---|---|---|---|---|
| H3 | `ringing/hub.rs:810`（触发点 `qaqh-daemon/ringing_http.rs:1745`） | `seal_orphan_channel_state` 的 conversation/compact/tool 三段无 liveness guard：live turn 进行中触发 bootstrap 就发布假可靠 `ConversationCancelled` 或取消等待中的 permission | 🔴 | P0 |
| R2 | `registry.rs:563,205` | force 收尾在 `spawn_with()` **之后**执行：新 worker 刚发的 ask/TurnOpened 可能被当 ghost 封掉 | 🟠 | P0（并入 H3 协议） |
| H3c | `hub.rs:947` | 孤儿交互收尾后无条件 `live_interactions.remove(seed)`，抹掉并发发布的新 ask 注册 → 下次 bootstrap dismiss 用户活 ask（重建了该守卫本来要修的 bug） | 🟠 | P0（并入 H3 协议） |
| R1 | `hub.rs:1184` | `sequencer.next()` 在 channels 锁外分配：两并发发布者可乱序提交 seq 11 先于 10 → 回放乱序、running 状态陈旧、磁盘顺序坏 | 🟠 | P1 |
| R3 | `hub.rs:449` | lazy-load I/O 失败静默 return：该 seed 以 `channel_seq=1` 重新发布，重启重放后永久乱序重复 | 🟠 | P1 |
| R4 | `hub.rs:1365` | per-seed replay 合并 journal+replaceable（HashMap 序）不排序 → SSE 批内 stream_seq 回退（频道级 :1424 有排序，此路径没有） | 🟠 | P1 |
| R5 | `ringing/journal.rs:82` | compact_round_deltas 移除条目但 `seen_event_ids` 永不收缩 → 流式 delta 场景内存无界增长 | 🟠 | P1 |
| R6 | `projection.rs:183,122` | `cancelled`/`last_failure` 写一次永不清除 → 会话余生所有快照误报已取消 | 🟠 | P1 |
| R-low | `journal_store.rs:138-199` | rewrite 的 `remove_file`→`rename` crash 窗口可丢整本 seed 历史（Unix rename 本可原子覆盖） | 🟢 | P2 |

*runtime 干净区：reliable 事件不受背压丢失（内存日志有界窗口）；event_id 由单调 stream_seq 构成发布期不可重复；lazy-load 水位恢复精确；锁序 store→timeline 一致且无跨 await 持锁；失败 intent 不烧 timeline_seq；孤儿收尾幂等。设计注意项（非 bug）：`running`/`pending_permission` 单槽仅在严格串行工具执行下正确；shutdown 时 seal_all_orphans 触发全量磁盘懒加载延迟尖峰。*

### 2.3 qaqh-message / msgloop 交界（存储层，11 项）

| ID | 位置 | 问题摘要 | 级别 | 归属批次 |
|---|---|---|---|---|
| H4 | `store.rs:1217,1230,1179` | 重复压缩把合成摘要计入 `skip`，`[Compacted N]` 少报 → `truncate_before_turn` 的 seq→index 映射漂移：**undo 最新真实 turn 静默失败或删错范围**（常规路径） | 🔴 | P0 |
| H5 | `engine_input.rs:257` + `store.rs:1169` | 空闲期注入 `allocate_turn_id()` 但 SUBAGENT sink Trailing 不落 Turn → phantom seq slot：undo 最新真实 turn 返回 false、对旧 id 截掉更新的 turn；busy 路径无此问题 | 🔴 | P0 |
| G1 | `context_flow.rs:315,361` | 失败/dedup 的 ingest 仍上报 command_id=committed（注入日志假成功=丢失注入）；`last_keys` 在 store 接受前写入 → 合法重试被拒 | 🟠 | P1 |
| G2 | `engine_compact.rs:158` ↔ `store.rs:1216` | `kept_user_count`(扁平 user 消息数，含 subagent 报告与旧摘要) 当 `keep`(Turn 结构数) 用 → 注入密集时 skip==0 早退但发 `CompactFinished{Completed}`（压缩静默停摆）；且 run_auto_compact 仍返回 true（engine_turn.rs:699→719）→ record_auto_compact_result(true) 不设阻断（agent.rs:260），每个后续 lap 在 gate 前内联阻塞重跑一次摘要 LLM 调用（livelock，后续轮次极慢）；count 偏低时（system-input turn 被 :102 过滤）反向过度折叠近期 turn；边界切半回合多折一 turn 〔排查补充 2026-08-24：补 livelock 与过度折叠后果〕 | 🟠 | P1 |
| G5 | `store.rs:597-608` | 运行时孤儿 tool_result 仅 log::error 后静默丢弃、无任何领域事件（注意与 M-low③ 死分支区分）：G4/H1 场景下工具"已授权执行却消失"，前端零提示——不可观测性的直接堵点 〔排查补充 2026-08-24〕 | 🟠 | P1 |
| G3 | `store.rs:664` | 写序合并允许 trailing 注入插在 tool_use 与迟到 result 之间 → Anthropic/OpenAI 直接 400（挂起期间 idle 注入 drain 时触发） | 🟠 | P1 |
| M-low① | `store.rs:936` | compact 模式 `snapshot_full` 清 `pending_save` 不归档：原始档永久分叉（checkpoint 损毁即 fail-closed 无法 resume） | 🟢 | P2 |
| M-low② | `store.rs:356` | trailing 全历史文本 dedup 合并相隔数小时的相同报告（tool result 安全——按 call_id 键控） | 🟢 | P2 |
| M-low③ | `store.rs:567` | `push_tool_result_inner` 死 fallback 分支（前置逆向扫描已覆盖同谓词） | 🟢 | P2 |
| M-low④ | `store.rs:1008,1042` | replay 按文本前缀判别：无标记的 system/developer 消息重启后消失；`"[SUBAGENT "` 开头的真实用户消息被改类 | 🟢 | P2 |
| M-doc | `context_flow.rs:170` | last-key dedup 的 A,B,A 放行为文档化设计（A,B,A passes by design）——不改行为，补注释与测试锁定 | 🟢 | P2 |

*message 干净区：fsync 先于 meta 写（crash 后以文件为准恢复安全）；单次压缩 undo 映射正确（含边界测试）；compact trailing-fold 符合 A1 语义双侧有测试；孤儿 tool_use 重启注入 `[RESTORE]` 结果无悬挂 call_id；resume 侧水位单调守卫完备。持久化附注：torn tail 一行损坏即永久 fail-closed（见 L-session）。*

### 2.4 qaqh-workspace / todo / 进程管理（12 项）

| ID | 位置 | 问题摘要 | 级别 | 归属批次 |
|---|---|---|---|---|
| H6 | `process_registry.rs:276` + `exec.rs:578` | Unix spawn 未设进程组、kill 仅 SIGKILL 单 pid：孙进程持管道写端 → reader 线程永不 EOF 泄漏（exec.rs:712 注释却声称杀树；Windows taskkill /T 正常） | 🔴 | P1 |
| M13 | `manager.rs/safety.rs`（归属 crate 待 Step 0 定位） | `is_path_in_workspace` 字符串前缀比较无组件边界、`..` 未归一化 → `/home/u/proj-backup/…` 绕过 Destructive 出工作区阻断 | 🔴(安全) | **P0** |
| W1 | `todo.rs:347` | `"T1-T4000000000"` 无界展开 OOM/卡死 agent loop（创建侧有 MAX_CREATE_ITEMS=20，展开侧没有） | 🟠 | P1 |
| W2 | `todo.rs:97,272` | GoalEngine 公开 save 绕过 `TODO_LOCK` 且共用 tmp 文件名 → 丢失更新/todo.json 损坏后静默报空 | 🟠 | P1 |
| W3 | `permission.rs:188` | `extract_target_paths` 只处理 `exec` 的 cwd；bash/pwsh 同 schema 不匹配、apply_patch 目标在 patch 文本未解析 → Level-3 空绑定自动批，审计/确认框失明 | 🟠 | P1 |
| W4 | `process_registry.rs:263` | kill 已退出进程谎报成功并把 `Exited(code)` 改写为 `Killed`（exit code 丢失）；NOT_FOUND 提示文案相反 | 🟠 | P1 |
| W5 | `process_registry.rs:150` | PTY stdin 单次 `write` 非 `write_all`：部分写谎报全成；满 tty 缓冲阻塞至 180s 超时 | 🟠 | P1 |
| W6 | `process_registry.rs:71` | 注册表条目只进不出（无任何 remove API，~8KB/条）长会话内存单调涨；u32 id 无 wrap 处理 | 🟠 | P1 |
| W7 | `permission.rs:205` | 相对路径授权绑定进程 cwd 而执行解析用 workspace root → 授权/审计指向从未使用的目录（当前无决策翻转但资源绑定错位） | 🟠 | P1 |
| C3 | `lib.rs:264` + `manager.rs:330` | actor 线程读 thread-local cancel，另一线程的全局 `cancel_tool` 传不到子代理 manager → 子代理长 exec 取消无效直到自身 timeout(≤3600s)；全局 flag 仅输入路径清零 | 🟠 | P1 |
| W-low① | `exec.rs:765` | 取消路径 2s 收集超时后伪造 `[WARN] stdout pipe timed out` 替代真实捕获（H6 修后 EOF 自然到达，改为返回部分捕获兜底） | 🟢 | P2 |
| W-low② | `process_registry.rs:182` | trim 以字节计长、按 chars 跳过 → 多字节(CJK)输出保留量最多减半（`char_safe_tail` :20 现成可用） | 🟢 | P2 |

*workspace 干净区：try_wait 终态缓存健全（防复活、防永跑）；Mutex 中毒统一恢复；输出捕获 5MB 硬顶 OOM-safe 且 UTF-8 断字延迟 ≤3 字节；execution.rs:43-52 对 args 做 admit→execute 双提取 TOCTOU 防御；todo 持久化 tmp+rename 原子、V2 字段守卫拦截元数据走私。*

---

## 3. 修复批次

### P0 —— 核心正确性（常规路径可触发数据/状态破坏）

| ID | 模块 | 范围 | 修复方向 | 回归测试 |
|---|---|---|---|---|
| H1 | qaqh-msgloop | 单 crate | 挂起期间收到 `SendMessage` 显式 abort-and-detach 旧 suspension（镜像现有 `undo_conflict` 守卫）；resume 前 revalidate store last step 属于 `saved.turn_id` | `suspended_turn_replaced_by_new_input` |
| H2 | qaqh-msgloop | 单 crate | 该分支强制走 `prepare_session_switch`/`reset_all_engines`；四个 resume handler 统一补 `saved.session_id == seed` 检查（抽公共 helper，见 §4.1） | `session_switch_invalidates_suspension` |
| H3(+R2+H3c) | qaqh-runtime（daemon 触发点确认） | **跨 crate** | 代际收尾协议（§4.2）：liveness gate + 先收尾后拉起 + 条件删除，**单 PR 原子落地** | 并发 bootstrap×live-turn 序列测试 |
| H4 | qaqh-message | 单 crate | `turns[0]` 为摘要时 `skip_real = skip-1`、`N_total = prev_N + skip_real`（经 `compacted_turn_count` 解析） | `repeated_compact_undo_mapping` |
| H5 | qaqh-msgloop | 单 crate | **方案 B（已定）**：保留事件 wire 不变，`truncate_before_turn` 改用分配器高水位推算索引（扣除无 turn 的 id）；方案 A 另立 RFC issue | undo-after-injection 用例矩阵 |
| M13 | qaqh-workspace | 单 crate | 组件级 `Path::starts_with`（对齐 `permission.rs::all_within_workspace`），相对路径先归一化 `..` | sibling-prefix 与 `..` 用例矩阵 |

### P1 —— 竞态 / 取消失效 / 资源泄漏（按时序或负载触发）

| ID | 修复方向一句话 | 测试 |
|---|---|---|
| M1 | 弃手写名单，写集合改从 `extract_target_paths`/handler 声明资源推导；与 admission 的 Write category 交叉校验 | `same_file_double_apply_patch_serialized` |
| M4 | 重复 call_id 返回终态 outcome（或 seal + `phase=Idle`）再返回，禁止裸 `Handled` | duplicate-id 后 turn 必有终态事件 |
| C1 | 并行循环顶部 `if cancel.is_set() break`（对齐串行 ：552） | 批间取消不再产生 Running |
| C2 | ToolInvoke 派发前 clear 两处 token | `cancel_then_ui_toolinvoke_succeeds` |
| C3 | 最小修：`is_cancel()` OR 全局 flag；彻底修随 §4.3 取消契约 | 子代理 exec 可被全局取消 |
| H6(+W-low①) | spawn 设 `process_group(0)`，kill 走 `killpg(SIGKILL)`；收集超时时返回部分捕获而非伪造占位 | 孙进程存活验证 + 管道 EOF |
| R1 | 序号分配移入 channels 临界区（seq 序 = 提交序）⚠️ 热路径需基准对比 | `concurrent_publish_monotonic_replay` |
| R3 | 加载失败 publish 返回错误（fail-closed），禁止未加载 seed 发布 | `lazyload_failure_fails_closed` |
| R4 | 合并后按 `stream_seq` 排序（对齐频道级 ：1424） | 批内单调性断言 |
| R5 | 按 `stream_seq <= evicted_through` 同步剪 id 集 | 长流式会话内存平稳 |
| R6 | `TurnStarted/TurnCompleted` 清 `cancelled`；`last_failure` 改每次覆盖 | 快照字段生命周期测试 |
| G1 | 仅 `stored‖deduped` 才回执 command_id；`last_keys` 移到确定性结果之后 | 失败 ingest 日志不标 committed；重试可达 |
| G2 | store 新增按 turn 边界的 `compact_from_turn_index` API；engine 把 `msgs[kept_idx]` 经 msg_id 映射回所属 turn（排除 subagent 报告与 `[Compacted` 文本）；apply_compact no-op 时如实上报 Failed/Cancelled（禁发 Completed 假成功）；run_auto_compact 仅在 turns 实际减少时返回 true；与 H4 同 PR 改压缩簇 | 注入密集会话压缩不再停摆；`no_op_compact_reports_failed`；`post_compact_turns_removed_matches_store`；`auto_compact_livelock_suppressed` |
| G4 | start_compact 入口补守卫：`phase != Idle || is_suspended()` 时拒绝并回 `OperationFailed(compact_busy)`（镜像 ：1490 undo_conflict 拒绝模式）；如需支持挂起期压缩，应用前必须经 §4.1 validate_suspension 复核悬空 step 仍存活 | `compact_during_suspension_rejected` |
| G3 | 最后 step 有未满足 tool_use 时拒绝/延后 trailing 写入（`has_pending_tools()` 守卫） | suspend→inject→grant 写序正确 |
| G5 | push_tool_result_inner 孤儿丢弃路径升级为领域事件：`OperationFailed`(scope=Tool, code=orphan_tool_result) + timeline 工具块 failure 标注；保留 log::error | `orphan_tool_result_emits_domain_error` |
| W1 | 展开上限（≤1000）+ 先与现存 id 求交再物化 | `todo_range_capped` |
| W2 | 锁下沉进 `read_store/write_store` + 唯一 tmp 名 | GoalEngine 与工具并发写不丢失 |
| W3 | 名称匹配扩为 `exec\|bash\|pwsh`；解析 patch 头 `*** Update/Add/Delete File:` | bash cwd 与 patch 目标进授权记录 |
| W4 | status≠Running 或 child=None 时返回 false/no-op，保住 exit code | kill-after-exit 语义测试 |
| W5 | `write_all` 循环 + WouldBlock poll 截止 + 写间隙查 cancel | 满 tty 缓冲可超时可取消 |
| W6 | register 时惰性驱逐终态条目（如 >10min）；`checked_add` | 长会话条目数有界 |
| W7 | `resolve_target_path` 内改走 `resolve_workspace_path` | 授权路径=执行路径断言 |

### P2 —— 低危清扫（一次主题收口）

| 批 | 内容 |
|---|---|
| L-msgloop | ① YieldToUser 后 phase 复位 Idle ② plan/todo 死路径标注或删除 + 补 co-pending asks 陷阱注释 ③ start_compact 对 build_prompt_and_meta()==None 分支补终态回执（现 loop_core.rs:1092 if-let 无 else：无 ack、无 OperationFailed、无 CompactFinished，前端零反馈；测试 `compact_noop_emits_terminal_ack`）〔排查补充 2026-08-24〕 |
| L-runtime | journal rewrite 删除 Unix 侧多余 `remove_file`（cfg 仅 Windows 保留） |
| L-message | ① snapshot_full 先 `save_append(pending_save)` 再写 checkpoint ② trailing dedup 改窗口化/command_id 键 ③ 死分支删除 ④ replay 判别改用持久化 source/name 字段弃文本前缀 |
| L-ws | trim 复用 `char_safe_tail` 修正多字节裁剪 |
| L-session | 读档对**最后一行** torn tail 容错截断（非末行损坏仍拒绝） |
| M-doc | A,B,A 行为补注释 + 测试锁定（不改语义） |

---

## 4. 跨项联合设计点（防碎片化修补打出新洞）

1. **Turn 身份守卫簇（H1+H2+M4+H5-B）**：抽单一 helper `validate_suspension(ctx, saved)` —— 校验 `session_id` + turn 未被取代 + store last step 归属 `saved.turn_id`；四个 resume 入口与新输入入口共用；G4 若选择"挂起期允许压缩"而非直接拒绝，应用前亦须经此 helper 复核。
2. **代际收尾协议（H3+R2+H3c）**：① `seal_orphan_channel_state` 各段按 worker generation 门控；② respawn 严格 **seal(old gen) → spawn(new gen)**；③ `live_interactions.remove` 改条件删除。三处同一状态机，必须同 PR。
3. **取消传播契约（C1+C2+C3+H6+W-low①）**：明确三层信号（loop CancelToken / workspace 全局 flag / inflight 子进程 killpg）的置位者与清零者，写入 `docs/cancel-contract.md`（内部契约，非冻结面，无需 RFC）。
4. **压缩簇（H4+G2+G4）**：store 侧一次性重构计数与“按 turn 边界压缩”API，两个调用方同步切换；G4 的入口守卫与本簇同文件落地（PR-5），拒绝语义与 §4.1 validate_suspension 保持一致。

---

## 5. 已锁定决策（用户拍板）

| 事项 | 结论 |
|---|---|
| H5 | 方案 B 立即修（wire 不变）；方案 A 走 RFC issue |
| 取消传播契约 | 落成 `docs/cancel-contract.md` 正式文档 |
| L-session torn-tail 容错 | 纳入本轮 |
| 前端协同 | 全部以 issue 形式落本仓库（gh cli 已认证 QAQTam/QAQ-Harness） |
| Wire 安全线 | 所有修复不改 envelope 字段形状与既有错误 code；新错误码走加法兼容 |

## 6. gh issue 清单（执行到对应 PR 时创建）

1. `[RFC] Ringing V1：空闲期注入不再发射 TurnStarted/TurnCompleted（H5 方案A）` — 标签 `documentation`。含现状、提案事件形态、前端 checklist（活动指示器、时间线注入条目渲染、缺失 Turn 事件容错）。→ 随 PR-1 创建
2. `[frontend-sync] 挂起 turn 被新输入取代时将发出 Cancelled 终态（H1）` — 标签 `bug`。新终态事件场景 + 渲染路径与错误码白名单核对项。→ 随 PR-2 创建
3. `[frontend-sync] 工具确认对话框将开始显示 bash/pwsh cwd 与 apply_patch 目标路径（W3/M13）` — 标签 `enhancement`。前端无必改项（展示自动变准确）；可选增强：确认框按目标路径分组渲染。→ 随 PR-4 创建
4. `[frontend-sync] 会话标签条改用稳定 key 选中并消费 session.created（泄露/丢失双修复）` — 标签 `bug`。背景：winui-app 标签条 `on_selection_changed` 按 index 反查 seed（渲染列表与点击瞬间快照差一拍即错投会话→内容"泄露"）；`spawn_new_session` 以 15s 轮询 diff 猜新 seed。修复：① fork TabView 提供 key 化 selection 回调；② UI 消费既有 `publish_session_created`（ringing_http.rs lease attach 后发布，**无需协议改动**）；③ 创建成功后将 current_workspace 对齐到新会话归属（修标签不可见）。依赖：**无硬阻塞**，但 H2（同链路后端半边）须保持在 PR-2 不顺延；可选增强：本 PR 附带 `RingingCommandAck.data.seed`（加法兼容，符合 §5 wire 规则）。→ 随 PR-2 创建
5. `[frontend-sync] compact 错误可见性三连：dedupe_key=compact_failed 去重策略、CompactFinished{Cancelled} 无原因文案、no-op 压缩缺终态回执（G2/G4/L-msgloop③）` — 标签 `bug`。核对项：① 错误横幅是否按 dedupe_key 折叠连续 compact 失败（应显示最新一条而非静默吞掉）；② Cancelled/Failed 终态是否需要携带 reason 字段（加法兼容，符合 §5 wire 规则）；③ 压缩无可压内容时按钮的反馈与提示文案。→ 随 PR-5 创建

Issue 链接创建后回填到对应 PR 描述。

## 7. 执行顺序（PR 划分）

| 顺序 | PR | 内容 | 预估 |
|---|---|---|---|
| **Step 0** | 复核报告 | Read 逐条验证全部 P0/P1 行号与场景；定位 `manager.rs/safety.rs` 归属 crate；产出确认/否决清单贴入首个 PR 描述 | 0.5 天 |
| PR-1 | msgloop 身份守卫(上) | H5 方案 B + L-msgloop + M4 | 1 天 |
| PR-2 | msgloop 身份守卫(下) | H1+H2（`validate_suspension` helper 四入口复用） | 1.5 天 |
| PR-3 | runtime 代际收尾 | H3+R2+H3c（原子落地） | 1 天 |
| PR-4 | workspace 安全覆盖 | M13+W7+W3+M1（同文件/同主题聚合） | 1 天 |
| PR-5 | 压缩簇 | H4+G2+G4（顺带 L-msgloop③） | 1.5 天 |
| PR-6 | 取消语义簇 | C1+C2+C3+H6(+W-low①) + 新增 `docs/cancel-contract.md` | 1 天 |
| PR-7 | hub 序号簇 | R1+R3+R4+R5+R6+L-runtime | 1 天 |
| PR-8 | message 流水线 | G1+G3+G5 | 1 天 |
| PR-9 | 杂项中危 | W1/W2/W4/W5/W6 | 0.5 天 |
| PR-10 | P2 清扫 | L-message+L-ws+L-session+M-doc | 0.5 天 |

## 8. 验证策略与完成标准

**每 PR 强制**：回归测试先行失败→修复后通过；`cargo test -p <涉及crate>` 与 `cargo clippy --workspace --all-targets` 干净（仓库有 clippy.toml / justfile，以 just 配方为准）。

**专项**：
- Hub 热路径（PR-3/7）：跑 `tests/session_inprocess.rs` + `subagent_inprocess.rs` 全绿 + publish 微基准对比（>10% 回退需说明或退化为锁内取号缓存方案）。
- Wire 自查：diff envelope 字段集合与错误 code 枚举，零变化方准合入。
- 关键新增测试名见 §3 各表；另加 `undo-after-injection` 用例矩阵、并发 bootstrap 序列测试。

**完成标准**：42 项发现全部呈三态之一——已修 / 已否决(附理由) / 已立 issue；汇总到收尾报告并在本文档标记各项最终状态。

## 9. 风险与回滚

| 风险 | 缓解 |
|---|---|
| 子代理结论未二次验证（曾发生 rg -r 输出污染） | Step 0 一票否决关卡；任一 P0 否决即取消对应 PR 并更新计划 |
| PR-3 三处改动拆开会产生新孤儿窗口 | 已强制单 PR 原子落地 |
| R1 触及最热发布路径 | 微基准门禁；退化方案预置（锁内取号缓存） |
| H5-B 高水位推算逻辑自身出错 | 用例矩阵覆盖 busy/idle/refuse 三源；wire 零变化保证随时可回滚 |
| 前端对 H1 新终态事件的渲染未知 | wire 使用既有 Cancelled 形态；issue #2 提供核对清单，前端侧独立排期不阻塞后端合入 |

---

*本文档为第一轮审查的唯一事实源；后续轮次（第二轮建议范围：qaqh-gate 流式适配层、qaqh-skills/subagent、qaqh-client SDK）待本轮收敛后启动。*
