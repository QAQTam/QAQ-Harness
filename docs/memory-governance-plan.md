# 内存治理方案（memory-governance-plan）

> 2026-09-02。背景：daemon RSS 随会话历史单调增长（截图实测 302.6 MiB）。
> 根因链已核实（见对话记录）：① worker 常驻全量 MessageStore（设计）；
> ② compact_skip 只裁 LLM 视图不裁内存；③ 无 idle 卸载 / 冷分层；
> ④ stop 后 RSS 不回落 = glibc arena 滞留 + IMAGE_REGISTRY 真泄漏 + hub per-seed 残留。
> 前置已就绪：L1 lap 边界 drain、L2 WAL（enqueue 级 + 幂等重放）、L3 工具 outbox——
> 本方案的每一条安全前提都建立在其上。

## 路线总览（决策顺序 D → A → B；E 已批准先做）

| 项 | 内容 | 状态 |
|---|---|---|
| **E. idle 卸载** | 空闲 worker 整体 seal + 摘除，内存归零；下次输入自动恢复 | **本次实施，待检查** |
| D. stop 清理钩子 | close_session 补 `read_image::reset_images(seed)` + hub `forget_seed(seed)` + `malloc_trim(0)` | 待批准 |
| A. 图片外置 | base64 → 磁盘文件引用，请求组装时按需读入 | 待批准 |
| B. 冷前缀驱逐 | 驱逐 compact_skip 前缀 turns（内存上限 ≈ 可见窗口，恒定） | 待批准 |
| C. 仅活动 round 驻留 | 变体 2：连可见窗口也驱逐，逐 turn 重物化（Z5 字节等价是主风险） | 暂缓（与 E 收益重叠） |

配置项统一为 `config.toml` 的 `PersistentConfig.session_idle_unload_secs: Option<u64>`（None/0 = 禁用）。
**不走 UI ConfigDto 链**：这是 daemon 级行为策略而非模型配置；将来要 WinUI 暴露时再补 dto 映射。

---

## E. idle 卸载（本次实施）

### 语义

- **判定**：worker 满足全部条件才可卸载——
  1. 空闲时长 `now - last_activity ≥ idle_unload_secs`；
  2. `!busy`（无命令 dispatch 进行中，长 turn 的工具执行期间绝不卸载）；
  3. `!suspend_pending`（无挂起的 ask / 权限 / plan 评审——卸载会让 pending 交互被
     bootstrap 孤儿收尾 seal 掉，破坏 UX；用户走开导致 ask 长期挂起时宁可不卸载）。
- **动作**：复用 `registry.close(seed)` 现有优雅路径（SessionShutdown → join → bundle drop
  → 退出前 final flush + drain）。持久会话磁盘不动；下次输入走
  `get_or_spawn → init_session → load_for_resume`（WAL 重放 + outbox 对账 + from_messages）
  自动恢复，对话无损。
- **范围**：仅 `AgentKind::Session`；子代理 worker 短命，不纳入（避免与 collect 时序耦合）。
- **卸载副作用**：向 hub 发 `SessionStateChanged::Closed`（与手动 close_session 一致，
  UI 可感知）；日志 `idle unload seed=X idle=Ys`。

### 数据流

```
Loop::safe_dispatch ─┬─ 进入: liveness.busy = true
                     └─ 退出(含 panic 恢复): busy=false, touch(), 
                        suspend_pending = turn.is_suspended()

daemon 周期任务(60s, server.rs run_with) 
  → 读 PersistentConfig.session_idle_unload_secs（热生效，每次 tick 读）
  → >0 时 service.unload_idle_sessions(secs)
    → registry.unload_idle_sessions: 过滤可卸载实例 → 逐个 close（spawn_blocking 防阻塞 tokio）
```

`WorkerLiveness`（新，`agent/liveness.rs`）：`Arc` 由 registry spawn 时创建，
一端存 `AgentInstance`（判定方），一端进 `Loop`（生产方）。三个原子字段：
`last_activity: AtomicU64`（epoch s）、`busy: AtomicBool`、`suspend_pending: AtomicBool`。

### 竞态与安全性

- **卸载 vs 新命令并发**：命令在 close 后到达 → `send` 失败 → 现有 fallback
  `get_or_spawn` 重拉 worker（registry.rs:470-484 的死 worker 语义），经 load_for_resume
  恢复后执行。命令在 close 中到达 → 旧 worker 执行完才 join，亦安全。
- **F4 重生不冲突**：close 把实例从 map 摘除；F4 只重生 map 内的死实例。
- **长 turn**：busy 守卫保证不选中；即便误判，close 的 join 也只会在 turn 完成后返回
  （但仍用 spawn_blocking 包裹，避免卡 tokio 线程）。
- **恢复一致性**：unload 前置条件含 drain 已完成（dispatch 退出即 drain），WAL 空；
  恢复路径即 resume 测试覆盖的同一路径。

