# qaqh-mcp 实施计划（MCP 客户端支持 PLAN）

> 状态：**制定完毕，待 owner 批准 M0 开工** · 2026-09-07 · 制定者：Omen Alpha（QAQ-Harness Agent）
> 上游文档：`docs/mcp-client-design.md`（设计权威来源，含 D1–D4 已拍板决策与本次评审后的勘误任务）
> 方法论：继承前任 PLAN（冗余收敛执行计划，已归档）——**提案 → PLAN → 分阶段 PR →
> 可命令化验收 → 独立回退**。执行中发现与设计文档不符，先改设计文档再改本 PLAN。
> 评审方式说明：原计划由 4 个子代理并行评审（架构/并发/安全/可测性），子代理在当前
> harness 会话中被取消（exit 0，无产出），评审由主会话以同一四角度自行完成，结论已回写。

---

## 0. 执行上下文（2026-09-07 实测快照）

- 分支 `main`；设计稿 `docs/mcp-client-design.md`（268 行 + 评审回写 2 处）；旧 PLAN 已归档至回收站。
- 工具链实测：Rust 1.98.0（edition 2024，≥ rmcp MSRV 1.88 ✅）、Node v26.8.1（stdio server 宿主 ✅）。
- rmcp 官方 SDK：3.2.0（2026-08-31），实现 spec `2026-07-28`，crates.io 总下载 2463 万。
- 本次评审关键源码锚点：`qaqh-workspace/src/lib.rs:699-726`（ToolHandler.fn 指针）、
  `manager.rs`（三阶段 PreparedCall / inflight_tasks）、`backend.rs`（回退语义先例）、
  `qaqh-runtime/src/{service.rs:24（QaqhService）, registry.rs:146（AgentRegistry）, actor.rs:111（子代理沙箱旗标）}`、
  `docs/cancel-contract.md`（三层取消信号 + 评审红线）、`qaqh-config/src/secrets.rs`（per-slot 仓库）。

## 1. 评审结论摘要（四角度，详见 §2 勘误表）

| 角度 | 总评 | 关键发现 |
|---|---|---|
| 架构/边界 | 小改 | 单一 dispatcher（前缀解析）优于 per-server 静态 dispatcher；依赖方向需在设计 §4 画明（runtime → qaqh-mcp 单向） |
| 并发/桥接 | **大改前必补契约对齐** | 取消契约三层模型未对齐（红线：禁 `set_cancel(false)`；L3 必须 killpg/process group）；idle 判定需"无 inflight"语义 |
| 安全/权限 | 小改但有一处必改 | D4 免审批与 MaxLockdown 档位冲突（Level 1/2 必须仍走审批）；`${secret:}` 插值的底层（通用 secret store）不存在，需扩展 secrets.toml |
| 可测性 | 小改 | feature 名与仓库惯例核对通过；验收命令化（出口命令）必须补齐；孤儿进程测试需双平台脚本 |

## 2. 设计勘误任务（PR-M0-0，先改设计再写码）

