# QAQ-Harness 冗余设计审计报告

| 项 | 值 |
|---|---|
| 日期 | 2026-09-06 |
| 审计对象 | `/home/qaqtamsy/Projects/QAQ-Harness`（main @ `d172ac7`，代码区干净；docs 侧存在未提交改动，见附录 C-1） |
| 范围 | 15 crates，227 个 .rs 文件（约 83k 行）+ bindings/qaqh 70 个 TS 文件 |
| 分析方式 | 子代理独立审计（`audit_redundant_design`）→ 主会话逐项复核 |
| 工具 | codegraph CLI v1.6.0（索引 299 文件 / 5,821 节点 / 20,715 边）、rg、md5sum、diff、cargo check |
| 复核状态 | **10 项发现中 9 项经主会话二次取证确认**（见各项标注）；合理冗余 8 项中 3 项确认、5 项子代理报告 |

## 如何核验本报告

- 每项发现附 **复现命令**，可直接粘贴执行比对输出。
- 标注说明：
  - ✅ **双重复核**：子代理取证 + 主会话独立重跑命令，结论一致；
  - 🔶 **部分复核**：核心结论已确认，个别细节数值未逐字验证（已注明）；
  - ⚠️ **子代理报告**：仅子代理取证，主会话未复核（采信前请自行跑命令）。
- ⚠️ 方法论警示：codegraph 的 `callers` 存在漏报（实例：`RingingTimelineIntentEnvelope` 在 `agent/types.rs:293` 作为枚举载荷被引用但未列入 callers）。因此本报告所有"死代码"结论均以 `rg` 全量文本检索 + `lib.rs` 模块声明表双重确认，**未单独采信 codegraph**。

---

## 结论速览

| # | 严重度 | 问题 | 类型 | 处置 | 复核 |
|---|---|---|---|---|---|
| 1 | P0 | `file_core/` 整目录 1813 行死代码 | 死代码+新旧并存 | 删除 | ✅ |
| 2 | P0 | gate 三份 `GLOBAL_CLIENT` 参数漂移 | 重复代码致配置漂移 | 合并为单构造函数 | ✅ |
| 3 | P0 | `agent/wire.rs` 退役 IPC 化石编入生产 | 死代码+位置错误 | 迁 tests/ 或 cfg(test) | ✅ |
| 4 | P1 | proto↔domain 类型重复 + 3 处机械映射 | 新旧并存+过度拆分 | 两步收敛 | ✅ |
| 5 | P1 | `file_edit_v2.rs` shim 迁移未完成 | 新旧并存 | 改 3 调用点后删 | ✅ |
| 6 | P1 | `sha256_hex` 双实现 | 重复代码 | runtime 改用 types | ✅ |
| 7 | P2 | `handle` 406 行 / `dispatch_ringing_one` 425 行 | 巨型方法 | 按方法族拆分 | ✅ |
| 8 | P2 | exec.rs 2527 / axum_server.rs 2334 / todo.rs 1622 行 | 巨型文件 | 低优先级排期 | ✅ |
| 9 | P2 | `qaqh-types` 依赖 `qaqh-skills`（层次倒置） | 依赖方向错误 | 迁类型或删再导出 | ✅ |
| 10 | P3 | 测试目标 ~16 条编译警告 | 卫生项 | `cargo fix`+人工核查 | ✅ |

---

## 一、应清理的冗余（详情）

### 1.【P0】`file_core/` 整目录已从编译中脱落，1813 行完全死亡 ✅ 双重复核

