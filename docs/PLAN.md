# 冗余收敛执行计划（redundancy-convergence PLAN）

> 状态：**已完成**（2026-09-06 制定并当日执行完毕；全部 11 个 PR 收口，§8 总闸全表达标，
> 全量 test 863 通过 / 0 失败 / 3 忽略 ≥ 基线 853，clippy 0/0，fmt clean，bindings 70 不变，crates 14）。
> 上游文档：`docs/redundancy-audit-2026-09-06.md`（冗余审计报告）——问题清单、证据与复现命令的**唯一权威来源**。
> 前任计划：`docs/boundary-reform-plan.md`（crate 边界整改，Phase 0–4 已全部收口归档）。其
> 「提案 → PLAN → 分阶段 PR → grep 可测验收 → 独立回退」的方法论由本 PLAN 直接继承。
> 偏离规则：执行中发现与审计报告不符，**先改审计报告再改本 PLAN**；当日发现的全部偏差登记于 §9 勘误表。

---

## 0. 执行上下文（2026-09-06 实测快照）

- 分支 `main`，HEAD `d172ac7`；代码区干净，docs 侧存在未提交改动（boundary-reform-plan.md 重命名、
  本 PLAN 与审计报告 untracked），PR-0-1 先行入库（审计报告勘误 C-1）。
- 规模：15 crates ≈ 82k 行 Rust；codegraph 索引 299 文件 / 5,821 节点 / 20,715 边。
- 审计结论：10 项应清理发现（9 项经主会话双重复核）+ 8 项合理冗余保留 + 3 项误报排除；
  全部验收命令见审计报告附录 A，本 PLAN 只列 PR 切分与出口。
- 已知基线：`cargo check --workspace --all-targets` 有 ~16 条 test 目标警告
  （含 1 条 `unreachable pattern`，PR-1-3 关闭）。
- 基线已实测登记（PR-0-1，2026-09-06）：详见 §8。摘要：test 853 通过 / 0 失败 / 3 忽略；
  clippy 0 error / 320 条诊断首行（含 lib/test 重复计数）；fmt clean。

**方法论警示（继承审计报告）**：codegraph `callers` 存在漏报实例。本 PLAN 所有「删除/收敛」类
操作的唯一判定标准是 **`rg` 全量文本检索**；codegraph 仅用于探索加速，不作为出口依据。

## 1. 总览：阶段与 PR 切分

| 阶段 | 主题 | PR | 风险 | 依赖 |
|---|---|---|---|---|
| Phase 0 | 基线登记 | PR-0-1 | 无 | — |
| Phase 1 | 死库存清零（纯删除） | PR-1-1 / 1-2 / 1-3 | 极低 | PR-0-1 |
| Phase 2 | gate 全局客户端单源（含 bug 加固） | PR-2-1 | 低 | PR-0-1 |
| Phase 3 | proto 收敛（核心战役） | PR-3-1 … 3-5 | 中 | PR-0-1；阶段内按序 |
| Phase 4 | 可穿插小项 | PR-4-1 / 4-2 | 低 | 无硬依赖 |

- 顺序纪律：**先死库存（零语义变化）→ 再 wire 形状兼容验证过的单源化**；
  结构塑形（巨型方法/文件拆分）**不在本 PLAN 范围**（§7 D-5），完成后另行立项。
- 对外可见面一律不动（继承前任 PLAN 纪律）：JSON wire shape、daemon.json 磁盘格式、
  `bindings/qaqh` TS 面、`qaqh-workspace` CLI 词表、Ringing V1 协议。

## 2. Phase 0 —— 基线登记

### PR-0-1 基线实测与登记

- 跑 `cargo test --workspace`、`cargo clippy --workspace --all-targets`、`cargo fmt --all --check`，
  将通过数/结果登记进 §8 基线表（前任 PLAN 教训：出口口径必须先实测，不许假设全绿）。
