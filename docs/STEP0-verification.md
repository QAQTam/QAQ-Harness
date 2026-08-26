# Step 0 复核报告（PLAN.md 强制关卡）

> 复核日期：2026-08-24 · 方法：逐条 Read 源码验证 PLAN §2 全部 P0/P1 行号与触发场景
> 结论总览：**29 项 P0/P1 中 28 项确认（含 4 项行号偏差/机制细化），1 项部分确认建议降级，0 项否决**
> 范围说明：本报告只覆盖 P0/P1；P2 清扫项与"干净区"声明未在本轮复核范围。

## 一、P0（全部确认，无否决）

| ID | 判定 | 实际位置与证据 |
|---|---|---|
| H1 | ✅ 确认 | `engine_input.rs:169` `handle_user_input` 无任何挂起守卫，`allocate_turn_id` 后直接 `Outcome::ContinueTurn` 开新 turn；入口 `loop_core.rs:1424` 直通。迟到权限解析经 `engine_turn.rs:159 handle_permission_resolved` 仅查 `pending_permission_ids` 成员资格（:170-175）；`backfill.rs:54` 恒读 `last_step_tool_results()`——新 turn 的工具结果被归到旧 turn_id 发出，幽灵 lap 成立 |
| H2 | ✅ 确认 | `loop_core.rs:1245-1251`：`close_current=false` 仅 `clear_injections`+`finish_pending_compact`+`reset_compaction_coordination`，不走 `prepare_session_switch`(:380，会 reset_all_engines+清 suspension) 也不调 reset；随后 `session_eng.create()` 整包替换 store。`TurnEngine::resume`(engine_turn.rs:129，:139 唯一 session_id 校验)全仓无调用者。client 示例与多个 inprocess 测试确实传 close_current:false |
| H3 | ✅ 确认 | `hub.rs:806 seal_orphan_channel_state`：段1 active_turn→假 ConversationCancelled(:810-827)、段2 compact(:834-856)、段3 running/pending_permission→假 ToolFinished cancelled(:861-913) 均无 liveness guard；仅段4交互有活表守卫(:926-935)。触发点 `ringing_http.rs:1745` 每次 bootstrap 都调用 |
| R2 | ✅ 确认 | spawn 先于收尾，两处：`respawn_dead_agents` spawn:555 → seal:563-565；`get_or_spawn` spawn:177 → seal:200-205。force=true 无视守卫，可封掉新 worker 刚发布的交互/TurnOpened |
| H3c | ✅ 确认 | `hub.rs:946-950` 收尾后无条件 `live_interactions.remove(seed)`，可抹掉并发发布的新 ask 注册 |
| H4 | ✅ 确认（行号偏差） | 机制本体在 `store.rs:1242`（skip=self.turns.len()-keep 把合成摘要位计入）、`:1255`（[Compacted {skip}] 头部计数少报）、`:1204-1222`（truncate_before_turn 以头部数推 first_seq/idx）。数值推演：20 turn→compact(5)→+5→compact(5) 后 undo 最新真实 turn idx=19≥len(6) 静默返回 false；undo 旧 id 删错范围。计划引用 :1217/:1230/:1179 为同函数簇内偏差行 |
| H5 | ✅ 确认 | `engine_input.rs:257` 空闲注入取号后经 SUBAGENT source（Sink::Trailing, context_flow.rs:640）→ `push_trailing_system`(store.rs:338) 只 append trailing_messages 不建 Turn；busy 路径 lap 边界 drain 不取号。推演：M 真实 turn+1 phantom 后 undo 新输入 t{M+2} 得 idx=M+1≥len 返回 false；undo 幽灵 id 反截掉最新真实 turn |
| M13 | ✅ 确认＋归属定位 | 归属 **qaqh-workspace**：`manager.rs:366 is_path_in_workspace`，:383 用字符串 `abs_path.starts_with(&ws)`（无组件边界、join 后 `..` 未归一化）→ sibling 前缀与 `..` 均绕过；:215→`safety.rs:18-21` (Destructive,true)=Allow 即放行出工作区破坏性操作。对照 `permission.rs:253 all_within_workspace` 为 PathBuf::starts_with 组件级 |

