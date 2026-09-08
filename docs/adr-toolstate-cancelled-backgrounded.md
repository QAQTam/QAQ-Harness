# ADR: TimelineToolState 补 cancelled / backgrounded 两态

- 状态：已合入（后端侧）；前端仓库待按本 ADR 开 PR
- 日期：2026-09-09
- 影响契约面：`bindings/qaqh/TimelineToolState.ts`（新增两个 union 成员，
  **加法变更**）；`bindings/qaqh/ToolResult.ts` / `TimelineTool.ts` /
  `ControlCommand.ts` / `ActivityState.ts` / `AgentLifecycleState.ts`
  （仅注释与既有漂移同步，成员无增减）

## 背景

工具侧 `ToolStatus` 是五态（`Ok / Error / Partial / Cancelled / Backgrounded`），
而前端可见的 `TimelineToolState` 只有四态。投影层长期把 `Cancelled` 折叠进
`failed`、`Backgrounded` 折叠进 `succeeded`，前端无法区分"这个工具被用户取消了"
和"这个工具真的失败了"，也无法表达"转入后台继续运行"。

更深层的问题：状态信息在主执行路径上**中途丢失**。工具线程返回的
`ToolResult` 携带真实五态，但归档时经 `push_tool_result_*` 只保留
`success: bool`（message 层用 ok/error 重建），backfill 阶段再从布尔
伪造 `ToolFinished` 载荷。除 UI 主动调用路径外，五态信息从未到达过前端。

## 决策

1. `TimelineToolState` 新增 `cancelled`、`backgrounded` 两个变体（serde
   snake_case），并提供 `From<ToolStatus>` 固定映射：
   `Ok→succeeded`、`Error|Partial→failed`、`Cancelled→cancelled`、
   `Backgrounded→backgrounded`。
2. 归档保真：`MessageStore::push_tool_result_canonical` 直接存归档
   `ToolResult`（含五态 status 与展示面 diff），不再经布尔重建；
   主路径四个执行点（并行批/串行/延迟批/单发）全部切换。
3. `last_step_tool_results` 返回 `StepToolResult { tool_call_id,
   tool_name, result: ToolResult }`，backfill 用真实 status 投影
   timeline 终态，且 `ToolFinished` 载荷直接携带归档的 canonical
   `ToolResult`（不再伪造 ok/error）。
4. `ToolResultDef`（timeline journal/重建面）新增
   `status: Option<ToolStatus>`（serde default 兼容旧 journal）；
   重建时优先用五态，缺失按 success 布尔回退。
5. 顺带修复 v3 拆分遗漏：UI 路径 dashboard 即时刷新仍匹配旧工具名
   `"todo"`，改为 `todo_write | todo_update | todo_list`。

## 兼容性

- 契约级别：**加法**。TS union 新增成员对非穷举消费方无感；前端
  （尚未发版）应按六态实现，对未知 state 值兜底按终态处理。
- serde 线上格式：`ToolResult` 字段集合与可选性不变（ts-rs 包含私有
  字段，再生后成员一致，仅声明顺序与注释变化）。
- 旧 timeline journal：`ToolResultDef.status` 缺省 `None`，重建走
  success 布尔回退，行为与从前一致。

## 当前生产语义边界（诚实声明）

本次打通的是**投影链路**。`cancelled` / `backgrounded` 的实际产生来源
目前有限：

- `cancelled`：daemon 重启收尾（orphan seal）发出的
  `ToolFinished(Cancelled)`；会话内用户取消仍走"结果丢弃"路径，
  后续如需把取消结果带回模型上下文，另立变更。
- `backgrounded`：暂无生产者（工具侧尚未有转后台实现），枚举值
  先行，供 exec 后台化落地时使用。

## 跟进

- 前端仓库按本 ADR 开 PR：六态渲染（cancelled 用中性色而非错误色，
  backgrounded 可提供"跟进查询"入口）、穷举 switch 放开或加兜底分支。
- 双侧 tag 同步发版后关闭本 ADR。