- 将本 PLAN 与审计报告 commit 入库（当前均 untracked——权威文档不入库，「唯一权威来源」无从谈起）。
- 确认 §7 全部决策点已拍板（D-4 已实证拍板，其余默认值可执行，但需用户过目一次）。

## 3. Phase 1 —— 死库存清零

### PR-1-1 删除 `crates/qaqh-workspace/src/file_core/`（审计 #1，P0）

- 现状：7 文件 1,813 行，`lib.rs` 无 `mod file_core;` 声明，不在编译图；`ledger.rs` 与
  `file_state.rs` md5 孪生（`1e1ab570…`）；`file_core/mod.rs:9` doc 已失实。
- 动作：整目录删除；顺带清理全仓对 `file_core` 的注释残留。
- 验收（grep 可测）：
  ```bash
  rg -l "file_core" crates/          # → 0 命中
  cargo check --workspace            # 零新增错误（其本就不在编译图，验证性检查）
  cargo test -p qaqh-workspace       # 绿
  ```

### PR-1-2 退役 `qaqh-runtime/src/agent/wire.rs`（审计 #3，P0）

- 现状：`agent/mod.rs:64` 无条件 `pub mod wire;`，自述服务已退役的 `Loop::new_ipc`；
  全仓唯一使用方 `tests/common/mod.rs:12`。
- 动作：迁移至 `qaqh-runtime/tests/common/`（新文件或并入既有 mod），删除 `pub mod wire;` 声明与源文件，更新 `tests/common/mod.rs` 导入。
- 验收：
  ```bash
  rg -n "pub mod wire" crates/qaqh-runtime/src        # → 0
  rg -n "agent::wire" crates/                         # → 0（迁移后 tests/common 为本地 mod wire，不再出现该字样）
  cargo test -p qaqh-runtime                          # 绿
  ```

### PR-1-3 test 目标警告清零（审计 #10，P3 提前搭车）

- 动作：`cargo fix --workspace --all-targets --tests` 清机械项；`unreachable pattern` **人工核查**
  （可能隐藏逻辑错误），处置结论登记 §9。
- 验收：`cargo check --workspace --all-targets 2>&1 | rg -c "^warning"` → **0**
  （弃用 grep -c：0 命中时 exit 1 会断 && 链/CI；口径只数诊断首行，per-crate 汇总行
  「warning: … generated N warnings」在归零时自然消失）。

## 4. Phase 2 —— gate 全局客户端单源化

### PR-2-1 合并三份 `GLOBAL_CLIENT`（审计 #2，P0，含 bug 加固）

- 现状（实测漂移）：chat(15s 连接 + keepalive 60s + pool 120s + 总 30min) /
  message(30s + keepalive + pool + 30min) / responses(**仅 30s 连接超时，缺 keepalive/pool/总超时**
  ——reqwest 默认无总超时，流僵死可能永久挂起，接近 bug)。
- 动作：`qaqh-gate` 内新增单一构造函数（建议 `lib.rs` 或 `types.rs`），三适配器统一引用；
  responses 补齐 `tcp_keepalive(60s) + pool_idle_timeout(120s) + timeout(30min)`；
  connect_timeout 统一取 30s（§7 D-3，弱网更保守）。
- 行为变化（PR 描述必写）：chat connect_timeout 15s→30s（D-3 已拍板），弱网失败判定变慢 15s。
- 后续纪律：未来 provider 级差异走显式配置字段，**禁止再复制构造**。
- 验收：
  ```bash
  rg -n "LazyLock<Client>" crates/qaqh-gate/src       # → 仅 1 处定义
  cargo test -p qaqh-gate                             # 绿
  ```
- 人工冒烟：三协议（chat/responses/anthropic）各跑一次流式对话（SSE 行为不回归）。

## 5. Phase 3 —— proto 收敛（核心战役）

> 决策前提：§7 D-1 默认**方案 A**——proto 溶解，聚合投影并入 `qaqh-domain`，crate 消失。
> 消费面实测：proto 59 处引用（runtime 41 / workspace 12 / daemon 6；occurrence 口径，
> `rg --count-matches "qaqh_proto"`），**零前端曝光**（70 个 TS bindings 无一 proto 类型）；
> domain 650 处引用、5 个 crate 依赖、前端契约唯一来源。

