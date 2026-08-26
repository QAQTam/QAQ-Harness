# 取消传播契约（cancel-contract）

> 状态：**生效中**（PR-6 产物，内部契约非冻结面）
> 关联修复：C1 / C2 / C3 / H6 / W-low①
> 最后更新：2026-08-24

## 三层取消信号

| 层 | 载体 | 作用域 | 典型写者 |
|---|---|---|---|
| L1 Loop token | `Loop.cancel: CancelToken`（msgloop） | 单个会话 worker 的 gate/tool 循环轮询 | registry interrupt 预置(:448)、ConversationCancel handler(:1480)、panic 恢复 |
| L2 workspace flag | `qaqh_workspace::CANCEL`(全局 AtomicBool) + `ACTOR_CANCEL`(actor 线程本地) | 工具 admission/执行轮询（execution.rs:63、exec poll :709） | 同 L1 写点经 `set_cancel(true)`；`manager.cancel_tool(None)` |
| L3 进程树 | Unix `killpg(pgid, SIGKILL)`（spawn 已 `process_group(0)`）/ Windows `taskkill /T /F` | 单次 exec 的子进程及全部后代 | ProcessRegistry::kill |

## 不变量（修复后成立）

1. **双层同步归零**：任何清零路径必须调用 `qaqh_workspace::clear_cancel()`
   （同时清 ACTOR_CANCEL 与全局 CANCEL）。禁止直接调 `set_cancel(false)`
   ——在 actor 线程上它只写本地，会让全局残留毒化其它会话（C2 根因）。
2. **清零时机**：
   - 新用户输入 / system 注入入口（engine_input）；
   - `prepare_session_switch`（含 SessionCreate 两分支，H2 后无条件走守卫）;
   - panic 恢复；
   - UI `ToolInvoke` 派发前（C2 处方：Stop 后快捷工具可用）。
3. **置位时机**：interrupt 类命令（SessionCreate/Resume/Shutdown/ConversationCancel，
   见 `ringing_command_is_interrupt`）在 daemon 侧入队前预置；worker 内
   ConversationCancel 再次确认。置位允许跨线程写全局——因为不变量 1 保证
   下一个用户动作必然归零，毒化窗口从"永久"收敛为"单次命令周期"。
4. **L3 组杀**：Unix spawn 设独立进程组；kill 一律 `killpg`。禁止退回单 pid
   SIGKILL（孙进程持管道写端 → reader 永不 EOF，W-low① 场景根源）。
5. **收集兜底**：exec 收集超时回退注册表已捕获输出，不伪造占位文本。

## 已知边界（未完全闭合，登记跟踪）

- **C3 按 id 取消**：`ToolCancel(Some(id))` 只作用于调用线程解析到的
  ToolManager；子代理 actor 持独立 manager，其 inflight 任务收不到按 id
  取消。缓解：全局取消（None）路径经 exec poll 的 `is_cancel()` 全局读已可达。
  彻底修需跨 actor 注册表路由（随子代理注册表改造）。
- **L1 与 L2 的生命周期差异**：loop token 随 worker 存活；workspace 全局
  flag 是进程级。契约要求"写后必清"，但不提供 TTL。

## 违反契约的代码评审红线

- 新增 `set_cancel(false)` 字面调用（必须用 `clear_cancel`）。
- 在持 `channels` 锁或工具执行临界区内调用 clear（可能复活刚取消的操作）。
- Unix 分支新增 `child.kill()` 单 pid 杀（必须 killpg）。