- **位置**：`crates/qaqh-workspace/src/file_core/`（7 文件，`wc -l` 合计 **1813 行**）
- **类型**：死代码 + 新旧并存（旧版残留）
- **证据**（全部实测）：
  1. `lib.rs`（L5–54）声明了 `edit`、`file_edit_v2`、`file_shared` 等 30+ 模块，**唯独没有 `mod file_core;`**；全仓无 `#[path]` 指向（`rg '#\[path' crates` 零命中）→ **不参与编译**；
  2. 目录内 5 个文件是 `edit/` 同名文件的陈旧分叉：`edit/hunk.rs` 多出 `Overwrite`/`PrependFile`/`AppendFile` 三个 hunk 类型（diff 46 行），`edit/locate.rs` 多出 Overwrite 定位分支（diff 45 行）——`edit/` 是超集且仍在演进（9月2日更新），`file_core/` 停在 8月31日；
  3. `file_core/ledger.rs` 与根级 `file_state.rs` **md5 完全相同**（`1e1ab570e34e3b400622a8b82d94a954`，348 行）——同一份代码物理复制两份；
  4. 其 doc「当前为物理搬迁后权威实现，edit/* 逐步改为 re-export」（`file_core/mod.rs:9`）**已失实**：实际迁移方向反转，`file_edit_v2.rs` shim 现指向 `crate::edit`。
- **影响**：1813 行死代码污染检索与索引（codegraph 仍收录）；失实注释误导维护者；ledger 的 md5 孪生意味着 hash 逻辑修改需改两处且无编译护栏。
- **处置**：**整目录删除**，无任何活跃代码引用，无需改调用方；同时更新 `edit/` 内残留的 `//! split from file_edit_v2.rs` 头注释。
- **复现命令**：
  ```bash
  grep -n "file_core" crates/qaqh-workspace/src/lib.rs          # 应零命中
  md5sum crates/qaqh-workspace/src/file_core/ledger.rs crates/qaqh-workspace/src/file_state.rs
  find crates/qaqh-workspace/src/file_core -name "*.rs" | xargs wc -l | tail -1
  diff crates/qaqh-workspace/src/file_core/hunk.rs crates/qaqh-workspace/src/edit/hunk.rs
  ls -la crates/qaqh-workspace/src/file_core/                   # 时间戳 8/31 vs edit/ 9/2
  ```

### 2.【P0】gate 三个适配器各自手写 `GLOBAL_CLIENT`，参数已漂移 ✅ 双重复核（chat 侧 pool/timeout 两行细节🔶）

- **位置**：
  - `crates/qaqh-gate/src/chat_completions_api.rs:108-119`
  - `crates/qaqh-gate/src/message_api.rs:61-70`
  - `crates/qaqh-gate/src/responses_api.rs:53-59`
- **类型**：重复代码（3 份近似 `LazyLock<Client>` 构造）→ 复制后各自漂移
- **证据**（主会话逐文件核实）：

  | 参数 | chat_completions | message_api | responses_api |
  |---|---|---|---|
  | connect_timeout | **15s** | 30s | 30s |
  | tcp_keepalive | 60s | 60s | **缺失** |
  | pool_idle_timeout | 120s🔶 | 120s | **缺失** |
  | 总 timeout | 30min🔶 | 30min | **缺失** |

  **主会话补充发现**：`responses_api` 连总超时都没有——reqwest 默认无总超时，Responses 协议的流若中途僵死，请求可能**永久挂起**，而另两个协议有 30min 兜底。已接近 bug，建议提级。
- **影响**：connect_timeout 15s vs 30s 属行为分歧（哪份是意图不可知）；同一 bug 需修三处。
- **处置**：合并为 `lib.rs`/`types.rs` 单一构造函数，差异参数显式入参。**需人工确认**：① responses 缺 keepalive/pool/timeout 是疏忽还是有意；② connect_timeout 统一取 15s 还是 30s（主会话建议 30s，弱网更稳）。
- **复现命令**：
  ```bash
  grep -n -A8 "GLOBAL_CLIENT: " crates/qaqh-gate/src/{chat_completions_api,message_api,responses_api}.rs
  ```

### 3.【P0】`agent/wire.rs` 测试化石编入生产二进制 ✅ 双重复核

- **位置**：`crates/qaqh-runtime/src/agent/wire.rs`；声明于 `agent/mod.rs:64`（**无条件的 `pub mod wire;`**）
- **类型**：死代码（退役机制残留）+ 位置错误
- **证据**：doc 自述「生产路径是 `Loop::from_channels`，不经过行解析；本模块仅服务于测试 harness 模拟旧管道输入（`Loop::new_ipc` 已退役）」。全仓唯一使用方：`crates/qaqh-runtime/tests/common/mod.rs:12`（`use qaqh_runtime::agent::wire::read_worker_command_frame;`）。
- **影响**：测试辅助被编入 release（`opt-level=z` 仍占体积）；退役历史包袱留在 pub API 面。
- **处置**：迁移至 `tests/common/` 或加 `#[cfg(test)]`（后者需同步从 `agent/mod.rs` pub 导出摘除）。
- **复现命令**：
  ```bash
  grep -n "wire" crates/qaqh-runtime/src/agent/mod.rs
  grep -rn "agent::wire" crates/qaqh-runtime/tests/
  head -8 crates/qaqh-runtime/src/agent/wire.rs
  ```

### 4.【P1】`qaqh-proto` 与 `qaqh-domain` 类型级重复 + runtime 三处 1:1 机械映射层 ✅ 双重复核（映射点行号🔶）

- **位置**：
  - `qaqh-proto/src/agent_protocol.rs:17` `SessionActivityState` vs `qaqh-domain/src/event.rs:100` `ActivityState` —— **变体集合完全相同**（Starting/Idle/Working/WaitingUser/Disconnected），domain 侧注释自认「与 legacy `SessionActivityState` 同义，domain 化」（event.rs:96，主会话已核实原文）；
  - `agent_protocol.rs:32` `SessionActivity{seed,state,turn_id,seq,…}` vs `event.rs:527` `ControlEvent::SessionActivityChanged` 载荷 —— 字段逐一相同；
  - 映射点① `qaqh-runtime/src/activity.rs:15-21`（5 行 match 逐变体翻译）；② `agent/dashboard.rs`（全文件 34 行，workspace proto::DocInfo/TaskInfo 逐字段抄进 domain::DashboardDocument/DashboardTask）；③ `ringing/projection.rs`（15 处 proto 引用）产出 `proto::TurnData/RoundBlock` 再由 `ringing/timeline_rebuild.rs:37` 重放进 domain `TimelineAppender`。
- **旁证**：`qaqh-ringing/src/lib.rs:15` 明文「本 crate 依赖 `qaqh-domain`，**不得**依赖 `qaqh-proto`（legacy）」——团队本就视 proto 为待消化对象（主会话已核实原文）；`qaqh-proto` 共 261 行、无 ts-rs 导出（70 个 TS bindings 中不含 proto 类型）、**proto 类型从不到达前端，是纯内部中间层**。
- **影响**：新增一种会话状态/dashboard 字段需同步改 proto 定义、映射 match、domain 定义三处；穷举 match 只在变体级防漂移，字段级漂移（如 turn_id 语义）编译器无法拦截。
- **处置**（涉及架构边界，**整体待人工确认**）：
  1. 低风险先行：`activity.rs` 内部状态直接用 `domain::ActivityState` + `SessionActivityChanged` 载荷，删 `proto::SessionActivity/SessionActivityState`；dashboard 让 workspace 直接产出 domain 模型（domain 仅依赖 types，workspace→domain 无环）。
  2. 待决策：proto 收敛为「真投影」（`TurnData` 聚合模型与 domain 事件形状确实不同，有存在理由）+ `DaemonDiscovery`（或下放 daemon crate），其余类型别名/删除。仓内已有单源先例：`qaqh-ringing/src/content.rs` 用 `pub use qaqh_domain::ContentRef as RingingContentRef;`。
  3. 收缩 proto 前先更新 `docs/PLAN.md` 边界整改记录（msgloop→runtime 合并的方法论可复用）。
- **复现命令**：
  ```bash
  sed -n '17,40p' crates/qaqh-proto/src/agent_protocol.rs
  sed -n '95,110p' crates/qaqh-domain/src/event.rs        # 含「与 legacy 同义」注释
  grep -n "不得.*qaqh-proto" crates/qaqh-ringing/src/lib.rs
  ls bindings/qaqh/ | grep -ci proto                      # 应为 0
  sed -n '15,21p' crates/qaqh-runtime/src/activity.rs
  ```

### 5.【P1】`file_edit_v2.rs` 废弃 shim 仍有生产调用点 ✅ 双重复核

- **位置**：`crates/qaqh-workspace/src/file_edit_v2.rs`（7 行 shim：`pub use crate::edit::*;`，自注「After verification, delete this file and update callers to `crate::edit`」）
- **证据**（主会话全量 grep 核实）：
  - 生产调用：`registration.rs:14`（`use super::file_edit_v2;`）与 `registration.rs:39`（`file_edit_v2::register(&mut mgr)`）、`confirm_apply.rs:51`（`"edit" => crate::file_edit_v2::exec_edit(...)`）；
  - 测试调用：`confirm_apply.rs:156`（**已确认**：位于 :114 起 `#[cfg(test)] mod tests` 的测试辅助 `dry_run_v2` 内）、`file_query.rs:520,551`（:317 起 `#[cfg(test)]`）；
  - `edit/mod.rs:58-70` 为兼容 shim 保留「核心重导出」脚手架（注释明言「保持 crate::edit::* 兼容 file_edit_v2 的旧导入」）。
- **影响**：双层间接（调用方→shim→edit）；兼容重导出阻碍 `edit` API 收敛。
- **处置**：改 3–5 处调用为 `crate::edit::` → 删 `file_edit_v2.rs` → 删 `edit/mod.rs:58-70` 兼容段。约 30 分钟，低风险。
- **复现命令**：
  ```bash
  cat crates/qaqh-workspace/src/file_edit_v2.rs
  grep -rn "file_edit_v2" crates/qaqh-workspace/src --include="*.rs" | grep -v "^.*edit/"
  sed -n '58,70p' crates/qaqh-workspace/src/edit/mod.rs
  ```

### 6.【P1】`sha256_hex` 双实现 ✅ 双重复核（逐字节同形比对🔶）

- **位置**：`qaqh-types/src/image_store.rs:60` 与 `qaqh-runtime/src/ringing/content_store.rs:103`
- **证据**：两处 `pub fn sha256_hex(data: &[u8]) -> String`（主会话 grep 确认位置）；types 侧注释自述「与 runtime content_store 同形；types 层自持一份，避免 types→runtime 反向依赖」——types 持副本理由成立，但 **runtime 依赖 types 是既有合法边**，runtime 侧副本无存在必要。
- **影响**：小（8 行×2），但 content_id 跨进程稳定性依赖两份实现永远同步，无编译护栏。
- **处置**：删 `content_store.rs:103` 实现，改 `use qaqh_types::sha256_hex;`。15 分钟。
- **复现命令**：
  ```bash
  grep -rn "fn sha256_hex" crates/ | grep -v test
  sed -n '55,70p' crates/qaqh-types/src/image_store.rs
  ```

### 7.【P2】runtime 两个核心分发点超 400 行 ✅ 双重复核

- **位置与实测**（主会话以相邻函数行号锚定）：
  - `qaqh-runtime/src/agent/loop_core.rs:1272` `dispatch_ringing_one` → 下一函数 `apply_outcome` 位于 1697 → **425 行**（loop_core.rs 共 1901 行，占 **22.4%**）；
  - `qaqh-runtime/src/service.rs:180` `handle` → 下一函数 `shutdown` 位于 587 → **约 406 行**，内含约 90 个唯一方法名分支🔶（分支计数未复核）；service.rs 共 1217 行。
- **类型**：巨型方法（可维护性风险，非死代码）。
- **影响**：命令新增只能继续膨胀 match；review diff 粒度差；分支间共享局部变量形成隐式耦合。
- **处置**：`handle` 按 control/conversation/tool/session 方法族拆 4 个 `handle_*` + match 表头（与 workspace `registration.rs` 注册表风格对齐）；`dispatch_ringing_one` 按命令类型拆。纯重构，每拆一块跑一次 `just test`。拆分粒度**待人工确认**。
- **复现命令**：
  ```bash
  grep -n "pub fn handle\|pub fn shutdown" crates/qaqh-runtime/src/service.rs
  grep -n "fn dispatch_ringing_one\|fn apply_outcome" crates/qaqh-runtime/src/agent/loop_core.rs
  wc -l crates/qaqh-runtime/src/service.rs crates/qaqh-runtime/src/agent/loop_core.rs
  ```

### 8.【P2】三个 2000+ 行巨型文件 ✅ 双重复核（文件内函数级数据🔶）

- **实测行数**：`qaqh-workspace/src/exec.rs` **2527**、`qaqh-daemon/src/axum_server.rs` **2334**（18 个 `route(`，主会话此前已独立核实路由面）、`qaqh-workspace/src/todo.rs` **1622**。
- **子代理补充**🔶：exec.rs 最大函数 `direct_exec` 287 行、`handle_run_with_shell` 191 行，含 4 处 `#[allow(dead_code)]`（抽样核实为平台条件编译豁免，合法）。
- **处置**：低优先级。exec.rs 可按「进程spawn/管道排空/平台胶水/超时控制」切目录；axum_server.rs 按 ringing 路由族拆 handler 子模块。与 #7 一并排期。
- **复现命令**：
  ```bash
  wc -l crates/qaqh-workspace/src/exec.rs crates/qaqh-daemon/src/axum_server.rs crates/qaqh-workspace/src/todo.rs
  grep -c "\.route(" crates/qaqh-daemon/src/axum_server.rs
  ```

### 9.【P2】`qaqh-types` 依赖 `qaqh-skills` 仅为再导出（依赖方向倒置） ✅ 双重复核

- **位置**：`crates/qaqh-types/Cargo.toml:8`（`qaqh-skills = { path = "../qaqh-skills" }`）；`qaqh-types/src/session.rs:3`（`pub use qaqh_skills::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};`）
- **证据**：真实定义在 `qaqh-skills/src/session_state.rs`；types 是**基础类型层**却依赖**上层功能 crate**，方向倒置。消费方经 types 间接引用（runtime/state/agent.rs、session/manager.rs）。
- **处置**（**待人工确认**，二选一）：
  - 方案 A（更干净）：`session_state.rs` 迁入 `qaqh-types/src/session.rs`，skills 改依赖 types（叶子地位不受影响），触及约 5 个文件 import；
  - 方案 B（最小改动）：删 types 再导出，消费方直接 `use qaqh_skills::`。
- **复现命令**：
  ```bash
  grep -n "qaqh-skills" crates/qaqh-types/Cargo.toml
  grep -n "qaqh_skills" crates/qaqh-types/src/session.rs
  grep -rn "SkillSessionEntry" crates --include="*.rs" -l
  ```

### 10.【P3】测试目标编译警告 ~16 条 ✅ 双重复核

- **实测**（`cargo check --workspace --all-targets`，主会话重跑）：
  - `4× unused variable: other`、`4× unused variable: dir`、`1× variable does not need to be mutable`、`1× unused variable: path`、`1× unused variable: ops`、`1× unused import: Path`、**`1× unreachable pattern`（需人工核查，可能隐藏逻辑错误）**、`1× static INIT is never used`、`1× function provider is never used`、`1× clear_host_for_test is never used`；
  - 分布：workspace lib test 10 条、message persist_effects 2 条、gate lib test 2 条、runtime 1 条、subagent 1 条。
- **处置**：`cargo fix --workspace --all-targets --tests` 清理机械项 + **人工核查 `unreachable pattern`**。
- **复现命令**：
  ```bash
  cargo check --workspace --all-targets 2>&1 | grep -E "^warning" | sort | uniq -c | sort -rn
  ```

---

## 二、有意的合理冗余（建议保留）

| # | 位置 | 结论 | 复核 |
|---|---|---|---|
| 1 | `qaqh-config-api`（单文件 406 行） | **保留**。非过度拆分：doc 明载 K1 约束（叶子 crate 只依赖 serde），winui/ratatui/web 三端共享唯一真相；`ConfigPatch` = RFC 7386 Merge Patch 且防「整包写回毒化」（2026-08-25 事故 R5）。小是契约层优点。可补一句「勿添加引擎依赖」的维护者警告 | ✅（doc 原文已核实） |
| 2 | `qaqh-config/src/dto.rs`（296 行穷举字段映射） | **保留**。有意不用 `..Default::default()` 兜底，穷举字面量使「新增字段未同步映射」变成编译错误——教科书式反 DRY 护栏 | 🔶（config-api 侧穷举纪律 doc 已核实，dto.rs 本体未开） |
| 3 | gate `sse.rs`(161行) vs client `sse_decoder.rs`(188行) | **保留**。实测相似度 49.9%🔶：帧语义不同——gate 版聚合 `data:` 且 `event:` 行冲刷（上游 LLM 流）；client 版产出 `SseFrame{id,event_type,data}` 仅空行定帧（daemon 协议需 id 做 cursor）。分属不同信任域（不可信上游 vs 本机 daemon），各 crate 内部均单源。若未来出现第三消费方再下沉共享层。建议两文件头互加「与对方语义差异」注释防误合 | 🔶（两个 SseDecoder 定义位置已核实；相似度数值未复算） |
| 4 | `edit/`(4376行) vs `apply_patch_engine/`+`apply_patch.rs` | **保留**。`apply_patch.rs:1-12` 明文分工：edit=结构化 hunk、内容锚定、严格拒绝歧义、两阶段全事务；apply_patch=Codex 格式、无行号、四级匹配、按序应用（移植自 codex-rs，Apache-2.0）。**暴露给模型的两个不同工具**，非新旧版本 | ✅（doc 原文已核实） |
| 5 | client `sse.rs`+`sse_decoder.rs`+两处 `drain_frames` | **保留**。decoder 已单源化（旧 O(n²) 实现已删）；现存两个 `drain_frames` 是消费端（event vs timeline 通道），相似度仅 38.6%🔶，校验逻辑不同（reset_required vs Ringing schema/epoch/seed/cursor） | ⚠️ |
| 6 | `RingingContentRef` = `pub use qaqh_domain::ContentRef` | **保留**。跨层单源别名零复制，且是 #4 proto 收敛时可复用的先例模式 | ⚠️ |
| 7 | `tool_side_fold.rs` + message/store 终态化注释 | **保留**。`store.rs:11-14` 明载 shaping 已移至 tool 侧、存储即终态 | ⚠️ |
| 8 | exec.rs/secrets.rs 等 `#[allow(dead_code)]` | **保留**。抽样核实均为 `cfg_attr(not(windows/test),…)` 类平台条件编译豁免 | 🔶 |

## 三、已排除的误报（核查后确认不是冗余）

> 以下均为子代理排查结论（⚠️ 未主会话复核，但排除逻辑自洽）：

| 疑似项 | 排除理由 |
|---|---|
| `qaqh-message`（状态机存储）vs `types::message.rs` | 名字近、职责完全不同（后者是 OpenAI DTO） |
| `workspace::dashboard.rs` vs `agent/dashboard.rs` | 投影与域映射分层（但该链路本身是 #4 的收敛对象） |
| `file_shared.rs` vs `file_state.rs` | 前者是共享原语（hash/normalize/atomic_write），后者是 `<file_state>` 环境注入账本，被 grep_tool/file_query/edit 广泛复用 |

---

## 四、待人工确认清单

1. **#2**：responses_api 的 GLOBAL_CLIENT 缺 keepalive/pool/总超时——疏忽还是有意？connect_timeout 统一 15s 还是 30s？
2. **#4**：proto 收敛策略——并入 domain vs 保留为「纯投影层」（TurnData + DaemonDiscovery）？
3. **#5**：`confirm_apply.rs:156` 是否位于内联测试模块（影响迁移工作量）？
4. **#9**：`session_state.rs` 迁 types（方案 A）vs 删再导出（方案 B）？
5. **#7/#8**：巨型 match/文件拆分的目标粒度与排期意愿？

## 五、风险提示

1. **#1 删除 `file_core/`**：本就不在编译图内，删后跑 `cargo check --workspace` 应零新增错误（验证性检查）；建议独立 commit 便于反悔。
2. **#4 proto 收敛**触及 ringing lib.rs 的 Domain/Projection/Wire/Transport 四层约定，先改 `docs/PLAN.md` 再动手（团队有既定迁移方法论）。
3. **#7/#8 重构**前确认测试覆盖：`loop_core` 行为回归目前主要依赖 integration tests；每步小拆 + `just test`。
4. 本报告基于 `d172ac7` 快照；若 HEAD 前进需重跑关键复现命令。

## 六、处置工作量汇总

| 优先级 | 项 | 处置 | 预估 |
|---|---|---|---|
| P0 | #1 file_core 死目录 | 删除 | 10min |
| P0 | #2 GLOBAL_CLIENT 漂移 | 合并单构造 + 补 responses 超时 | 1h |
| P0 | #3 wire.rs 化石 | 迁 tests/ 或 cfg(test) | 30min |
| P1 | #5 edit shim | 改调用点后删 | 30min |
| P1 | #6 sha256_hex | 改用 types 版 | 15min |
| P1 | #4 proto↔domain | 两步收敛（待决策） | 0.5–2 天 |
| P2 | #7/#8 巨型方法/文件 | 排期重构 | 各 0.5–1 天 |
| P2 | #9 types→skills | 迁类型或删再导出（待决策） | 1h |
| P3 | #10 警告卫生 | cargo fix + 人工核查 | 30min |

---

## 附录 A：核验命令速查（一键粗验）

```bash
cd /home/qaqtamsy/Projects/QAQ-Harness

# 1. file_core 死目录
grep -c "file_core" crates/qaqh-workspace/src/lib.rs; md5sum crates/qaqh-workspace/src/file_core/ledger.rs crates/qaqh-workspace/src/file_state.rs

# 2. GLOBAL_CLIENT 三份
grep -n -A8 "GLOBAL_CLIENT: " crates/qaqh-gate/src/{chat_completions_api,message_api,responses_api}.rs

# 3. wire.rs
grep -n "pub mod wire" crates/qaqh-runtime/src/agent/mod.rs; grep -rn "agent::wire" crates/qaqh-runtime/tests/

# 4. proto/domain 重复
sed -n '17,30p' crates/qaqh-proto/src/agent_protocol.rs; sed -n '96,108p' crates/qaqh-domain/src/event.rs

# 5. edit shim
cat crates/qaqh-workspace/src/file_edit_v2.rs; grep -rn "file_edit_v2::" crates/qaqh-workspace/src --include="*.rs" | grep -v edit/

# 6. sha256_hex
grep -rn "fn sha256_hex" crates/ | grep -v test

# 7/8. 巨型方法/文件
grep -n "fn dispatch_ringing_one\|fn apply_outcome" crates/qaqh-runtime/src/agent/loop_core.rs
wc -l crates/qaqh-workspace/src/exec.rs crates/qaqh-daemon/src/axum_server.rs crates/qaqh-workspace/src/todo.rs

# 9. types→skills
grep -n "qaqh-skills" crates/qaqh-types/Cargo.toml; sed -n '1,5p' crates/qaqh-types/src/session.rs

# 10. 警告
cargo check --workspace --all-targets 2>&1 | grep -cE "^warning"
```

## 附录 B：分析过程记录

- 子代理 `audit_redundant_design`（seed `36339a60`）独立完成首轮审计，明确报告 codegraph `callers` 漏报问题并降级为 rg 验证；
- 主会话两轮复核：第一轮抽验 5 项 P0/P1 核心（file_core md5、GLOBAL_CLIENT 三份、wire.rs、sha256_hex、types→skills）全部证实；第二轮补验 5 项（file_edit_v2 调用点全量 grep、handle/dispatch_ringing_one 行号锚定、三巨型文件 wc、proto/domain 枚举逐变体比对、cargo check 警告清单 + 合理冗余 doc 原文）全部证实；
- 报告中唯一"主会话补充发现"：responses_api 缺总超时（接近 bug，已建议提级）。

## 附录 C：复核勘误（2026-09-06，PLAN review 期间实证回写）

| # | 位置 | 原文 | 复核结果 | 修正 |
|---|---|---|---|---|
| C-1 | 元信息「审计对象」 | 工作区干净 | `git status`：docs 侧未提交改动（boundary-reform-plan.md 重命名、PLAN/审计报告 untracked），代码区干净 | 已改为「代码区干净 + docs 未提交注记」 |
| C-2 | 元信息「范围」、§4 旁证 | bindings/qaqh 71 个 TS 文件 | `find bindings/qaqh -type f` 统计 = 70（全为 .ts） | 已改 70 |
| C-3 | §5 映射点② | agent/dashboard.rs 全文件 43 行 | `wc -l` = 34 | 已改 34 |
| C-4 | §6 证据 | confirm_apply.rs:156 疑似内联测试，需人工确认 | :114 起 `#[cfg(test)] mod tests`，156 行位于测试辅助 `dry_run_v2` 内，确为内联测试 | 已确认（PLAN §7 D-4 据此拍板） |
| C-5 | §6 证据 | 调用点清单缺 `registration.rs:14` 的 `use super::file_edit_v2;` | 全量 grep 补齐：生产 3 行（2 文件）+ 测试 3 行（2 文件），共 6 处 | 已补 |