| # | 勘误 | 严重度 | 落点 |
|---|---|---|---|
| E-1 | **取消契约对齐**：§5.2 桥接只读 `ctx.cancel`（Arc<AtomicBool>，L1/L2 已接线），禁止触碰全局 `CANCEL`；退出路径禁用 `set_cancel(false)` 字面调用（契约红线1）；MCP server 子进程 spawn **必须设独立进程组**（契约 L3：Unix killpg / Windows taskkill /T）——M0 需实测 rmcp `transport-child-process` 是否已启用 process-wrap 进程组，未启用则在 adapter 层补 | 高 | 设计 §5.2 增补"契约对齐"小节 |
| E-2 | **idle 语义精确化**：idle 判定 = `inflight == 0` 连续 `idle_shutdown_secs`（默认 300s），原子计数器实现；否决"无调用即计时"（会误杀慢响应活跃连接） | 中 | 设计 §5.1 表格修订 |
| E-3 | ~~D4 与权限档位交互~~ → **D5 已拍板（owner 2026-09-07）**：全档位默认放行 MCP，临时豁免登记设计 §5.5；勘误动作改为“落 D5 行 + 登记临时豁免窗口”（原分档方案作废，随 workspace 隔离权限重构另立 RFC） | 高 | 设计 §5.5 + §1 D5 行 |
| E-4 | **secrets 扩展**：`${secret:name}` 依赖通用命名 secret 存储——现 `secrets.rs` 为 per-slot（Main/Subagent）API key 仓库，需扩展 `secrets.toml` 增加通用 map 段（沿 DPAPI/0600 既有机制，Windows 加密、其余 0600）；拒绝在 config.toml 明文 env（重蹈审计 P0-1 覆辙） | 高 | 设计 §6 修订 + secrets.rs 扩展任务（PR-M1-3） |
| E-5 | **dispatcher 收敛**：放弃"每 server 一个静态 dispatcher fn"，改为**单一 dispatcher fn + `mcp__` 前缀解析路由**（fn 指针无状态、永不失效，天然满足 PreparedCall 在飞安全） | 中 | 设计 §5.2 修订 |
| E-6 | **stderr 处理**：MCP server stderr 默认丢弃（计数留痕），可配置捕获但**必须过脱敏钩子**（env 值/secret 值出现在 stderr 不得进 ToolResult/审计/模型上下文） | 中 | 设计 §5.5 增补 |
| E-7 | **依赖方向图**：§4 增加依赖箭头说明——`qaqh-runtime` → `qaqh-mcp` → `{qaqh-workspace, qaqh-config, rmcp}`，qaqh-mcp 不反向依赖 runtime/msgloop；refresh 由 runtime 调用 | 中 | 设计 §4 增补 |
| E-8 | **prompt injection 接受风险登记**：MCP 工具结果/resource 内容直通模型上下文为协议固有面；防线=既有 JSON 包裹 + 输出截断 + E-6 脱敏；在 §10 显式登记为接受风险 | 低 | 设计 §10 增补 |

## 3. 阶段总览与 PR 切分

| 阶段 | 主题 | PR | 风险 | 依赖 |
|---|---|---|---|---|
| Phase M0 | Spike：SDK 可行性验证 + 设计勘误 | PR-M0-0 / PR-M0-1 | 低 | — |
| Phase M1 | 核心链路：配置→生命周期→投影→调用 | PR-M1-1 … M1-5 | 中 | M0 |
| Phase M2 | Resources 接入 | PR-M2-1 / M2-2 | 低 | M1 |
| Phase M3 | 传输扩展与打磨 + 配置兼容 | PR-M3-1 … M3-4 | 低 | M1 |

- 顺序纪律：**勘误先行（E-1/E-3 是安全语义，不可带病开工）→ spike 验证 → 核心链路**。
- 每阶段独立可回退：qaqh-mcp 全部为新 crate，回退 = workspace members 移除 + 接触面 3 处还原。

## 4. 分阶段任务与验收

### Phase M0 — Spike + 勘误（预估 0.5–1 天）

| PR | 任务 | 出口命令（可命令化验收） |
|---|---|---|
| PR-M0-0 | 落实 §2 全部勘误（E-1…E-8）到设计文档 | `rg -c "取消契约|inflight == 0|全档位默认放行|\[secrets\.mcp\]" docs/mcp-client-design.md` ≥ 4 处命中 |
| PR-M0-1 | `crates/qaqh-mcp` 骨架：`default-features=false` 编译通过；in-memory（dev transport-io）连 mock server（rmcp ServerHandler）打通 initialize→tools/list；**实测 transport-child-process 的进程组行为并记录** | `cargo check -p qaqh-mcp --features qaqh-mcp/test-util` 0 error；`cargo test -p qaqh-mcp --test m0_spike` 通过（含 1 条"connect + list_tools"用例）；`docs/` 附录记录进程组实测结论 |

### Phase M1 — 核心链路（预估 2–3 天）