## 二、P1

### msgloop 组

| ID | 判定 | 证据 |
|---|---|---|
| M1 | ✅ | conflict.rs:14-21 匹配 `"patch|write|edit_file|edit|edit_diff|delete"`；实际 key 是 `apply_patch`(apply_patch.rs:194)/`copy_range`(copy_range.rs:377)，均不命中→同文件双 patch 不进串行组 |
| M4 | ✅ | admit.rs:328 先置 ToolsRunning；重复 id 分支 :339-353 清 step 后返回 `Handled`；loop_core.rs:1762 `Outcome::Handled => {}` 无 phase 复位→永久卡 ToolsRunning、timeline 不 seal |
| C1 | ✅ | admit.rs:472 并行批 `while` 顶部无 cancel 检查（仅 :514 收割后观察一次，取消仍继续下一批 spawn+发 Running）；串行路径 :561 有 `break` |
| C2 | ✅ 且机制更精确 | 毒化源不止 loop_core.rs:1481：`registry.rs:448-450` 在**非 actor 的 daemon 线程**对 interrupt 类命令 `set_cancel(true)` 写**进程全局** CANCEL(lib.rs:191)。而全部清零点(engine_input:83/:250、loop_core:325/:389)都在 actor 线程执行，按 lib.rs:255-260 只写线程本地 ACTOR_CANCEL——全局一旦置位无人复位。`ActorToolScope::capture/install`(runtime.rs:110-128)不携带 cancel/session→工具执行线程读全局。计划"已实证"的复现与此完全吻合；PR-6 需把 registry.rs 写点纳入契约 |
| G4 | ✅ | start_compact(:1076) 仅查重入不查 phase/suspension；check_pending_compact(:924) apply_result(:951) 无守卫且主循环每秒轮询(:756)+每 lap 后(:1754)；迟到 grant 经 engine_turn.rs:170-175 仅 call-id 校验即执行出孤儿结果（与 G5 组合成静默失效） |

### runtime 组

| ID | 判定 | 证据 |
|---|---|---|
| R1 | ✅ | hub.rs:1184 `sequencer.next()` 在 :1200 channels 锁外取号，两并发发布者可乱序提交 |
| R3 | ✅ | hub.rs:449-457 load_seed 失败仅 log::warn 后 return（fail-open）：seed 不入 channels→publish 以全新 state 从 seq=1 发布，重启重放永久乱序重复 |
| R4 | ✅ | hub.rs:1365-1374 per-seed replay journal+replaceable 合并不排序；频道级 :1424 有 sort_by_key 对照 |
| R5 | ⚠️ 部分确认·建议降级 | journal.rs:82-95 只删 entries 不删 seen_event_ids 属实(:81 注释自认保留)；但 append 侧容量淘汰(:60-67)持续剪 id，\|seen\|<2×capacity(8192×2≈1.6 万 id，约 MB 级)**有界非无界**。"内存无界增长"不成立，建议降为 🟢 并改述为"压缩条目的 id 占用至多 2×容量窗口" |
| R6 | ✅ | projection.rs:183-185 `cancelled=true` 后 TurnStarted(:144)/TurnCompleted(:148) 均不清除；:122-124 `last_failure` 写入后无任何清除路径 |

### message 组

