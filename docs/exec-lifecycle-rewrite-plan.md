# exec 生命周期重写——核查报告与执行计划

状态：核查完成（2026-09-02 20:59），基于 `exec-lifecycle-rewrite-proposal.md` 的逐项证据复核。
结论：**报告真实性高**（核心证据链全部可复验，代码引用全部属实），一处事实错误、两处因果/数字偏差，另有两项超出报告的新发现。

---

## 一、核查结论

### 1.1 确认为真（逐项）

| 报告声明 | 核查证据 | 结果 |
|---|---|---|
| t7 冻结、20:12 恢复、补录 138 事件 | journal 58876 行；补录区间 58604–58741 = **138** 精确吻合；`turn_sealed state=cancelled` | ✅ |
| audit 7054ms、19:05:30.649 返回、outbox ok | audit.csv 行 `2026-09-02T11:05:30.649…,agent,bash,…,ok,7054`（本地 19:05:30.649，毫秒级吻合）；tool_outbox.wal mtime **19:05:30.645**，`call_9e9a…,bash,status=ok` 且与 t7 tool block 同 call_id | ✅ |
| gdb 栈帧偏移（242823/242783/243249） | `/tmp/bt_all.txt`（**仍存在，见 1.2**）逐帧核对：actor 15 帧、timeline 7 帧、健康对照 #2–#10 同地址，全部一致；读线程 247267/247268 均阻塞 `read()` | ✅ |
| 0 panic / 0 ERROR | daemon 日志全量 grep 计数 0/0 | ✅ |
| TUI 19:26 重连 | 日志 `[1788348381] worker alive for 692d1605; skipping bootstrap orphan seal`（=19:26:21） | ✅ |
| 会话隔离经受考验 | 冻结窗口（19:26:40–20:12:00）内日志 535 行**全部**属于其他会话（ce5b9755/0049402f 的 t6/t12），692d1605 零活动 | ✅ |
| 构建 886540b、HEAD=d89e38c off-by-one | `git show`：886540b（16:45:46）→ d89e38c（17:59:39）父子关系属实；abb2038 存在 | ✅ |
| W-low① 兜底 | `exec.rs:876-894`：`recv_timeout(2s)` ×2 + `captured()` 快照回退，注释明写假设"H6 修复后 EOF 正常到达" | ✅ |
| `wait_for` 无取消检查 | `process_registry.rs:401-425`：循环仅 try_wait+超时，无任何 cancel/is_cancel 检查 | ✅ |
| 两个测试名 | `exec.rs:1613/1645`（per_call_cancel，双平台）与 `exec.rs:1865`（timeout_transfers）真实存在 | ✅ |
| meta.json cwd 反斜杠 | `"cwd": "\\home\\qaqtamsy\\…"` + 日志 `cannot cd to '\home\…'` WARN | ✅ |
| 事故 bash 原命令 | messages.jsonl 找到原文：`rm -rf /tmp/qaqh && … && QAQH_DATA_DIR=/tmp/qaqh nohup ./target/release/qaqh-daemon run > /tmp/qaqh-repro-daemon.log 2>&1 &` | ✅ |
| kill 即恢复 | 日志 20:12:24/31 两条 termination signal（247272/247286）；journal mtime 20:12:45；t8 INPUT（text_len=6="继续"）20:12:31 | ✅ |

### 1.2 事实错误（1 处）

- **"/tmp/bt_all.txt 已随 /tmp 易失" → 不实**。文件仍在（22706 字节，mtime 20:10），全部冻结栈证据可用。
  影响：阶段 0 成本大幅下降（无需"凭记忆重建栈"），但需**立即归档**防真丢失。

### 1.3 因果/数字偏差（2 处，不动摇主结论）

