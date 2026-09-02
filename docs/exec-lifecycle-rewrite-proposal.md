# exec 生命周期重写提案（基于 process/registry 收敛）

状态：提案存档（2026-09-02，未排期）。触发事故：session 692d1605 turn t7 冻结 67 分钟。
构建：886540b（生产与实验 daemon 同版）。诊断会话即运行在该 daemon 的 exec 之下，全部证据为活体实测。

## 一、事故结论（已定罪部分）

时间线（~/.config/qaqh/ringing/timeline-journal/692d1605.jsonl + audit.csv + ~/.qaqh/qaqh-daemon.log 交叉验证）：

| 时刻 | 事件 |
|---|---|
| 19:03:03 | loop 收到最后一条 worker command（SendMessage，启动 t7），此后再未回到命令接收点 |
| 19:05:23–28.6 | t7 round2 的 bash 执行（rm -rf + `nohup ./qaqh-daemon run > log 2>&1 &` + DISCOVERY-OK + pgrep） |
| 19:05:28.6 | bash 退出；但 exec 管道写端被 nohup 孙进程树（247272/247286）持有 → 读线程 247267/247268 永等 EOF |
| 19:05:30.649 | exec 走 W-low① 兜底返回（audit 7054ms = 5.05s 运行 + 2.0s 收集超时；tool_outbox status=ok） |
| 19:05:30.65 之后 | 【第二个无界等待】封口事件发射路径冻结：journal 61 分钟零事件、t7=running、ConversationCancel accepted 但永无终态 |
| 20:12 附近 | 用户 kill 247272 → EOF → 读线程退出 → journal 补录 138 事件 → t7 → cancelled |

排除项：panic（日志 0 panic / 0 ERROR，线程存活）；普通死锁的表象不符（futex 停靠形状与健康 actor 相同）；PR-3-4 会话隔离经受住考验（ce5b9755 / 0049402f 全程无感）。

## 二、冻结线程栈（gdb 2026-09-02 20:08，二进制 stripped，地址态）

/tmp/bt_all.txt 已随 /tmp 易失，关键帧备份如下（基址 + 偏移，二进制加载基址见当时 /proc/242770/maps）：

```
LWP 242823 "qaqh-session-69"（冻结 actor）:
  syscall ← 0x…9b66c6(futex包装) ← 0x…65f3e02 ← 0x…6627c77 ← 0x…66271dc
  ← 0x…6623044 ← 0x…661d3c6 ← 0x…6613389 ← 0x…66096e8 ← 0x…6609700×2
  ← 0x…65ff597 ← 0x…65f669d ← 0x…65f495a ← 0x…65f47f9 ← 0x…69b0c12(线程入口)

LWP 242783 "qaqh-timeline-p"（时间线落盘线程）:
  syscall ← 0x…9b66c6 ← 0x…65f3e02 ← 0x…69c8ccc ← 0x…69c85f1
  ← 0x…6647591 ← 0x…664743a ← 入口

健康对照 LWP 243249 "qaqh-session-ce"（ce5b9755，当时空闲）:
  #2–#10 与 242823 完全同地址 → 栈形状本身无法区分"等命令空闲"与"等工具结果"，
  需要 242823 之外的判据（journal 零增长 + receipt running）才定罪。
```

栈复活解析方法：`git -C QAQ-Harness worktree` 出 886540b，带符号重构建（关 strip、debug=1），
用 `nm` 将上述偏移映射回函数。注意：off-by-one-commit（HEAD=d89e38c）可能使个别帧漂移。

## 三、根因两段论

1. **上半段（已定位到设计假设）**：exec 前台路径假设"子进程退出 ⇒ 管道 EOF"。子进程再派生
   持有写端的后台进程时假设破产。exec.rs 的 W-low① 兜底（2s recv_timeout → registry 快照）
   正是为此设计且实测触发——exec 本身**有界返回了**。