| PR | 任务 | 出口命令 |
|---|---|---|
| PR-M1-1 | config：`[mcp]`/`[mcp.servers.*]` 解析 + fail-fast 校验（server 名 `[a-z0-9_-]+`、重名拒绝、command 非空）+ config-api 只读 DTO | `cargo test -p qaqh-config --test mcp_config` 全绿（含重名/非法字符/缺 command 用例） |
| PR-M1-2 | McpManager + connection actor：lazy connect（10s 超时）、inflight==0 idle 回收、重连冷却 5s、`shutting_down` 闸、RAII reap（killpg/taskkill /T） | `cargo test -p qaqh-mcp --test lifecycle` 全绿（connect/timeout/crash-reconnect/idle/shutdown 五用例，in-memory + 子进程各一） |
| PR-M1-3 | secrets 通用 map 扩展（E-4）+ env 插值 + 脱敏 | `cargo test -p qaqh-config --test secrets_mcp` 全绿（插值/缺失 fail-fast/DPAPI 往返[win]） |
| PR-M1-4 | 工具投影：单一 dispatcher（E-5）、`mcp__{server}__{tool}` 命名、`ToolManager::register_dynamic`（仅回合边界）、模型面合并、description 截断 2KB、碰撞拒绝 | `cargo test -p qaqh-workspace --test dynamic_registration` 全绿（前缀/碰撞/白名单过滤/截断） |
| **PR-M1-5 ✅**（2026-09-07 收口） | 调用路径：bridge（专属 runtime + mpsc + 250ms cancel 轮询、超时链 60s→3600s 封顶）、错误码 §7 全量映射（+`MCP_BUSY`/`MCP_SHUTDOWN` 闸门码，见设计 §7 补行）、审计经既有 execute_authorized→audit 通道免费接入（tool/args_hash/elapsed/success；env 永不落）、ToolStats 经 finalize_req 累计；D5 豁免落 authorization.admit（mcp__ 前缀全档位直通 + 子代理沙箱显式拒绝）；回合边界 refresh 钩子挂 engine_turn::run_lap（take_projection_batch→replace_dynamic_tools→tool_defs 重建）；QaqhService::init 装配 McpManager；daemon main 收尾 shutdown_global；**修复 rmcp graceful_shutdown 孙进程漏杀缺口**（adapter 登记 pgid + connection.sweep_group 组杀兜底，orphan_reap 验证三层树全灭） | `cargo test -p qaqh-mcp --test call_path --test orphan_reap` 全绿（7+3）；`rg -n "set_cancel\\(false\\)" crates/qaqh-mcp/src` 零命中 ✓；workspace 总闸 **917 passed / 0 failed**（基线 904 → +13）、clippy 0 警告、fmt clean |

### Phase M2 — Resources（预估 1–2 天）

| PR | 任务 | 出口命令 |
|---|---|---|
| **PR-M2-1 ✅**（2026-09-07） | `mcp` 聚合工具（list_servers/list_resources/read_resource）+ blob 占位 `[blob mime=… size=… uri=…]` + 资源模板展开提示；**无前缀名**（不经 D5 快路径，category=Read 常规审批）；**批次钉底**（enabled 即在场，零 server 配置也可答）；connection +resources/templates 缓存 + read_resource（同款保障：inflight 占用/超时硬顶/断连→crash） | `cargo test -p qaqh-mcp --test resources` 全绿（17 用例，含 env block 渲染/封顶） |
| **PR-M2-2 ✅**（2026-09-07） | 资源清单注入块（封顶 20 条/120 字符）经 **ContextFlow trailing developer 消息**（skills 同管线，内容门控——变更才物化，prefix cache 稳定；落点 run_lap 回合边界）+ **tools/resources list_changed 订阅**（adapter NotifyBridge 客户端 handler → CONN_NOTIFY Weak 表 → 重拉+置脏）+ **观察项①修复**（allowed_raw + 动态层重建后 reapply） | `cargo test -p qaqh-runtime --test mcp_env_block` 全绿（2）；`cargo test -p qaqh-workspace --test dynamic_registration` 全绿（7） |

### Phase M3 — 扩展与打磨（预估 1–2 天）