1. **journal 零事件窗口起点早于 exec 返回**。补录尾部包含 reasoning deltas（fragment 120–128）与整个 text block 26，而模型流必须在 tool admit（bash 启动 19:05:23.6）**之前**完成 → journal writer 停止落盘 ≤19:05:23.6，比报告所称"19:05:30.65 之后"**早约 7 秒**，且与管道 EOF 无关。
   → 存在一个**独立的 journal 落盘停摆**（H-A 类），与 exec 封口等待（H-B 类）**可能是两个叠加的缺陷**。
2. **数字不自洽**：标题"冻结 67 分钟"（19:05:30→20:12:24≈66.9min ✓）与正文"journal 61 分钟零事件"互斥；按 1.3.1 实际零事件窗口 ≈67–68 分钟。

### 1.4 超出报告的新发现

1. **H-B 已可钉到行**（报告止步于"候选"）：
   - `exec.rs:737/769` — stdout/stderr 读线程各自 `progress_tx.clone()` 并 move 进线程；读线程阻塞 `read()` 期间克隆不 drop；
   - `engine_tool.rs:790-804` — `drain_progress` 仅在 `Disconnected` 时 break，`Timeout` 无限循环；
   - 组合效果：孙进程持有管道写端 ⇒ 读线程永卡 ⇒ 信道永不断开 ⇒ **actor 必然永卡 drain**（确定性，非概率性）⇒ `join()`（engine_tool.rs:640）不返回 ⇒ 封口事件永不发射。
   - kill→EOF→读线程退出→克隆 drop→drain break→join 返回→缓冲事件按序刷盘：与补录 138 事件的**顺序**（3×tool_progress → block_sealed → round_sealed → turn_sealed cancelled）完全互证。
2. 报告中 `execute_and_emit` 的运行轮询三层检查（`exec.rs:820-840`：per-call cancel + is_cancel / deadline / handoff）与提案阶段 1 的设计完全对应，方案可行性得到代码佐证。

### 1.5 无法独立证实（诚实项）

- 247272 树具体在哪个 fd 持有管道写端（报告亦列为 P1 待复现，未过度声称）；
- `~/.local/share/qaqh/qaqh-daemon.log` "被并行会话删除"（目录现已不存在，无法证明曾存在；与多会话互踩的主题方向一致）；
- H-A 与 H-B 对 journal writer 停摆的责任划分（需阶段 0 符号化；但 1.4.1 的 H-B 机制无论 H-A 是否存在都必然导致冻结）。

---

## 二、执行计划（修正版，三阶段 + 观测线）

> 与原提案的差异：①阶段 0 成本下调（栈文件还在）；②新增阶段 1 的"最小止血"优先级——H-B 是确定性 bug，可先于重写独立修复上线；③新增 journal writer 停摆的独立调查线（1.3.1）。

### 阶段 0：钉死根因（0.5–1h，前置必做）

- [x] 0.1 **立即**归档 `/tmp/bt_all.txt` → `docs/incidents/2026-09-02-exec-freeze-bt-all.txt`（防真丢失）。（已归档，md5 一致：a1e3d8a2…）
- [x] 0.2 `git worktree` 出 886540b，带符号重构建（`debug=1`、关 strip）→ 用 `nm` 映射关键偏移。
  （已完成但**结论修正**：构建成功（143MB 未 strip，31691 符号），但与事故二进制**布局不匹配**——报告所载偏移实为运行时地址的低 24 位截断，真实加载基址随 /proc/242770/maps 一并失传。证据链：① thread_entry 锚帧 `0x…69b0c12` 与重建 `thread_start`（vaddr 0xb143b0）之差不落在任何页对齐基址；② 全基址窗口（1236 页）逐页扫描，指令边界最大命中仅 13/23；③ 212 个全帧 DWARF 覆盖候选中，addr2line 解析 **0/23** 落在 qaqh 工作区源码。判定：事故二进制为**脏树构建**（16:45 提交、16:48 部署，当时工作区含未提交变更），干净 886540b 重建无法精确符号化。报告"栈复活解析方法"的前提（干净提交构建）不成立。）