| ID | 判定 | 证据 |
|---|---|---|
| G1 | ✅ 主半 / ⚠️ 次半潜伏 | context_flow.rs:315-319 `drain_turn_boundary` 对 ingest 回执 `let _ =` 忽略并无条件回执 command_id=committed→失败/dedup 注入假成功丢失。次半 :361 last_keys 先于 store 接受写入属实，但当前 store 推入路径不可失败，属潜伏型（修复时顺手调整顺序即可） |
| G2 | ✅ 全链路 | engine_compact.rs:158 kept_user_count 扁平数 user 消息（subagent 报告 role=user 计入）当 keep 用→engine_turn.rs:699 apply_compact(kept)；keep≥turns.len() 时 store.rs:1243 早退零压缩，但 :707-719 仍发 CompactFinished{Completed} 且 return true(:719)→agent.rs:260-265 true 不设阻断→每 lap 重跑摘要 LLM=livelock。过度折叠反向场景由同一计数错误解释 |
| G3 | ✅ | store.rs:659-680 flat_in_write_order 按 msg_id 写序合并 turns+trailing；挂起期间 deferred 注入经 handle_system_input(engine_input.rs:282) 即刻 drain 落 trailing（msg_id 介于 tool_use 与迟到 result 之间）→序列化顺序 tool_use<injection<tool_result，Anthropic/OpenAI 400 成立 |
| G5 | ✅ | store.rs:597-608 孤儿 tool_result 仅 log::error+return false，无领域事件无 timeline 标注 |

### workspace 组

| ID | 判定 | 证据 |
|---|---|---|
| W1 | ✅ | todo.rs:347-349 `start_n..=end_n` 直接展开无上限；MAX_CREATE_ITEMS=20(:389,:430) 仅约束创建侧 |
| W2 | ✅ | save_todo/load_todo(:98-105) 直调 read_store/write_store，二者均不取 TODO_LOCK(:21)（工具路径 :183/:510/:573 取锁）；write_store :272 固定 tmp 名 `json.tmp`；真实并发写方：engine_input.rs:40-74 goal 模式 load→mutate→save |
| W3 | ✅ | permission.rs:188-192 仅 `tool_name=="exec"` 提取 cwd；bash/pwsh 同 schema 不命中；apply_patch 目标在 patch 文本未解析→Level-3 自动批准资源绑定失明 |
| W4 | ✅ | process_registry.rs:263-288 kill：child 已 reap(None) 时跳过杀戮仍置 Killed 并返回 true，Exited(code) 被覆盖丢失；NOT_FOUND 返回 false 与成功同布尔通道 |
| W5 | ✅ | write_to :150-154 单次 `w.write()` 非 write_all；满 tty 缓冲部分写/阻塞风险属实 |
| W6 | ✅ | register(:71-91) 后全文件无 entries.remove；next_id 无 checked_add |
| W7 | ✅ | permission.rs:205-211 resolve_target_path 相对路径绑 `std::env::current_dir()`；执行侧(manager.rs:378 等)绑 workspace root→授权/审计指向错位 |
| C3 | ✅ 结构性 | lib.rs:264-271 is_cancel 在 actor 线程只读本线程 ACTOR_CANCEL；manager.rs:323-337 cancel_tool 只作用于调用者线程 with_manager 解析到的实例——子代理 actor 的独立 ACTOR_TOOL_MANAGER(runtime.rs:165 install_actor_tool_manager) 收不到其它线程的取消 |
| H6(+W-low①) | ✅ | exec.rs:578-596 Unix 未设 process_group(0)（仅 Windows CREATE_NO_WINDOW）；registry kill :276-280 Unix 单 pid SIGKILL；exec.rs:712 注释"取消=杀进程树"与实现矛盾；孙进程持管道写端→reader 永不 EOF。W-low① exec.rs:764-770 属实（2s recv 超时伪造 [WARN] 占位替代真实捕获） |

## 三、给后续 PR 的增量发现（本轮复核新产出）