2. **下半段（已锁定范围，未钉到行）**：exec 返回、audit/outbox 落盘之后，"工具结果 → 封口事件发射"
   路径上存在第二个无界等待。两个候选：
   - H-B：封口等待 progress 流关闭（流关闭 = 全部写端 EOF = 孙进程死亡）——与"kill 即恢复"完美吻合；
   - H-A：hub 向 SSE 客户端广播的无限背压（TUI 于 19:26 曾重连，`worker alive; skipping bootstrap orphan seal`）。
   判定手段见第二节栈复活解析，或直接审计 engine_tool/loop_core 封口发射代码路径。

## 四、重写方案（三阶段，绞杀式）

### 阶段 0：钉下半段根因（1-2h，前置必做）
符号化解析栈 → 判定 H-A/H-B → 拿到 `文件:行号`。
**此步决定重写是否治愈本次症状**：若为 H-A（daemon 层），仅重写 exec 不够。

### 阶段 1：新生命周期并行落地（0.5–1.5 天）
工具名/schema/权限/audit 全不变（bash/pwsh 照旧），内部换 registry-native 实现：

```
start：解析可执行 → 新进程组 spawn → registry.register + attach_child → 立即返回句柄
run-to-completion：poll { try_wait; per-call cancel + is_cancel() 三层检查; deadline; handoff }
seal：以 registry 快照（captured()）为权威 —— 任何路径不得等待管道 EOF / 流关闭
超时：移交后台（status=backgrounded，process 工具接管）——语义不变
取消：killpg 整组（Windows taskkill /T）——语义不变
流式：reader 线程照发 progress chunk（保住实时输出 UX），但封口绝不等待它们
```

**契约 = exec.rs 既有测试移植**：timeout_transfers_process_to_background_registry、
per_call_cancel_stops_only_the_running_command 等全部保留。
**新增回归测试（本次事故）**：孙进程持有管道写端 → 子进程退出 → 回合必须在有界时间内完成封口。

红利：统一 cancel 故事后，`process_registry::wait_for` 无取消检查的缺口一并修掉。

### 阶段 2：绞杀 + 删除（0.5 天）
新路径 soak 后删 exec 前台直读管道代码与 W-low① 兜底；compat 注册收敛；双端冒烟（pwsh 路径必测）。

## 五、附带修复清单

- P0：exec 完成路径去 EOF 化（子进程退出后直接快照返回，2s 等待也不留）。
- P0：receipt `running` > 30s 告警（本次僵尸只能靠人肉轮询发现）；journal 行加时间戳；`GET /activity`（has_active_work 只读暴露）。
- P1：`wait_for` 补取消检查（非 exec 阻塞工具的飞行中取消）。
- P1：fd 持有复现任务——精确回答"247272 树在哪个 fd 持有管道写端"（对照实验：spawn 假 daemon + /proc/*/fd 对管道 inode）。
- 纪律：长驻服务禁止 bash `nohup &`（exec 工具 hint 已写明应转后台后用 process 工具接管）。

## 六、杂项证据（防失传）

- 生产 daemon 242770 fd1/fd2 → /dev/null，fd3 → ~/.qaqh/qaqh-daemon.log（真日志，仍在写）；
  ~/.local/share/qaqh/qaqh-daemon.log 于 19:49–19:50 间被并行会话删除（多会话互踩实证）。
- 实验环境 /tmp/qaqh（QAQH_DATA_DIR 隔离）由 692d1605 的 bash nohup 创建，测试 daemon 247272
  已于 20:12 前后被用户 kill，247286 同亡。
- 692d1605 meta.json cwd 为 Windows 反斜杠路径（`\\home\\...`）→ `set_process_workspace cannot cd`
  WARN 的来源（abb2038 要根除的残留类）。
- 会话恢复后 t8（"继续"）被用户手动 Esc 终止——clear_cancel 时机无次生 bug。