- [x] 0.3 判定清单 → **以代码审读 + 事件流分析替代失效的符号化**：
      - **H-B 实锤并钉到行（无需符号化）**：exec.rs 读线程持有 `progress_tx` 克隆（L737/769）+ engine_tool.rs 旧 `drain_progress` 仅 `Disconnected` 退出 ⇒ 孙进程持管道写端时 actor 永卡 drain、封口事件永不发射。确定性 bug，已由 1.1 修复。
      - **journal 停摆（H-A 类）确认独立存在**：事件流分析证明停摆早于 exec 返回 ~7s（边界在 reasoning fragment 119/120 之间、bash 启动前），且冻结期间存在"已发出未落盘"积压（deltas 120+ 与整个 text block 均在补录中）。写入侧机制无法在布局失配重建上定位；候选：wake 通道丢失唤醒 / 写线程锁等待。
      - **writer 栈形态判定修正**：其 futex 停靠形状与"空闲等待事件"不可区分（与报告对 actor 的观察同理），不能仅凭栈形定罪"冻结"。
- [x] 0.4 产出根因记录（本文档 + 本节）：
      - H-B：exec.rs:737/769 + engine_tool.rs:790-804（旧实现，1.1 已修复）——actor 冻结与 kill 恢复因果、补录 138 事件顺序全部互证。
      - H-A：范围确认（timeline 持久化写侧），`文件:行号` 待复现实验（符号化构建 + 受控复现）。1.4a 时间戳落地后，下次事件可直接在行级分辨"产生滞后"与"落盘滞后"。
      - 符号化基础设施保留：`~/Projects/qaqh-sym-886540b`（release+debug worktree）供复现实验复用。

### 阶段 1：最小止血（P0，可独立发版，0.5 天）

- [x] 1.1 **drain_progress 有界化**（修确定性冻结，对应 1.4.1）：drain 在"工具线程已结束"时强制退出（保留 Disconnected 快速路径）。
  （已完成：engine_tool.rs 新增 `drain_bounded`——`tool_done`（JoinHandle::is_finished 组合）为真后最终 try_recv 排空即收尾，绝不等待 Disconnected；`drain_progress_external` 增加 tool_done 参数，engine_tool.rs + admit.rs 全部 5 个调用点接入。附带治愈移交后台（backgrounded）路径的同类潜在卡死：读线程随存活进程持 sender，旧 drain 同样永不退出。）
- [x] 1.2 **exec 完成路径去 EOF 化**：子进程退出后以短排空预算 + `captured()` 快照返回，`recv_timeout(2s)`（W-low①）整体退役（报告 P0-1）。
  （已完成：exec.rs 新增 `recv_stream_bounded` + READER_SETTLE_TICK/BUDGET（50ms/300ms）——正常路径读线程 EOF 即返回、零额外等待，异常路径预算耗尽以快照为权威返回；快照不可得时保留 WARN 占位并标记 truncated。）
- [x] 1.3 **读线程生命周期绑定**：cancel/超时/封口时 `killpg` 整组（Windows `taskkill /T`），保证读线程必然退出、克隆必然 drop。
  （已完成：阶段 1 验证 `ProcessRegistry::kill` 已实现 Unix `killpg(SIGKILL)` / Windows `taskkill /T`（process_registry.rs kill），取消路径读线程必然退出；"正常退出+孙进程存活"的读线程驻留已由阶段 2 poll 化读循环治愈（见 2.2），回归 `reader_threads_terminate_after_grandchild_settle_even_without_eof`。）
- [x] 1.4 **观测三件套**（报告 P0-2）：journal 行加时间戳；receipt `running`>30s 告警；`GET /activity` 暴露 has_active_work。
  （已完成：① timeline_store.rs `TimelineJournalOp::Append` 增加 `ts` 字段（`serde(default)` 兼容旧行，`append_journal` 写入时注入，backfill/旧行为 None）；② pending_store.rs `warn_stale_running`（Accepted/Running 超 30s 无终态即 WARN，60s 限频，挂靠 daemon 既有 3s 周期任务）+ 限频/终态测试；③ axum_server.rs `GET /activity`（has_active_work + 逐会话 activity 快照，与 /health 同级免鉴权、仅含 seed/state/turn_id 无用户内容）+ 路由测试；service.rs `has_active_work` 重构为 `activity_snapshot()` 委托，单次加锁保证一致性。）