1. **C2 的毒化写点比计划多一处**：除 loop_core.rs:1481 外，registry.rs:448-450（daemon 侧 interrupt 预置）在非 actor 线程写全局 flag，是跨会话毒化的实际来源之一。PR-6（cancel-contract）必须同时覆盖两个写点。
2. **R5 建议降级** 🟠→🟢：有界（<2×8192 id），改述为"compact_round_deltas 不收缩 seen_event_ids，峰值占用翻倍窗口"。
3. **G1 次半（last_keys 时序）当前无可达失败路径**，随 PR-8 顺手调序即可，不必单列测试。
4. **H4/H5 修复联动提醒**：两者都在 truncate_before_turn 的 seq→index 映射上，PR-5 压缩簇与 PR-1(H5-B 高水位推算) 必须共用同一套索引推导，避免修一个坏一个。
5. 计划中少量行号偏差（H4 引用 1217/1230/1179、C1 串行检查实为 ：561 非 :552、G3 本体在 ：659-680 非 ：664）不影响机制成立，无需改计划正文。

## 四、Step 0 关卡结论

**放行**。42 项中抽全覆盖的 29 项 P0/P1 无一否决，P0 六项全部坐实，对应 PR-1~PR-9 可按计划批次启动；唯一修正为 R5 降级与 C2 写点补充。

---

# 附录：对抗性二验（2026-08-24 第二轮）

> 方法：对首轮判定逐条找反例——重走调用链、独立重算数值推演、检查测试盲区。
> 结果：**27 项维持原判，2 项实质性修正（G3 触发链改写、C2 毒化面扩大），0 项翻案否决**。

## 二验修正 1（重要）：C2 毒化面比首轮报告更大

`ringing_command_is_interrupt`（loop_core.rs:70-81）不止包含 ConversationCancel(:78)，还包含
**SessionCreate / SessionResume / SessionShutdown**(:74-76)。即：**前端切换会话（SessionResume）
本身就会在 daemon 非 actor 线程把全局 CANCEL 置 true**（经 registry.rs:448-450）。
PLAN 复现中"切到 B"这一步可能已是触发点之一，不必等到点 Stop。PR-6 的 cancel-contract 必须
把 interrupt 分类函数本身纳入治理（要么缩小分类、要么写点下沉到 actor、要么读侧统一本地化）。

## 二验修正 2：G3 触发链改写（缺陷本体维持确认）

首轮报告写的触发载体"挂起期间 deferred 注入经 handle_system_input 即刻 drain 落盘"**有误**：
- 挂起时 inject() 走 absorb_injection(loop_core.rs:486) 入 **injection_bus**（非 deferred_ringing）；
- bus 的唯一 drain 点在 apply_outcome 的 ContinueTurn 分支(:1729)，位于下一 lap 开工前——
  此时迟到 grant 已先执行完工具并 push_tool_result（msg_id 更小），随后才落盘注入，
  序列化顺序为 tool_use < result < injection，**不产生 400**。

缺陷本体（flat_in_write_order store.rs:659-680 无 has_pending_tools 守卫）仍成立，真实触发链改为：
1. **H1 组合链**：挂起 A → 新用户消息 B 开新 turn（B.user msg_id > A.tool_use）→ A 迟到 grant
   执行（result msg_id 最大）→ 序列化 A.tool_use < B.user < A.result → 400；
2. **Cancel 悬空链**：ConversationCancel 的 reset_all_engines(:364) **不做**
   remove_last_step_if_incomplete（仅 prepare_session_switch:384 做）→ 悬空 tool_use 长期存续，
   其后任何写入（含 idle 注入的 handle_system_input 直接 drain）都插在 tool_use 之后且永远等不到
   配对 result → 每次 build_context 400。

修复方向不变（最后 step 有未满足 tool_use 时拒绝/延后插入），但实现应与 H1、cancel 契约同簇落地，
否则只堵住两条链之一。

## 二验补强证据（维持原判）