| PR | 任务 | 出口命令 |
|---|---|---|
| **PR-M3-1 ✅**（2026-09-07） | streamable HTTP client（reqwest 0.13 对齐 + headers 进 default_headers，${secret} 插值生效；无子进程——pgid/sweep no-op；crash 语义同构） | `cargo test -p qaqh-mcp --test http_transport` 全绿（3：round_trip/resource_read/unreachable→ConnectFailed） |
| **PR-M3-2 ✅**（2026-09-07） | **Codex/Claude Code MCP 配置兼容**（A+B 均落地）——A：`QaqhService::init` 用户级只读合并（`~/.codex/config.toml` `[mcp_servers]` + `~/.claude.json` `mcpServers`，`ext-<src>-` 前缀避撞、本地优先、明文 env 直通子进程不落 QAQH secrets、不回写；开关 `[mcp].import_external` 默认开）；B：`qaqh-daemon mcp import --from codex|claude|claude-project`（dry-run 默认 + `--exec` 写入；**项目级 `.mcp.json` 需 `--root` + 逐 server 交互审批**——供应链面，D4 不扩；env 占位符化 `mcp-<server>-<key>` 入 [secrets.mcp]） | `cargo test -p qaqh-config --test mcp_import` 全绿（8：扫描/合并/碰撞/开关/缺文件/导入占位化/审批拒绝/D4 结构守卫） |
| **PR-M3-3 ✅**（2026-09-07） | E2E 门控测试（`QAQH_MCP_E2E=1` 连真实 `npx @upstash/context7-mcp`）——断言结构不变量（≥1 工具/前缀/批次钉底），不锁上游清单；默认 skip，缺 env 打印原因即返回 | `QAQH_MCP_E2E=1 cargo test -p qaqh-mcp --test e2e_context7` **实测通过（1.35s，本机 npx 缓存）；缺 env 默认 skip ✓** |
| **PR-M3-4 ✅**（2026-09-07） | 文档：`docs/mcp-client-design.md` 状态改 **Phase 1 已实施**（含实施 commit 记录 + S1 传输行更新）；PLAN M3 全表打勾 | 见 §5 总闸（本轮：948 passed / 0 failed、clippy 0、fmt clean、红线零命中） |

## 4b. Phase 2（2026-09-07 增补；owner 优先级：热重载 > 小项 > prompts；webUI 管理缓行）

| PR | 任务 | 出口命令 |
|---|---|---|
| **PR-P2-1 ✅**（2026-09-07） | **配置热重载**——`McpManager::apply_config` diff 保连语义（kept 原连接保留/updated 重建/removed 关闭/added 懒纳管/enabled 总闸）；`cfg` 改 `StdMutex`（热换面）；runtime `spawn_mcp_reloader`（watch 单写口订阅 → [mcp] diff → 外部配置重扫 → apply）；`spawn_config_file_poller`（mtime 1.5s 轮询手改文件 → `watch::reload_from_disk` 统一发布）；`Handle::try_current` 守卫（非 async 调用方跳过）；`ConfigStore::path()` getter | `cargo test -p qaqh-mcp --test lifecycle apply_config` + `cargo test -p qaqh-config --lib watch` 全绿 |
| **PR-P2-2 ✅**（2026-09-07） | 观察项④修复（`qaqh_mcp::sync_projection_now` 公开幂等投影 + engine_tool `handle_ui_tool_call` 在 mcp 前缀工具上前置 apply——UI 直调不再依赖回合边界）；unix socket 传输（url 校验放宽 `unix://` + adapter scheme 分发 → `from_unix_socket(path, "/mcp")`；内部路径约定 /mcp） | `cargo test -p qaqh-mcp --test http_transport` 5/5（新增 uds round_trip + missing path 失败语义） |
| **PR-P2-3 ✅**（2026-09-07） | prompts 能力——连接后 try-fetch `prompts/list` 入缓存（无 prompts 能力的 server method-not-found 静默降级，**不置脏**——不进工具投影）；`cached_prompts` + `get_prompt` 代理；聚合工具扩 `list_prompts`（server 可选，name/description/argument schema 行式清单）与 `read_prompt`（server+name+arguments → 渲染消息序列，lazy connect + 超时/取消同 read_resource 桥） | `cargo test -p qaqh-mcp --test resources` 20/20（新增缓存/渲染/缺 name 三用例） |

**P2 冒烟实录（2026-09-07，隔离实例 QAQH_DATA_DIR）**：手改 config.toml →
poller 检测（`file change detected; reloaded`）→ reloader diff——
`added=["demo"]` → `enabled=false` → `removed=["demo"]` → 重开 →
`kept=1 conn`（配置未变，连接懒恢复）。全链路（轮询→发布→diff→保连）实证通过；
用户 daemon（194715）全程零影响。

### Phase DT — 动态工具与 tool_search（2026-09-08 立项）

设计文档：`docs/dynamic-tools-design.md`（owner 诉求 U1/U2/U3 + Claude Code/Codex
实证 + Tool Manager 盘点实测）。三条工作流：W1 聚合工具拆分 / W2 exposure+
tool_search / W3 缓存失效诊断。owner 拍板（同日）：第一版全载入（全部 Direct）；
检索工具命名 `qaqh_tool`、参数 `{query}`；D1/D2/D6 已定；O3（prompt 告知工具集）
与 O4（skills 嵌套 query 澄清：query 只管 schema 发现，不叠路由层）在案。