- [x] 1.5 **新增回归测试**：孙进程持有管道写端 → 子进程退出 → 回合必须在有界时间内封口（本次事故的直接回归）；`wait_for` 补取消检查（报告 P1）。
  （事故回归已完成：exec.rs `grandchild_holding_pipe_write_end_collects_bounded`（unix `sleep 5 &` / windows `start /b` 双平台）+ engine_tool.rs `drain_bounded_*` 三测；`wait_for` 取消检查已随阶段 2 落地（见 2.3 附带修复）。）
- [x] 1.6 双平台跑既有契约测试：`timeout_transfers_process_to_background_registry`、`per_call_cancel_stops_only_the_running_command`（exec.rs:1613/1645/1865）全部保持通过。
  （Linux 侧已跑：exec::tests 全量 23 通过、qaqh-runtime 全量 165 通过、clippy 无新增告警；`timeout_transfers_process_to_background_registry` 为 #[cfg(windows)] 专属，留待 Windows 侧冒烟。）

### 阶段 2：registry-native 生命周期重写（0.5–1.5 天）

按原提案阶段 1 执行，六点设计不变（start/run-to-completion/seal/超时/取消/流式），

- [x] 2.1 契约：seal 以 registry 快照 `captured()` 为权威——**任何路径不得等待管道 EOF / 流关闭**（写入代码评审 checklist）。
  （已完成 2026-09-06：①评审清单落地 `docs/exec-lifecycle-review-checklist.md`（六节契约+测试矩阵）；②注册表新增**完整捕获**缓冲 `FullCapture`/`captured_full`（5MB 字节上限，`append_output/stderr` 同时写 tail 视图与 full 捕获）——快照从"4KB 尾部兜底"升级为权威完整数据源，孙进程场景不再丢头部；③seal 重写为 `captured_full` 权威 + 对读线程退出信号的**有界 join**（`SEAL_JOIN_BUDGET` 500ms）：正常路径读线程先于 seal 完成、首次 recv 即返零额外等待，异常路径信号来自 settle 到期。回归：`seal_uses_full_registry_capture_not_tail_when_grandchild_holds_pipe`（3000 行输出首尾俱全，旧兜底只剩 4KB 尾）。）
- [x] 2.2 阶段 1 的 1.1–1.3 作为重写内建行为，不留兼容分支。
  （已完成 2026-09-06：①**1.3 驻留治愈**——读线程 poll 化（unix `fcntl O_NONBLOCK` / Windows `PeekNamedPipe` 探测），退出条件完备（EOF/读错误/字节上限/settle 到期），"正常退出+孙进程持写端"时读线程确定性退出并 drop 全部 sender，回归 `reader_threads_terminate_after_grandchild_settle_even_without_eof`；②**1.1/1.2 内建化**——`recv_stream_bounded` 与汇总信道整体退役（数据源被 captured_full 取代），`drain_bounded` 语义保持；③读线程 handoff 善后块（100×50ms 轮询 mark_exited）判定为死代码删除——`try_wait` 已在任何查询路径自动置终态。字节上限语义保持：cap 触发后不再 sink 排空到 EOF（旧实现此处同样可被孙进程卡死），超限就地丢弃并受 settle 约束。）
- [x] 2.3 判定：**不触发扩围**。阶段 0 结论为 H-A（journal 落盘停摆）属 timeline 持久化**写侧**且机制未定位（已挂并行观测线独立调查），并非 daemon/hub 广播层背压——原提案 2.3 的触发条件（H-A 判定为 daemon/hub 层）不成立。若后续 journal writer 调查实锤 hub 相关，再按本条扩围。
  （附带修复随本阶段落地：报告 P1 `wait_for` 取消检查——`ProcessRegistry::wait_for` 增 per-call 旗标 + ambient `is_cancel()` 双检查，命中即返回并标记 `wait_interrupted_by_cancel`，process 工具 wait 动作接线；回归 `wait_for_returns_promptly_on_per_call_cancel`。）