### 测试计划

1. 单测：`WorkerLiveness` touch/idle/unloadable 语义。
2. 集成（tests/）：spawn 会话 → 发消息 → 强制满足空闲（liveness 拨旧）→ unload →
   再发消息 → 断言 worker 重生、两轮消息齐全（`messages.jsonl` 连续、上下文含第一轮）。
3. 回归：全 workspace test + clippy + fmt。

### 已知取舍

- ask 挂起 + 用户长期不回 → 该会话不卸载（宁欠勿损）。
- 卸载后首条消息有恢复延迟（读盘 + 重放，数百 ms 级）。
- UI 若依赖 worker 进程内状态（如 read_image 注册表）→ resume 时已重放注册，无感知。

---

## D. stop 清理钩子（待批准后实施，改动小）

`service.close_session` 增补三行级钩子：
1. `qaqh_workspace::read_image::reset_images(seed)` —— 消除 base64 图片滞留（真泄漏）；
2. `hub.forget_seed(seed)`（新）—— 清 `channels[seed]` / `live_interactions` / disk 索引；
3. `libc::malloc_trim(0)`（spawn_blocking）—— 把 arena 还给 OS，RSS 立即回落（glibc only）。
风险：低；纯回收，无语义变化。

## A. 图片外置（待批准）

`ContentBlock::Image` 持久化改为磁盘引用（`sessions/<seed>/images/<sha256>.bin` +
mime），Message 内存态只存引用；组装 LLM 请求 / UI 投影时按需读文件。
内存收益与历史图片数解耦。迁移：读旧格式兼容（data 直接内联），写新格式。
风险点：多消费方（gate 投影编号、read_image 注册表重放、dry-run 序列化）需逐一核对。

## B. 冷前缀驱逐（待批准）

turn 完成 && drain 完成 && WAL 空后，把 `turns[..=compact_skip 边界]` 从内存丢弃，
记 `evicted_max_msg_id` 水位；`flat_in_write_order` 用「水位 + 窗口 turns」拼接；
undo 语义不变（本就禁止跨压缩边界）。内存上限 ≈ 可见窗口 + 活动 turn，恒定有界。
无每请求盘读。风险：中低——需要逐一核对 `turns` 的全部消费方（投影、计数、goal）。

## C. 仅活动 round（暂缓）

在 E 落地后重估：E 已把空闲内存打到 0；C 的增量收益只剩「活跃会话的窗口内存」，
而 Z5 字节等价重建成本高。除非长会话高并发常驻场景成为瓶颈，不建议做。

## §D 实施记录（2026-09-02，E 获批后推进）

**D-1 close 路径 per-seed 清理**（service.rs `release_seed_resident_state`）
- `close_session` / `unload_idle_sessions` 在 worker join + 终态发布后调用：
  `read_image::reset_images(seed)` + `hub.forget_seed(seed)`
  （channels / live_interactions / live_workers / content_store 全清，
  磁盘索引保留 → 下次访问 lazy-load 重放，与 daemon 重启恢复同构）。
- 顺序约束：forget 必须晚于终态 publish，否则 publish 触发 lazy-load 原样重建。
- `qaqh_workspace::remove_session_cancel(seed)`：SESSION_CANCELS 整项移除，
  键控表规模与活跃会话同阶（防 ephemeral seed churn 无界增长）。

**D-2 内存归还**（service.rs `release_freed_heap_memory`）
- glibc（linux-gnu）`malloc_trim(0)`，close/unload 后调用；
  Windows/macOS/musl 编译为 no-op。Windows 侧若仍需 RSS 回落，备选：
  daemon 全局换 mimalloc + decommit（全局行为变化，待单独决策）。

**D-3 取消作用域修复**（registry.rs `signal_shutdown`）
- 旧：进程级全局 `set_cancel(true)`——对已绑定会话的工具线程不可见
  （`is_cancel` 先读 SESSION_CANCELS），却残留全局脏标记。
- 新：实例 token `cancel.set()`（turn 循环检查点解卷）+
  `set_session_cancel(seed, true)`（工具线程轮询点中止，按会话隔离）。

**D-4 阻塞 join 隔离**（axum_server.rs）
- SessionClose / Archive / Unarchive / Delete / stop / stop-if-idle 全部
  移入 `spawn_blocking`；`finish_shutdown` 保持阻塞 join，调用契约写入 doc。

测试：hub `forget_seed` 单元测试；集成测试
`close_session_cleans_per_seed_resident_state`（session_inprocess.rs）。

## §A 实施记录（2026-09-02，D 获批后推进；前置调研 docs/image-passing-research.md）

**用户决策**：三端点（chat_completions / responses / anthropic）继续并存 + 图片机制按端点
能力分层；A-2 只做 L0 磁盘外置（L1 file_id 增强另议）。