| 项 | 新增佐证 |
|---|---|
| M1 | conflict.rs 内置测试集（:134-166）只用 write/edit_file/todo 名字——apply_patch 从未进过测试视野，解释漏匹配逃过单测 |
| H4 | compacted_turn_count(store.rs:1312-1322) 确认按 `[Compacted N` 前缀解析头部计数，漂移链最后一环闭合；另验证单次压缩 undo 正确（20→compact(5)→undo t20 idx=5<6 成功），仅重复压缩后出错——与 PLAN 干净区声明一致 |
| H5 | 独立重算：M 真实 turn+phantom seq M+1 后，undo t{M+2} 得 idx=M+1≥len(M+1)=false；undo 幽灵 t{M+1} 得 idx=M 反截掉最新真实 turn。两分支均维持 |
| M4 | phase=Idle 全仓仅 5 个赋值点（loop_core :323/:1675/:1689/:1717/:1791），均不在 Handled 分支。细化表述：卡 ToolsRunning 为真实，但非进程级死锁——新用户输入可恢复流转；该 turn timeline 永不 seal、恢复前仪表盘持续误报 |
| W7 | 双侧基准差异实锤：授权侧 permission.rs:205-211 join `std::env::current_dir()`；执行侧 apply_patch.rs:18-19 与 lib.rs:296-315 join `current_workspace()` |
| C2 | send_ringing 调用方为 daemon HTTP handler 线程（无 set_actor_context，actor 上下文仅在 actor.rs:144 安装）→ 写全局路径确认 |

## 二验结论

Step 0 关卡结论不变：**放行**。两处修正已并入正文理解——PR-6 范围应含 interrupt 分类治理，
G3 应与 H1/cancel 簇同 PR。其余 27 项首轮判定经对抗性复核无一推翻。

---

# 附录：修复进度日志

## 批次 A（2026-08-24，PR-1 前置小修包）

| 项 | 状态 | 落点 |
|---|---|---|
| M4 | ✅ 已修 | admit.rs duplicate-id 分支：OperationFailed + seal_timeline_terminal_round(Cancelled) + Outcome::TurnAborted（终态事件+phase=Idle） |
| C1 | ✅ 已修 | admit.rs 并行批 while 顶部 cancel.is_set() break |
| W1 | ✅ 已修+单测 | todo.rs expand_todo_ids 封顶 MAX_RANGE_EXPAND=1000；todo_range_capped 测试通过 |
| L-msgloop③ | ⚠️ **翻案降级** | 原 else 分支(loop_core.rs:1189)已发 CompactFinished{Skipped}，PLAN/首轮“零反馈”断言系误报（未读全函数）。仅补命令级 receipt（code=compact_noop） |
| L-msgloop① | ⛔ 暂缓 | YieldToUser 后置 Idle 会改变 inject() 分支语义——挂起期间 idle 注入会开新 turn，破坏 ask/权限 UX；需引入独立 Suspended phase 设计后再动 |

回归：qaqh-msgloop --lib 49 通过；qaqh-workspace todo 15 通过；clippy 零新增警告；cargo check --workspace 干净。

### 对后续批次的修正指令
1. PR-5 的 L-msgloop③ 条目改为“补命令级回执”（已完成，可从 PR 划掉）；
2. PR-6 范围确认含 interrupt 分类治理（SessionCreate/Resume/Shutdown 也是毒化写点）；
3. G3 与 H1/cancel 簇同 PR（两条真实触发链见二验附录）；
4. H5-B 实施前必须先回答“重启后 next_turn_seq 如何恢复、phantom 是否跨重启存续”，否则高水位推算会引入新洞。

---

# 长程修复任务收官报告（2026-08-24）

## 最终验证

- `cargo test --workspace --lib`：**706 passed / 0 failed**
- `cargo clippy --workspace --all-targets`：**0 error**
- 关键集成：ask_user_lifecycle 12 ✓ / permission_lifecycle 8 ✓ / session_lifecycle 4 ✓ / inprocess_loop ✓ / session_inprocess 2 ✓ / subagent_inprocess ✓

## 逐批交付