- [x] **PR-DT-1**：todo 拆分（commit 本轮）——`todo_create/todo_insert/todo_set/
  todo_list` 薄壳 register（reject_fields 同表 + exec_* 复用）+ 旧聚合
  description 尾注 deprecated（软迁移）。接入点：PLAN_BLOCKED（写类三件进名单，
  todo_list 读类放行 plan 模式）、permission AutoApprove、conflict 合成键
  `__qaqh_todo__`、fold 透传白名单、registration 名单、schema_spot_check 守卫
  （无 action 维）。测试 +4（跨字段拒绝/roundtrip/plan+conflict 语义/
  deprecated+注册断言），workspace lib 323 全绿、runtime 164 全绿、clippy 0、
  fmt clean。**实测字节**：拆分四件 3336B vs 聚合 2705B——净 +631B（每工具
  ToolDef 固定开销 ~300B ×4；W1 真实收益=调用质量非字节，spec 已修正）。
- [x] **PR-DT-1 复盘**（同日）：todo_set 收敛单一形态 `{id,status,evidence?}`
  （owner 拍板）——ids/updates 移出模型面（底层 HTTP/CLI 保留），required
  [id,status]，reject 补 ids/updates；测试 +3 断言（ids/updates 拒绝、单条
  roundtrip、schema 单一形态守卫）；实测 todo_set 1264→531B，四件合计
  2603B < 聚合 2705B（净 -102B）。
- [x] **PR-DT-1 复盘②**（同日）：**todo v3 混合制形态**（owner 拍板）——
  研究两家任务工具（Claude TodoWrite 全量重写 ~250B / Codex update_plan
  ~300B；清空=传空数组无特判；Claude Task* 四件套=持久 issue-tracker 另轨）
  → QAQH 混合制：`todo_write`（追加+空清空，ID 保留单调不复用）/ 
  `todo_update`（单条状态）/ `todo_list`；**删 insert 与单条 title 形态**；
  修改=cancel+重写；**直接替换**（聚合+旧四件退役，dispatch.rs 删除，
  reject_fields 迁 split.rs）；底层契约全保留（HTTP/CLI）。新增 
  exec_todo_write。测试重写（追加两轮 ID 连续/空清空+高水位持久/形态守卫/
  v3 注册+旧名退役断言）；workspace 324 绿 / runtime 164 绿 / clippy 0 /
  fmt clean。**实测三件套 1576B vs 原聚合 2705B——净省 1129B**。
- [ ] **PR-DT-2**：skills 拆分（排后，先观察 todo v3 实战效果；见 spec O4）
- [ ] **PR-DT-3**：ToolExposure 两档 + filtered_defs 过滤 + allow 即 Direct
- [ ] **PR-DT-4**：`qaqh_tool`（线性检索 + tool_result 返回完整 ToolDef +
  description 列 family 名单）
- [ ] **PR-DT-5**：MCP projection_mode + always_load per-server
- [ ] **PR-DT-6**：缓存失效原因枚举 + 聚合 `refresh_tools` action
- [ ] **PR-DT-7**（可选）：BM25 + `_meta.ui.visibility` + 字节预算


设计文档：`docs/dynamic-tools-design.md`（owner 诉求 U1/U2/U3 + Claude Code/Codex
实证 + Tool Manager 盘点实测）。三条工作流：W1 聚合工具拆分（todo/skills →
单一职责，schema 打平或更省）/ W2 exposure+tool_search（MCP deferred 主战场）/
W3 缓存失效诊断。PR-DT-1..7 分步，决策点 D1-D7 待 owner 拍板（§6）。

## 5. 质量门禁总闸（每 PR 合入前必跑，继承旧 PLAN §8）

```powershell
just check          # cargo check --workspace          → 0 error
just clippy         # clippy --workspace --all-targets  → 0 error（deny unwrap_used/string_slice 维持）
just fmt            # cargo fmt --all --check           → clean
just test           # cargo test --workspace            → 全绿，0 失败；基线数 +N（MCP 新增用例）登记于本 PLAN §7
```