**A-0 调研结论**（详见 image-passing-research.md）：file_id 仅 Anthropic + OpenAI
Responses 支持；chat_completions 兼容生态只有 base64/data URI；base64 是唯一全端点
通用机制 → 外置化收益在 daemon 常驻内存，请求构建时读盘降格为 base64。

**A-2 L0 落地**：
- `qaqh-types::image_store`（新）：`{data_dir}/images/{sha256}.{ext}` 内容寻址存
  **base64 文本**（免 decode/re-encode；bytes_len=base64 长度，与 `[Image #N]` 占位符
  语义一致）；temp+rename 原子写，同 sha 幂等去重。
- `ContentBlock::ImageRef { sha256, mime_type, bytes_len }`（serde tag `image_ref`）：
  消息历史只持索引；旧 inline `Image` 变体读侧全兼容（旧会话零迁移）。
- 写侧 choke point（qaqh-message store.rs）：`push_image_to_last_user` /
  `push_tool_result_direct_with_attachments` 统一转换 ImageRef，落盘失败回退 inline
  Image（不丢数据）。daemon 常驻内存不再随图片数线性膨胀（诊断结论②根因消除）。
- gate 三适配器 + 两处计数 filter：ImageRef → 按需读盘 → 与原 Image 完全相同的线上
  形状（data URI / input_image / anthropic base64 source）；读盘失败丢弃该图并告警
  （不炸请求）。`[Image #N]` 占位符用 bytes_len，索引时序语义不变。
- IMAGE_REGISTRY 索引化（read_image）：条目变 `{mime, sha256}`，peek 读盘取文本；
  新增 `register_image_ref`（resume 重建零字节）；lifecycle 重建双变体（旧 inline
  Image 借重建时机外置落盘）。
- UI 展示：RoundBlock 快照不含图片字节（tool 图片走 ToolFinished 事件内联
  `result.images`，L0 未动）→ 前端零改动。

**已知后续项（A-2.1）**：`ToolResult.images`（ToolImage 内联）仍随 ToolResult 块序列化
进消息/事件，工具图存在双份存储（ImageRef + result.images）；消除需动 ToolResult
serde + UI 事件消费，单独评估。

测试：+7（image_store 3、store 转换回退 1、read_image 隔离/重建 2、serde 形状 1、
gate 线上格式 1）。全量 820 passed / 0 failed；clippy EXIT=0（新代码零警告）。

## §B 实施记录（2026-09-02，A 获批后推进）

**范围**：Variant 1——逻辑压缩前缀（`compact_skip > 0` 且无 compact checkpoint）驱逐出
常驻内存。磁盘（messages.jsonl）仍是真源，驱逐纯内存态。

**实现**（store.rs + lifecycle.rs）：
- `MessageStore::evict_compacted_prefix()`：`turns.drain(0..n)` + `compact_skip=0` +
  水位 `evicted_prefix=n`；**持久化模式物理化**——翻转 `has_compact_context=true` 并
  立即排队 `UpdateCompactContext`。此后整写走 compact checkpoint，归档不再被
  SaveFull 触碰（内存已无前缀字节，SaveFull 无法重建完整归档——这是把「驱逐」
  与「undo→SaveFull」解耦的关键设计）。
- 持久化水位：`persisted_compact_skip()` 驱逐后保持 meta.compact_skip=N（归档里
  前缀仍在，重启 `from_messages(archive, N)` 还原同一视图；checkpoint 存在后该值
  被生命周期强制无效化）。崩溃窗口双向安全（WAL 本就不记录 compact 类 op）。
- undo 映射：`truncate_before_turn` 的 seq 起点增加水位分支（窗口首 turn 全局
  seq = evicted_prefix+1）；撤回点落进被驱逐前缀 → 清空活跃视图（与物理 compact
  的清空语义一致），归档不动。
- Hook 位置修正：决策日志原写「drain 后驱逐」，代码真相是 compact_skip 仅在
  `from_messages`（resume）置位——故 hook 在 lifecycle 分配器基线固化之后
  （`ensure_next_turn_seq(authority+1)` 已用全量 restored 计数校准，驱逐不 影响
  turn-id 分配）。ephemeral store 不驱逐。
- turn_count 语义保持 `turns.len()`（驱逐后回落）：重启从全量归档重放自愈，
  meta/timeline 双下限兜底（与物理 compact 既有行为一致，未改语义）。

测试：+4（驱逐/视图字节不变/物理化持久化/水位持久化；幂等与 no-op；undo 水位
映射；跨边界清空）。全量 **824 passed / 0 failed**；clippy EXIT=0（新代码零警告）。

**记忆治理主线（E→D→A→B）至此全部落地**。C（Variant 2）按决策永久搁置（与
E+B 收益重叠、Z5 字节级风险高）。后续候选：A-2.1（ToolResult.images 双份存储）、
carried 小项（WAL >8MB checkpoint、E2E 实弹验证等）。