### PR-3-1 `DaemonDiscovery` 单源化下沉 `qaqh-types`

- 现状：定义于 `proto/control.rs`（daemon 读写 daemon.json）；`qaqh-client/src/discovery.rs:12`
  **另有一份手工副本**（因 client 刻意不依赖 proto）——磁盘契约被定义两次，零编译护栏。
- 动作：`DaemonDiscovery` + `CONTROL_PROTOCOL_VERSION` 迁至 `qaqh-types`（如 `src/discovery.rs`）；
  **同步删除 proto 侧原定义（control.rs）与 lib.rs:23 re-export，避免双定义**；
  daemon 改 `use qaqh_types`（已依赖 types，无新增边）；删除 client 本地副本改统一引用；
  control.rs / lib.rs 内其余对两符号的引用随定义一并清理。
- 兼容红线：daemon.json 磁盘格式逐字节不变；新增 client 侧 roundtrip 测试
  （旧格式样本解析 → 序列化 → 字段保全）。
- 验收：
  ```bash
  rg -n "struct DaemonDiscovery" crates/              # → 1（qaqh-types）
  rg -n "qaqh_proto" crates/qaqh-client crates/qaqh-daemon   # → 0
  cargo test -p qaqh-client -p qaqh-daemon            # 绿
  ```

### PR-3-2 会话活动状态单源化（删映射点①）

- 现状：`proto::SessionActivity/SessionActivityState` 与 `domain::ActivityState`/
  `ControlEvent::SessionActivityChanged` 变体、字段逐一重复（domain 注释自认「与 legacy 同义」）；
  `runtime/activity.rs:15-21` 五臂 match 翻译 + 逐字段抄写，活动状态「双发」。
- 动作：`activity.rs` / `registry.rs:644,648` / `service.rs:612` 改用 domain 类型；
  删 proto 侧两个类型。
- 兼容红线：`session.activity` JSON 方法与 `session.list` 相关响应 **shape 逐字段不变**；
  重构前先落一个「shape 快照」serde roundtrip 测试锚定。
- 验收：
  ```bash
  rg -n "SessionActivityState|proto::SessionActivity\b" crates/   # → 0（event.rs:96 的 legacy 注释随本 PR 更新）
  rg -n "SessionActivityChanged" crates/ | wc -l                  # → 4（白名单：event.rs:527 定义 /
                                                                  #    activity.rs:24 发布 / projection.rs:90
                                                                  #    消费 match / :354 测试构造；「双发」等注释随 PR 更新）
  cargo test -p qaqh-runtime                                      # 绿
  ```

### PR-3-3 dashboard 投影单源化（删映射点②）

- 现状：`runtime/agent/dashboard.rs` 全文件 34 行，仅做 workspace `DocInfo/TaskInfo` →
  `domain::DashboardDocument/DashboardTask` 的逐字段抄写。
- 动作：`workspace/dashboard.rs` 直接产出 domain 模型（新增 **workspace→domain** 依赖边；
  domain 仅依赖 types，无环；workspace 禁止 import domain 事件类型——评审检查项，R-4）；
  `agent/dashboard.rs` 删除或退化为纯组装。
- 验收：
  ```bash
  rg -n "DocInfo|TaskInfo" crates/                    # → 0 命中（类型随之删除）
  rg -n "qaqh_domain" crates/qaqh-workspace/Cargo.toml # → 1（新依赖边登记）
  cargo test -p qaqh-workspace -p qaqh-runtime        # 绿
  ```

### PR-3-4 TurnData 系聚合投影迁入 `qaqh-domain::timeline`

- 现状：`TurnData/RoundData/RoundBlock/ToolCallDef/ToolResultDef` 是 resume 路径的**聚合投影**
  （回合聚合树 ≠ domain 事件流，形状确有存在理由），但住在 proto；消费方 5 文件全在 runtime。
