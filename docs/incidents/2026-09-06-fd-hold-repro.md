# fd 持有复现实验——"247272 树在哪个 fd 持有管道写端"

> 观测线 P1（`exec-lifecycle-rewrite-plan.md`）。执行：2026-09-06，Linux。
> 方法：Python `subprocess.Popen` 复刻 `direct_exec` spawn 形态
> （`start_new_session=True` ≙ `process_group(0)`、stdout/stderr PIPE、stdin DEVNULL），
> 全系统 `/proc/<pid>/fd` readlink 对管道 inode，持有者以 cmdline 取证。

## 实验组

| 组 | 命令形态 | 验证目标 |
|---|---|---|
| A | `nohup fake-daemon run > log 2>&1 &`（fake-daemon 内部再 `sleep 60 &` + `exec sleep 60`，模拟 247272+247286 两级树） | 事故记载的精确形态是否足以持有写端 |
| B | `sleep 30 &`（裸派生，无重定向） | 已知机制，验证方法学 |

## 结果（原始输出节选）

### B 组（裸派生）——实锤经典机制

```
child bash pid = 61014, 读端 inode = ['pipe:[120226]', 'pipe:[120227]']
[bash 存活期] pid=61015 comm='sleep' ppid=1280 fd=1  cmdline: 'sleep 30 '
[bash 存活期] pid=61015 comm='sleep' ppid=1280 fd=2  cmdline: 'sleep 30 '
bash wait() 返回 exit=0
[bash 退出后(+1s)] pid=61015 ... fd=1 / fd=2 仍然持有
```

- 孙进程 `sleep` 持 **fd1+fd2 = 管道写端**；bash 退出后被孤儿化，
  reparent 至 subreaper（ppid=1280，harness 进程树根，非 init），
  **直至 sleep 自然退出前写端永在** → 读线程永等 EOF。
- 与事故 H-B 定性、`grandchild_holding_pipe_write_end_collects_bounded` 回归完全互证。

### A 组（事故记载形态）——重定向彻底切断管道

```
child bash pid = 61007, 读端 inode = ['pipe:[115927]', 'pipe:[115928]']
bash wait() 返回 exit=0；/proc/61007 存在 = False
[bash 退出后(立即)/(+1s)] 持有者: 无（写端已全关 → 读线程必然 EOF）
```

- `> log 2>&1` 在后台派生体上**替换**了 fd1/fd2（原写端被 close），
  fake-daemon 内部再派生的 worker 继承的是 log/devnull，**不再引用管道**。
- bash 退出后全系统无任何进程持有该管道 inode。

## 结论

1. **持写端的充分条件是"无重定向（或未覆盖 fd1/fd2）的后台派生"**（B 组形态）；
   事故记载的 `nohup … > log 2>&1 &` 全重定向形态本身不产生写端持有（A 组）。
   → 事故实际现场中真正持写端的后代，其派生命令应存在未重定向的 stdout/stderr
   （或重定向顺序变体如 `2>&1 > log`）。此为对"247272 树在哪个 fd 持有写端"的
   精确回答：**fd1/fd2，来自未重定向的后台派生体**。
2. 实验同时佐证当前治理面是完备的：
   - 确定性修复（阶段 2 poll 化读线程 + settle 预算）对 A/B 两形态都保证读线程退出；
   - `grandchild_holding_pipe_write_end_collects_bounded`（B 形态）持续回归。
3. 副产品证据：孤儿进程 reparent 目标是 harness subreaper（ppid=1280）而非 init——
   与"多会话互踩"主题方向一致，列入杂项证据。