验证（Linux 侧，2026-09-06）：qaqh-workspace 307 通过（exec::tests 26 = 既有 23 + 新增 3）、qaqh-runtime 166 通过、qaqh-daemon 27 通过、clippy 零新增告警。Windows 专属测试（`timeout_transfers…`/`background_*` 双平台组）与 `PeekNamedPipe` 路径留待阶段 3.2 双端冒烟。

### 阶段 3：绞杀 + 删除（0.5 天）

- [ ] 3.1 新路径 soak 后删除 exec 前台直读管道代码与 W-low① 兜底；compat 注册收敛。
  （进展 2026-09-06：旧管道汇总代码与 W-low① 已随阶段 2 重写删除；剩余=清扫 `ProcessRegistry::captured()`（已零调用方）与 compat 注册（`register_exec_for_compat`，删/留需确认内部调用方）收敛。）
- [ ] 3.2 双端冒烟（**pwsh 路径必测**）。
### 并行观测线（不阻塞主线）

- [x] fd 持有复现任务（P1）：spawn 假 daemon + `/proc/*/fd` 对管道 inode，精确回答"哪个 fd 持有写端"。
  （已完成 2026-09-06：证据文档 `docs/incidents/2026-09-06-fd-hold-repro.md`。结论：**裸 `cmd &`（无重定向）的孙进程持 fd1/fd2=写端**，孤儿化 reparent 至 subreaper 长期滞留；事故记载的 `nohup … > log 2>&1 &` 全重定向形态本身**不产生**写端持有——真实现场的持写端后代必存在未重定向输出。副产品：孤儿 reparent 目标为 harness subreaper（非 init），佐证多会话互踩主题。）
- [ ] journal writer 停摆独立调查（源自 1.3.1）：确认停摆点与 emit→writer→hub 管道的关系。
- [x] 纪律：长驻服务禁止 bash `nohup &`；评估 exec 层对 `nohup.*&` 模式的强提示。
  （已完成 2026-09-06：exec/bash/pwsh 工具层检测后台派生（剥除 `&&`/`>&`/`&>` 后残留 `&`），前台完成的结果追加强提示（模型可见）引导 `background_after_secs` + process 工具受控路径；判定边界测试 + 工具层 e2e。）
- [x] meta.json 反斜杠 cwd 残留（abb2038 关联类）排入清理。
  （已完成 2026-09-06 根因修复：`grouping::canonical_cwd` 历史实现**无条件** `/`→`\`（Windows 时代残留），Linux 上把 meta.cwd 写坏为 `\home\...` → `cannot cd` WARN。修复：写侧平台化（Windows 保留 `\` 归一 + verbatim 前缀剥离，非 Windows 原生 `/`）+ 读侧 `repair_legacy_backslash_cwd` 存量修复（workspace_cwd 返回前归一）。注：存量 workspaces.json 中已损坏的归属路径需重建或手修。）
- [ ] 纪律：长驻服务禁止 bash `nohup &`；评估 exec 层对 `nohup.*&` 模式的强提示。
- [ ] meta.json 反斜杠 cwd 残留（abb2038 关联类）排入清理。

---

## 三、风险与后续

1. **off-by-one-commit 符号漂移**：用 886540b 精确重构建可消除；若个别帧仍漂移，以相邻帧锚定。
2. **H-A 若实锤**：阶段 2 范围扩大，工期 +0.5 天量级；不影响阶段 1 止血价值。
3. **本核查未运行 cargo test**（构建成本考量）：阶段 1.6 为发版闸门，必须实际执行。
4. 报告中"61 分钟"等数字偏差建议在原提案文档加勘误注记（本文档已修正，原稿保留作历史存档）。