- 动作：迁入 `domain/src/timeline.rs`（新增 projection/aggregate 分区或子模块）；
  `ringing/projection.rs`、`timeline_rebuild.rs`、`conversation_snapshot.rs`、`agent/util/mod.rs`、
  `agent/types.rs` 改 `use qaqh_domain::…`；**不加 ts-rs 导出**（维持零前端曝光的行为现状）。
- 兼容红线：resume / compact-context 检查点链集成测试必须全绿（fail-closed 语义不回归）。
- 验收：
  ```bash
  rg -n "qaqh_proto::(TurnData|RoundData|RoundBlock|ToolCallDef|ToolResultDef)" crates/   # → 0
  cargo test -p qaqh-runtime                          # 绿（含全部集成测试；--test '*' 需 cargo≥1.65
                                                      #  且冗余，弃用）
  ```

### PR-3-5 删除 `qaqh-proto` crate

- 动作：确认 `FileSnapshotInfo` 全仓零引用（审计标记的死类型，删除前 rg 复核）；
  剩余类型若仍有引用则回流 PR-3-4 补迁；`Cargo.toml` workspace members 移除；删目录；
  README workspace 成员表修正（16→14，README 本就漂移，顺带治理）。
- 验收：
  ```bash
  rg -n "qaqh_proto|qaqh-proto" crates/ Cargo.toml README.md     # → 无任何输出（exit 1 = 零匹配）
                                                                  # 范围声明：docs/ 中历史性提及（审计/勘误）不算残留
  cargo build --workspace && cargo test --workspace              # 全绿
  ls crates | wc -l                                              # 14
  ```

## 6. Phase 4 —— 可穿插小项（无硬依赖，任意时机）

### PR-4-1 `file_edit_v2` shim 退役（审计 #5）

- 动作：调用点改 `crate::edit::`——registration.rs:14（`use super::file_edit_v2;`）与
  registration.rs:39、confirm_apply.rs:51[,156 测试辅助内]、file_query.rs:520,551 →
  删 `file_edit_v2.rs` → 删 `edit/mod.rs:58-70` 兼容重导出段（D-4 已实证拍板，见 §7）。
- 验收：`rg -n "file_edit_v2" crates/` → **0**；`cargo test -p qaqh-workspace` 绿。

### PR-4-2 `sha256_hex` 单源化（审计 #6）

- 动作：删 `runtime/ringing/content_store.rs:103` 副本，改 `use qaqh_types::sha256_hex;`
  （runtime→types 既有合法边）。
- 验收：`rg -n "fn sha256_hex" crates/` → **1**；content_id 相关测试绿
  （跨进程 content_id 稳定性依赖此函数，行为必须逐字节一致）。

## 7. 决策点登记表（默认值可直接执行，执行前需用户过目一次）

| # | 决策 | 选项 | 默认 |
|---|---|---|---|
| D-1 | proto 收敛方案 | A：溶解并入 domain；B：独立 `qaqh-projection` 保留 | **A** |
| D-2 | responses_api 补总超时 | 是（30min 对齐 message_api）/ 否 | **是** |
| D-3 | connect_timeout 统一值 | 15s / 30s | **30s** |
| D-4 | `confirm_apply.rs:156` 属性 | 生产 / 内联测试（影响 PR-4-1 工作量口径） | **内联测试**（已实证：confirm_apply.rs:114 起 `#[cfg(test)]`，156 行位于测试辅助 `dry_run_v2` 内；审计勘误 C-4） |
| D-5 | 巨型方法/文件塑形（审计 #7/#8） | 纳入本 PLAN / 另立项 | **另立项** |

## 8. 验收总闸与基线登记

**基线表（PR-0-1 填写）**