| 批 | 项 | 状态 |
|---|---|---|
| 前置 | M4 C1 W1 | ✅ |
| B1 安全覆盖 | M13 M1 W3 W7 | ✅（新增 patch_target_paths 共享助手；组件级路径检查+..归一化） |
| B2 hub 序号 | R1 R3 R4 R5 R6 | ✅（取号入临界区/fail-closed/合并排序/id 剪枝/projection 生命周期） |
| B3 message | G1 G3 G5 | ✅（假 committed 回执/trailing 延迟队列+安全点回灌/孤儿结果领域事件单点上报） |
| B4 身份守卫 | H1 H2 | ✅（abort_suspended+drop_stale_suspension，四 resume 入口统一会话守卫；SessionCreate 无条件走切换守卫） |
| B5 压缩簇 | H4 G2 G4 | ✅（头部计数 total_real；count_live_turns_from 真实 turn 数；零压缩如实 Cancelled+阻断 livelock；start/consume 双守卫） |
| B6 取消语义 | C2 C3 H6 W-low① + 文档 | ✅（clear_cancel 双层归零+ToolInvoke 双清；H6 process_group(0)+killpg 新增 unix libc；收集兜底注册表快照；docs/cancel-contract.md。C3 按 id 跨 manager 取消登记为已知边界） |
| B7 H5-B | 预研完成，**实施推迟** | ⚠️ next_turn_seq 重启恢复=计数水位(max(turns,meta,timeline)+1)，与 timeline floor 纠缠；完整方案按 PLAN 高危评级推迟至独立 PR+用例矩阵 |
| B8 workspace 杂项 | W2 W4 W5 W6 W-low② | ✅（锁下沉公共入口防死锁+唯一 tmp；os_pid 快照使 kill-after-exit 仍可清树且 last_exit_code 经 get_info 保留——与 backgrounded 清理测试语义调和；write_all 循环；惰性驱逐+checked_add；字符口径裁剪） |
| B9 代际收尾 | H3 R2 H3c | ✅ 单批原子落地：hub live_workers 门控(force=false 且 worker 存活跳过)+条件删除；registry 两处 seal 前移到 spawn 之前 |
| B10 P2 清扫 | L-runtime L-session M-low③ M-low④ L-msgloop② M-doc/M-low② | ✅（Unix 免 remove_file；torn tail 仅末行容错；死 fallback 分支删除；replay 判别持久化 name 字段优先；plan/todo 死路径标注+co-pending 陷阱注释；**发现并修正 PLAN 自相矛盾**——A,B,A 实际被 store 层全历史 dedup 吞掉而非"passes by design"，两层去重统一收窄为仅相邻并加 aba_pattern_passes_by_design 锁定测试） |

## 翻案与推迟清单（诚实记录）

1. **L-msgloop③**：原断言"if-let 无 else 零反馈"系误报（else 已发 CompactFinished{Skipped}）；实际残余仅为缺命令级 receipt，已补 compact_noop。
2. **M-doc vs M-low② 自相矛盾**：PLAN 同时声称 A,B,A "passes by design" 又把同一现象列为 bug。实测证实为 bug（store 全历史文本 dedup 吞掉间隔重现报告），已按相邻化统一修复并以测试锁定。
3. **C2 处方不足**：PLAN 的"ToolInvoke 前 clear 两处 token"在 actor 模型下写不到全局 flag；本轮以 clear_cancel() 双层归零 + 全清零点替换解决，并把 registry interrupt 写点治理写入 cancel-contract.md。
4. **H5-B 推迟**、**L-msgloop① 推迟**（需 Suspended phase 设计）、**C3 按 id 跨 manager 取消**登记为已知边界（全局取消路径已可用）。

## 后续建议（下一轮）

- H5-B 独立 PR：先落"allocator seq 持久化"（meta 增加 last_turn_seq 字段）再实现高水位推算；
- L-msgloop① 引入 `LoopPhase::Suspended`；
- 子代理 ToolCancel 跨 actor 注册表路由（闭合 C3 按 id 场景）；
- PLAN §6 的 5 个 frontend-sync issue 待仓库 issue 化后回填链接。