新增红线（并入评审红线清单）：
- qaqh-mcp 源码禁 `set_cancel(false)` 字面调用（用既有 clear 语义）
- MCP server spawn 必须进程组隔离（禁单 pid kill）
- env/secret 值禁入审计、日志、ToolResult

## 6. 风险与回退

| 风险 | 缓解 |
|---|---|
| rmcp 3.x API 漂移 | adapter.rs 单点隔离 + `Cargo.lock` 锁定 3.2.x；升级只动 adapter |
| 模型面膨胀 | per-server 工具白名单 + description 截断；Phase 2 按需加载 |
| 恶意 server（信任模型边界） | 文档明示"配置即信任=信任可执行程序"；审计全量 args 可追责 |
| 子进程泄漏（Windows 优先） | PR-M1-2/1-5 双平台孤儿测试；process-wrap + RAII；fd-hold 教训（docs/incidents/）已纳入关闭次序 |
| feature unification 影响 clippy --all-targets | dev-deps server feature 仅测试构建生效；M0 实测后在本节回填结论 |

## 7. 基线登记（PR-M0-1 时回填）

- [x] `cargo test --workspace` 基线通过数：**880 passed / 0 failed**（PR-M1-2 收口实测 2026-09-07；qaqh-mcp 计 9 用例：m0_spike 1 + lifecycle 8；mcp_config 10 在 qaqh-config 内）
- [x] **PR-M1-5 后实测（2026-09-07）：917 passed / 0 failed**（+13：call_path 7 + orphan_reap 3 + authorization MCP 准入 2 + dynamic_registration replace 1；qaqh-mcp 全家 29 用例：m0_spike 1 + lifecycle 8 + secrets_env 1 + secrets_mcp 8(qaqh-config) + call_path 7 + orphan_reap 3 + lib 单测 9）
- [x] **PR-M1-5 真实 daemon 冒烟（2026-09-07）**：QAQH_DATA_DIR 隔离实例 + node fixture（`[mcp.servers.demo]`）经 ringing 命令面 SessionCreate→SendMessage 端到端——**测出并修复投影预热缺口**（lazy 连接唯一触发点是工具执行，模型首回合看不到 MCP 工具的鸡生蛋死锁；`prime_all_async` 装配点 fire-and-forget 预热补齐），附带修复 daemon 日志硬编码 `HOME/.qaqh`（多实例混写，改落 `data_dir()`）与 fixture echo schema 空 properties（模型无法传参）；终态验证：模型调用 `mcp__demo__echo{text:"hello from daemon smoke"}` → `succeeded`，output=`echo: hello from daemon smoke`，audit.csv 同路落账；3 轮优雅停止零孤儿。改动：manager.rs/bridge.rs/lib.rs(+prime)、service.rs(装配)、daemon/main.rs(日志)、fixture schema
- [x] **PR-M2 后实测（2026-09-07）：937 passed / 0 failed**（917 → +20：resources 17 + mcp_env_block 2 + dynamic_registration reapply 1；clippy 0 警告、fmt clean、红线零命中）
- [x] clippy 诊断基线：`--workspace --all-targets` **0 警告**（同日实测；deny unwrap_used/string_slice 保持）
- [x] rmcp transport-child-process 进程组实测结论（2026-09-07，源码审计）：
  **未启用进程组**——rmcp 3.2.0 全 crate 无 `ProcessGroup` 使用（`child_process.rs`
  仅裸用 `CommandWrap`），且其对 process-wrap 只开 `["tokio1"]` feature；
  `CommandWrap` 未被 re-export → qaqh-mcp 需**自引 process-wrap 直依赖**
  （版本对齐 9.1.0，feature 并集后生效），在 adapter 层组装
  `CommandWrap + ProcessGroup`（Unix）/ `JobObject`（Windows）后再交给
  `TokioChildProcess::new`。已落 PR-M1-2 任务范围

## 8. 开放问题（owner 复核，均不阻塞 M0）

1. ~~Level 3 下 MCP 免审批~~ **已决议（D5，owner 2026-09-07）**：全档位默认放行；workspace 隔离权限重构另立 RFC
2. ~~`mcp` 聚合 vs per-server~~ **已决议（D6，owner 确认）**：聚合工具
3. idle_shutdown_secs 默认 300s：按默认执行，per-server 覆盖留 M3