| 项 | 基线值 | 完工要求 |
|---|---|---|
| `cargo test --workspace` 通过数 | **853 通过 / 0 失败 / 3 忽略**（2026-09-06 实测，124s） | ≥ 基线（删除项均无测试归属，理论持平） |
| cargo check test 目标警告 | ~16 | 0 |
| clippy error / warning | **0 / 320**（诊断首行计数，含 lib/test 重复；去重后 ~150 条独立警告） | 0 / 0（与 check 警告分开计数，口径不同；结构类 lint（如 too_many_arguments）按 D-5 不改签名，用带注释 `#[allow]` 收口） |
| `cargo fmt --all --check` | **clean** | clean |
| `bindings/qaqh` 文件数 | 70 | **70（不变）**——本 PLAN 无 ts 导出面变化 |
| CLI 词表数（`qaqh-workspace list`） | **18**（当前 harness 会话上下文，动态注册） | 不变 |
| crates 数 | 15 | **14**（PR-3-5 后） |

**总闸**：§8 全表达标 + 各 PR 出口 grep 全清 + 人工冒烟（daemon 起停、webUI `/debug/` 会话往返、
三协议流式对话各一次）。

## 9. 勘误表

（执行中登记；格式：日期 | PR | 偏差 | 处置 | 是否回写审计报告）

