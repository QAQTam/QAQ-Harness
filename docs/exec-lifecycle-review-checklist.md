# exec / 工具生命周期代码评审 Checklist

> 来源：`exec-lifecycle-rewrite-plan.md` 阶段 2.1 契约（2026-09-06 随 registry-native 重写落地）。
> 适用范围：`qaqh-workspace` 的 exec/process 生命周期代码、`qaqh-runtime` 的工具排空（drain）代码，
> 以及一切"spawn 子进程并等待结果"的新路径。
> 背景：2026-09-02 session 692d1605 冻结 67 分钟事故——actor 被无界等待钉死在封口发射之前。

## 契约（违反即打回）

### 1. seal 以 registry 快照为权威——任何路径不得等待管道 EOF / 流关闭

- [ ] 结果数据源必须是 `ProcessRegistry::captured_full(id)`（完整捕获，5MB 字节上限），
      而不是读线程的返回值、信道汇总、或任何"流关闭才有"的产物。
- [ ] seal 路径允许的唯一等待：对**读线程退出信号**的有界 join
      （`SEAL_JOIN_BUDGET`，当前 500ms）。该信号在 EOF、settle 到期、读错误时都会发出，
      **不是** EOF 等待；join 超时只降级 `truncated` 标记，不影响返回。
- [ ] 新增任何"等待读线程 / 等待信道关闭 / 等待 EOF"的代码路径一律打回：
      先问"孙进程持有管道写端时它还能返回吗？"

### 2. 读线程生命周期必须有界

- [ ] 读线程循环必须是 poll 化的（unix：`O_NONBLOCK`；Windows：`PeekNamedPipe`），
      退出条件完备：EOF / 读错误（非 WouldBlock/Interrupted）/ 字节上限 /
      **settle 到期**（子进程终态后 `READER_SETTLE_BUDGET` 内强制收尾）。
- [ ] 读线程退出时必然 drop 全部 sender（progress 信道与 done 信道）——
      这是 drain 侧 `Disconnected` 快路径的前提。
- [ ] 禁止在 byte-cap 触发后进入"排空到 EOF"的无限 copy（旧实现此处同样可被孙进程卡死）；
      超限数据就地丢弃，受同一个 settle 预算约束。
- [ ] backgrounded（超时移交）路径的读线程同样受上述约束：
      子进程仍在运行时持续排空；子进程退出后 settle 收尾。

### 3. 取消 / 超时 / 移交语义

- [ ] 取消 = `ProcessRegistry::kill`：Unix `killpg(SIGKILL)` 整组（spawn 侧必须
      `process_group(0)`）；Windows `taskkill /T /F` 整树。禁止只杀直接子进程。
- [ ] 超时 = 移交后台（`status=backgrounded`，`process_id` 必须随 result 携带），
      **不得** kill；后续由 process 工具 check/wait/kill 接管。
- [ ] 阻塞型等待循环（`wait_for`、run-to-completion poll）每轮必须检查：
      per-call cancel 旗标 **和** ambient `crate::is_cancel()`；命中即有界返回。
- [ ] 任何新引入的等待必须有显式上界（常量命名 + 注释说明预算构成），
      并附"无界等待"反例的回归测试。

### 4. 工具线程排空（engine_tool / admit）

- [ ] progress 排空必须走 `drain_bounded`：`tool_done`（JoinHandle 结束）为真后
      最终 `try_recv` 排空即收尾，**绝不等待 Disconnected** 之外的无界条件；
      `Disconnected` 仅作快速路径。
- [ ] 新增排空调用点必须传入正确的 `tool_done` 闭包（覆盖全部并发工具线程）。

### 5. 输出语义（回归基准）

- [ ] stderr→stdout 组合顺序、`strip_ansi`、token 截断（head 70% + tail 30%）、
      `truncated` 口径（字节上限耗尽 ∨ 读线程未以 EOF 收尾）与既有测试一致。
- [ ] Windows 非 UTF-8 控制台输出走 OEM 码页解码（`decode_windows_oem`），
      分割中的 DBCS 序列等待后续字节而非产替换符。

### 6. 测试矩阵（改动以上任何一条必须全绿）

- [ ] `grandchild_holding_pipe_write_end_collects_bounded`（双平台，事故直接回归）
- [ ] `reader_threads_terminate_after_grandchild_settle_even_without_eof`（1.3 驻留治愈）
- [ ] `seal_uses_full_registry_capture_not_tail_when_grandchild_holds_pipe`（快照权威 + 完整性）
- [ ] `per_call_cancel_stops_only_the_running_command`（双平台）
- [ ] `timeout_transfers_process_to_background_registry`、
      `background_after_secs_handoff_before_timeout`（Windows 侧必跑）
- [ ] `wait_for_returns_promptly_on_per_call_cancel`（飞行中取消）
- [ ] `drain_bounded_*` 三测（engine_tool）