| 日期 | 位置 | 偏差 | 处置 | 回写审计 |
|---|---|---|---|---|
| 2026-09-06 | PR-3-5 验收命令 | 误用 `rg -rn`：rg 的 `-r` 是 `--replace`（组合解析为「替换为 n」），非 grep 的 recursive；实测 `rg -rn 'hello'` 将 `hello world` 显示为 `n world` | 改为 `rg -n`（rg 默认递归，无需 -r）；全文复查其余 rg 用法（`-n`/`-l`）无误 | 否（起草期笔误，非执行偏差） |
| 2026-09-06 | §0 快照 | 「工作区干净」失实：docs 侧未提交改动，PLAN/审计报告 untracked | 更正表述；PR-0-1 增「先行入库」 | 是（审计勘误 C-1） |
| 2026-09-06 | §5 前提 / §8 基线表 | proto 引用 57 处（40/11/6）→ 实测 59 处（41/12/6，occurrence 口径）；bindings 71 → 实测 70 | 更正数字并注明统计口径 | bindings：是（审计勘误 C-2）；57：否（PLAN 侧统计） |
| 2026-09-06 | PR-3-3 现状 | dashboard.rs「43 行」→ 实测 34 行 | 更正 | 是（审计勘误 C-3） |
| 2026-09-06 | PR-1-2 验收 | 「agent::wire → 仅 tests/common 命中」与迁移成功后的实际结果（0 命中）自相矛盾 | 验收改为 → 0 | 否（起草期笔误） |
| 2026-09-06 | PR-3-2 验收 | `SessionActivityState` 会命中 event.rs:96 legacy 注释（误报）；`SessionActivityChanged` 计数预期 2、实际 ≥4 | 注释随 PR 更新；验收改白名单 4 行 | 否（PLAN 验收口径） |
| 2026-09-06 | PR-4-1 动作 | 漏列 registration.rs:14 `use super::file_edit_v2;` | 补入动作清单 | 是（审计勘误 C-5） |
| 2026-09-06 | §7 D-4 | 可提前实证，无需执行时核查：confirm_apply.rs:114 起 `#[cfg(test)]`，156 行在测试辅助 `dry_run_v2` 内 | 拍板「内联测试」 | 是（审计勘误 C-4） |
| 2026-09-06 | PR-1-3 / PR-3-4 验收命令 | `grep -c` 0 命中时 exit 1 且 per-crate 汇总行膨胀口径；`--test '*'` 需 cargo≥1.65 且冗余 | 改 `rg -c "^warning"`；统一 `cargo test -p qaqh-runtime` | 否（起草期笔误） |
| 2026-09-06 | PR-2-1 / PR-3-1 / R-4 / PR-3-5 | 非偏差补强：chat connect_timeout 15s→30s 标注为用户可感知行为变化；PR-3-1 补「删 proto 侧原定义与 re-export」；R-4 检查落成可执行命令；PR-3-5 验收加 docs/ 范围声明 | 正文就地修订 | 否 |
| 2026-09-06 | PR-1-3 | `unreachable pattern`（execution.rs:403）人工核查：ToolEffect/SkillEffect 均为单变体 enum，`other` 臂被编译器证明不可达，**无隐藏逻辑错误** | 删除 `other` 臂并留注释：单臂 match 已穷尽，未来任一 enum 增变体即编译失败，强制显式处理；另删 3 处 dead_code（persist_effects `INIT` 旧 init 残留 / message_api 测试夹具 `provider()` / subagent `clear_host_for_test()` 无调用方） | 否（PLAN 验收口径） |
| 2026-09-06 | PR-3-5 验收 | 残余 grep 命中 2 行：ringing_architecture.rs 守卫测试的**否定断言**字符串（`contains("qaqh-proto")`）——防回归功能件，删除即拆除架构守卫；另 4 处历史/规则注释已改写消字面量 | 范围声明：守卫测试断言与同文件警示注释不算残留（同类处理先例：docs/ 范围声明）；断言保留且持续为真（该依赖已不可能重建） | 否（PLAN 验收口径） |
| 2026-09-06 | PR-4-2 验收 | 漏网：daemon axum_server.rs:426 引用 runtime 侧旧 pub 路径 `…content_store::sha256_hex`；PR-4-2 验收只跑 `-p qaqh-runtime` 未跑 workspace check，下游破坏晚发现 | axum_server 改用单源 `qaqh_types::sha256_hex`（ad866f5）；教训：单源化类 PR 的出口必须含 `cargo check --workspace --all-targets`，不能只验本 crate | 是（验收纪律补强） |
| 2026-09-06 | §8 总闸 clippy 0/0 | clippy 实测基线 320 条（去重后约 150 独立站点），远超 PR 清单预设；结构性 lint（too_many_arguments 等）修复即 D-5 塑形 | 按 §7 D-5 纪律：结构类 lint 用带注释 `#[allow]` 收口不改签名；风格类 355 处机器建议（字节偏移 + 逐文件编译验证）+ 手工批全部清零（a2055f7）；总闸口径达成 0/0 | 否（PLAN 门槛本就 0/0） |
| 2026-09-06 | §8 总闸执行 | clippy 文本替换批处理两类静默损伤：① byte 偏移误用码点切片（含中文注释文件全量错位）；② 错位片段落入字符串字面量/注释（base_url_preset_guard 测试 TOML、build.rs 注释）——**不破坏编译，cargo check 不可见**，靠全量测试抓获 | 修复两处损伤（a2055f7）；教训：文本替换类自动化的出口必须含 `cargo test --workspace`，仅 check 不够；后续自动化改写须逐文件验证 + 全量测试兑底 | 是（自动化工具纪律） |

## 10. 风险与回退

| # | 风险 | 缓解 | 回退 |
|---|---|---|---|
| R-1 | JSON wire shape 漂移（PR-3-2/3-3） | 重构前先落 shape 快照 roundtrip 测试 | 按 PR revert |
| R-2 | resume/compact 路径回归（PR-3-4） | compact-context 集成测试列为该 PR 必跑项 | 按 PR revert |
| R-3 | daemon.json 兼容性（PR-3-1） | client roundtrip + 旧格式样本解析测试 | 按 PR revert |
| R-4 | workspace→domain 新依赖边被滥用 | 只允许 Dashboard* 三个类型；评审检查 `rg -n "qaqh_domain" crates/qaqh-workspace/src \| rg -v dashboard` → 0，超出即打回 | 边可整体撤销 |
| R-5 | 全局状态测试串行假设被删除项扰动 | 每阶段出口全量 `cargo test --workspace`（含 TEST_RUNTIME_SERIAL 路径） | 按阶段回退 |

跨阶段无共享状态（全是删除 + 移动 + 接口收窄，无数据迁移）；任一阶段中止，已完成阶段即净收益。
