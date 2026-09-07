# QAQ-Harness qaqh-mcp（MCP 客户端支持）· 交接报告（handover）

| 项 | 值 |
|---|---|
| 日期 | 2026-09-07 |
| 交接范围 | qaqh-mcp 立项到 **M1-1 收口**：需求分析 → 设计 → 评审勘误 → PLAN → M0（Spike）→ M1-1（config schema），止于 PR-M1-2 开工前 |
| 起始 HEAD | `468cf59`（structure-simplification Phase 0-4 + D2 收官点；上一份 handover `docs/handover-2026-09-07.md` 的后续） |
| 结束状态 | HEAD 未动，**MCP 相关改动全部在工作区未提交**：8 个文件修改 + 3 个新增路径（清单见 §三） |
| 质量基线（交接时刻，实测） | `cargo check --workspace` **0 error** · `cargo test -p qaqh-config` **46 passed / 0 failed**（含新增 mcp_config 10 用例）· `cargo test -p qaqh-mcp` **1 passed**（m0_spike）· `cargo clippy -p qaqh-types -p qaqh-config -p qaqh-config-api -p qaqh-mcp --all-targets` **0 警告** |
| 权威路线图 | `docs/PLAN.md`（**本轮重写**为 qaqh-mcp 专用：M0–M3 分阶段 PR + 每 PR 出口命令 + 质量门禁总闸；旧「冗余收敛执行计划」已按 owner 指示归档至 `.qaqh/trash/PLAN.md.1788754970`） |
| 设计权威 | `docs/mcp-client-design.md`（D1–D6 决策 + §5 核心机制 + §2 依赖策略；评审勘误 E-1~E-8 已全部落盘） |
| 执行会话 | QAQ-Harness Agent（Omen Alpha）主会话；owner 全程在线拍板 |

---

## 一、任务背景与决策链（为什么会有这个 crate）

1. **起点**：会话主题是「Agent 知识库过时如何补全」——建立了三层补全体系：
   - L1 Skills（find-docs / context7-mcp，同一套 `npx ctx7@latest` CLI，每题 ≤3 次调用纪律）
   - L2 官方文档直读（web_fetch；`/llms.txt` 是 LLM 友好入口）
   - L3 环境内自证（编译器探针、`cargo new`、vendor 源码审计）
2. **展开**：用该体系实测了 8 家模型厂商 2026-09 最新版本（GPT-6 Astra / Claude Fable 5.1 / Gemini 3.8 Flash / Grok 4.6 / DeepSeek V4 / GLM-5.3 / Kimi K3 / MiMo-V2.5-Pro），随后定位到环境工具链（Python 3.14 / Node 26 / GCC 16 / **Rust 1.98**，全部超出 Agent 训练数据）。
3. **立项**：owner 亮明 QAQ-Harness 唯一开发者身份，指出 harness 无 MCP 支持；经 ctx7 + crates.io 侦察确认 `rmcp` 官方 SDK 成熟 → owner 四项拍板（D1–D4）→ 设计 → 评审 → PLAN → 开工。
4. **执行**：M0（Spike）+ PR-M0-0（设计勘误）+ PR-M1-1（config schema）已完成；M1-2 至 M3 未开工。

## 二、本次会话做了什么

### 2.1 侦察与设计（M0 前置）

- **SDK 选型依据**：`rmcp` 3.2.0（2026-08-31 发布）——crates.io 总下载 2463 万、官方 modelcontextprotocol 组织仓库、实现 spec **`2026-07-28`**（兼容 `2025-11-25` 及更早）、edition 2024 / MSRV 1.88 / tokio 原生——与 QAQH 技术栈完全同血统。ctx7 文档源 ID：`/websites/rs_rmcp_rmcp`（4537 片段）。
- **设计文档**：`docs/mcp-client-design.md`，11 节；核心机制 =「同步 fn 指针 handler ↔ mpsc 通道 ↔ tokio connection actor」桥接（复用 `backend_slot()` 的 `OnceLock<RwLock<Arc<_>>>` 槽位模式，不在 handler 线程 `block_on`）。
- **评审**：原计划 4 个子代理并行评审（架构/并发/安全/可测性），**全部被 harness 取消（exit 0，无产出）**→ 主会话四角度自评，产出勘误 E-1~E-8（3 条高危：取消契约对齐、D5 决策、secrets 扩展），已全部落盘设计文档。
- **PLAN**：`docs/PLAN.md` 重写（124 行）——继承前任 PLAN 的「提案 → PLAN → 分阶段 PR → 可命令化验收 → 独立回退」方法论。

### 2.2 PR-M0-0 · 设计勘误（全部落盘）

8 条勘误 E-1~E-8，要点：取消契约三层模型对齐（禁 `set_cancel(false)`、L3 进程组）、idle 语义改为 `inflight == 0` 连续计时、D5/D6 决策回写、secrets 扩展说明、单一 dispatcher 收敛、stderr 脱敏、依赖方向图、prompt injection 登记为接受风险。验收：`rg -c "取消契约|inflight == 0|全档位默认放行|\[secrets\.mcp\]"` 5 处命中（≥4 出口线）。

### 2.3 PR-M0-1 · Spike（已完成）

- workspace 接入：根 `Cargo.toml` 增成员 + `[workspace.dependencies] rmcp`（`default-features = false`，仅 `client` + `transport-child-process`）。
- **feature 剪裁验证**：`cargo check -p qaqh-mcp` 通过，依赖树无 server 侧代码（dev-deps 的 `transport-io`/`server` 仅测试构建生效）。
- **握手打通**：`tests/m0_spike.rs`——tokio in-memory duplex + rmcp `ServerHandler` mock（`get_info` + `list_tools` 返回 echo 工具），客户端 `().serve((read, write))` → `list_all_tools()` 断言 1 工具 + schema 结构 → `cancel()` 优雅收尾。**1 passed**，10s 兜底超时。
- **M0 必答题（关键发现）**：rmcp 3.2.0 **不设置独立进程组**——证据三连：① 全 crate 源码零 `ProcessGroup` 引用（`child_process.rs` 裸用 `CommandWrap`）；② rmcp 对 process-wrap 只开 `["tokio1"]` feature；③ `CommandWrap` 未被 re-export。→ 缓解路径：qaqh-mcp **自引 process-wrap 直依赖**（9.1.0 对齐，feature 并集生效），adapter 层组装 `CommandWrap + ProcessGroup`（Unix）/ `JobObject`（Windows）再交给 `TokioChildProcess::new`。已回填 PLAN §7。

### 2.4 PR-M1-1 · config schema（已完成，10/10 测试）

三层结构（严格沿用仓库既有模式）：

| 层 | 文件 | 内容 |
|---|---|---|
| 持久层 | `qaqh-types/src/config.rs` | `PersistentMcpConfig { enabled, idle_shutdown_secs, servers: HashMap<String, PersistentMcpServerConfig> }`、`PersistentMcpServerConfig { command, args, env, url, headers, tools, resources, default_timeout_secs, max_concurrent_calls }`（全 Option + skip_serializing_if）；`PersistentConfig.mcp` 字段；**lib.rs re-export 名单补录** |
| 运行时 | `qaqh-config/src/config.rs` | `McpConfig`/`McpServerConfig`/`McpTransportKind{Stdio,Http}` + `map_mcp_config()` 校验器 + load/save 双向映射；`load_from_paths_with` 改 `pub`（集成测试/多实例需要） |
| wire 层 | `qaqh-config-api` + `dto.rs` | `McpDto`/`McpServerDto`（只读；`servers` 为 BTreeMap 排序后的 Vec，确定性 wire 顺序）；`ConfigDto.mcp` 字段；穷举字面量映射（编译器强制） |

**校验规则（fail-fast，越界报错不静默 clamp）**：server 名 `[a-z0-9_-]+` ≤64；stdio/http 互斥（`command` 与 `url` 恰配一，http 强制 scheme 前缀）；tools 白名单条目非空；`default_timeout_secs` ∈ 1..=3600；`max_concurrent_calls` ∈ 1..=64。`${secret:name}` 占位符**原样保存**，求值推迟到 qaqh-mcp（PR-M1-3）。

**测试**（`qaqh-config/tests/mcp_config.rs`，10 用例）：缺省关闭 / stdio+http 双 server 解析与默认值 / 6 类非法配置拒绝（非法名、缺 transport、command-url 互斥、坏 scheme、timeout 越界、并发越界）/ 持久层 serde 往返 / 运行时默认值守卫。

## 三、当前仓库状态（git，交接时刻）

```
main ← 468cf59  structure-simplification: Phase 0-4 + D2 收官（单commit合入）   ← HEAD（未动）
工作区（未提交）：
  M  Cargo.toml                        # +成员 qaqh-mcp + [workspace.dependencies] rmcp
  M  Cargo.lock                        # rmcp 3.2.0 + process-wrap 9.1.0 + 传递依赖
  M  crates/qaqh-types/src/config.rs   # PersistentMcp* 两类型 + mcp 字段
  M  crates/qaqh-types/src/lib.rs      # re-export 名单 +2
  M  crates/qaqh-config/src/config.rs  # McpConfig 运行时三类型 + map_mcp_config + load/save 映射
  M  crates/qaqh-config/src/dto.rs     # to_dto 补 mcp 映射
  M  crates/qaqh-config-api/src/lib.rs # McpDto/McpServerDto + ConfigDto.mcp
  M  docs/PLAN.md                      # 整篇重写为 qaqh-mcp PLAN（旧 PLAN 已归档 trash）
  ?? crates/qaqh-mcp/                  # 新 crate：Cargo.toml + src/lib.rs + tests/m0_spike.rs
  ?? crates/qaqh-config/tests/mcp_config.rs
  ?? docs/mcp-client-design.md
```

**提交建议**（owner 决定）：可按 PR 边界拆两个 commit（`feat(mcp): M0 spike + 设计/PLAN 文档` 与 `feat(config): [mcp] schema + fail-fast 校验 + DTO`），或合一个；旧 PLAN 的归档意图已在 handover 与 PLAN 头部注明，无需单独提交说明。

## 四、技术决策记录（已拍板，勿复议）

| # | 决策 | 内容 |
|---|---|---|
| D1 | SDK 路线 | rmcp 3.2 官方 SDK，`default-features = false`，只开 client + transport |
| D2 | Phase 1 能力 | Tools + Resources（prompts/sampling/elicitation/tasks → Phase 2） |
| D3 | 方向 | 仅 MCP 客户端（反向 server → Phase 2，复用 `qaqh-workspace serve`） |
| D4 | 信任模型 | config.toml 白名单声明即信任 |
| D5 | **权限档位** | **全档位默认放行 MCP**（临时豁免已登记设计 §5.5）；owner 后续按 workspace 隔离重构权限体系时重新收敛——重构另立 RFC |
| D6 | Resources 形态 | `mcp` 聚合工具（list_servers/list_resources/read_resource），不新增 per-server 资源工具 |
| 归属 | McpManager 归属 | `QaqhService`（daemon 级 `OnceLock<Arc<McpManager>>` 槽位，`init` 装配、与 `init_tools` 同层）；**不放** AgentRegistry——per-server 共享生命周期与 per-session worker 生命周期轴正交，桥接走全局槽位无需 worker env 注入 |
| S1–S3 | 建议决策（已采纳进设计） | S1 Phase 1 仅 stdio；S2 命名 `mcp__{server}__{tool}` 前缀；S3 category：stdio→Exec / http→Net（子代理沙箱 exec/net 自动拒绝**零特判**生效） |

## 五、遗留工作（按 PLAN 顺序，todo T8–T11）

| PR | 内容 | 出口命令 |
|---|---|---|
| **PR-M1-2**（下一步） | McpManager + connection actor：lazy connect（10s 超时，`ClientLifecycleMode::Auto`）、`inflight == 0` idle 回收（默认 300s）、重连冷却 5s、`shutting_down` 闸、RAII reap；**adapter 层自引 process-wrap 补 ProcessGroup/JobObject**（M0 实锤缺口） | `cargo test -p qaqh-mcp --test lifecycle`（connect/timeout/crash-reconnect/idle/shutdown 五用例，in-memory + 子进程各一） |
| PR-M1-3 | secrets.toml 通用 map 段（`[secrets.mcp]`）+ `${secret:name}` 插值 + 脱敏钩子（E-4；现 `secrets.rs` 是 per-slot API key 仓库，需扩展） | `cargo test -p qaqh-config --test secrets_mcp` |
| PR-M1-4 | `ToolManager::register_dynamic`（仅回合边界）+ `mcp__` 前缀命名 + 模型面投影合并 + description 截断 2KB + 碰撞拒绝 | `cargo test -p qaqh-workspace --test dynamic_registration` |
| PR-M1-5 | bridge（250ms cancel 轮询、超时链）+ MCP_* 错误码全量映射（§7 八码）+ 审计接入 + ToolStats + **子代理拒绝验证**（`actor.rs:111` 旗标路径）+ **孤儿进程专项**（kill daemon 后 `pgrep -g` 空 / Windows `tasklist /T` 无后代） | `cargo test -p qaqh-mcp --test call_path --test orphan_reap`；`rg -n "set_cancel\(false\)" crates/qaqh-mcp/src` **零命中** |
| M2 | `mcp` 聚合工具 + 系统提示注入块（封顶 20 条/120 字符）+ tools/list_changed 回合边界置脏刷新 | `--test resources` / `--test mcp_env_block` |
| M3 | streamable HTTP（reqwest feature）+ unix socket + ToolStats + 文档收口；E2E（`QAQH_MCP_E2E=1` 连真 context7） | 见 PLAN §4 M3 |
| Phase 2 | 反向 MCP server（包 serve）、prompts/sampling/elicitation/tasks、OAuth、资源订阅、图片 content→read_image、配置热重载（`watch.rs` 已有能力）、WSL stdio server | 另立 RFC |

**设计文档需小幅跟进**：§4 crate 设计里 `qaqh-mcp/src/config.rs` 的职责已由 PR-M1-1 落到 `qaqh-config`（类型/解析/校验单一真相源），qaqh-mcp 侧改为消费——M1-2 开工时顺手修订 §4 文件清单。

## 六、坑与教训（给下一个执行者，全部为本会话亲历）

1. **qaqh-types 是显式 re-export 名单**（lib.rs L32 起），不是 glob——新增 config 类型必须同步补名单，否则 qaqh-config 侧报 `cannot find type ... in crate qaqh_types` 且错误提示指向 `PersistentConfig`（similarly named），极易误判为路径问题。
2. **map 类型不对称**：运行时 `McpServerConfig.env/headers` 用 `BTreeMap`（确定性顺序，利于 wire/测试），持久层用 `HashMap`（跟随 profiles 先例）——save 映射处必须 `.into_iter().collect()` 转换。
3. **编辑工具语义坑**：`replace` hunk 必须带 `old` 字段（`anchor` 不顶替）；`insert_after` 是在**整个锚块之后**插入（多行锚会越过块尾）；**中文弯引号（“”）会被编辑传输规范化成直引号**导致 old 匹配失败——长文档修订建议改用 python 字节级替换（本会话最终方案）。
4. **rmcp 3.2 API 与训练记忆差异大**：`ListToolsResult` 由 `paginated_result!` 宏生成（含 `result_type`/`ttl_ms`/`cache_scope` 等 SEP 新字段，构造用 `::with_all_items(vec![...])`）；`ServerInfo = InitializeResult` 别名；客户端挂 `RunningService::list_all_tools()`；`ServerCapabilities::builder().enable_tools().build()`。**vendor 源码是最好的老师**：`~/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-*/rmcp-3.2.0/`（清华镜像，沙箱无代理直连 crates.io 走镜像）。
5. **timeout 嵌套 Result 坑**：`tokio::time::timeout(dur, fut).await.expect(msg)` 只解外层 `Elapsed`，内层 Result 会被 `;` 丢弃并触发 `unused_must_use`——测试函数应返回 `Result` 并在 expect 后接 `?`。
6. **spawn_subagent 在本会话被 harness 取消**（4/4，exit 0 无产出）——重分析任务需主会话自扛，或 owner 排查 harness 子代理调度。
7. **ConfigStore::load 静默语义**：TOML 解析失败（含重复表名）→ 返回 None → 整个 config 回退默认。MCP 配置写错表名会连带全配置失效——既有仓库语义，PLAN §8 已登记观察，未在本次修改。

## 七、风险与观察点

| 风险 | 缓解 | 状态 |
|---|---|---|
| D5 临时豁免窗口（MaxLockdown 下 MCP 调用不经确认） | owner 已知情并承诺 workspace 隔离权限重构时收敛 | 已登记设计 §5.5 |
| 模型面膨胀（多 server × 多工具） | per-server 工具白名单 + description 截断 2KB；Phase 2 按需加载 | 设计 §10-2 |
| 恶意 server（配置即信任 = 信任可执行程序） | 审计全量 args 可追责 + stderr 脱敏（E-6）+ 文档明示 | 设计 §10-3 |
| rmcp 3.x API 漂移（63 版本/18 个月） | adapter.rs 单点隔离 + 锁 3.2.x | M1-2 落地 |
| Windows 子进程清理（owner 主力 Windows，本会话验证在 Linux） | process-wrap + RAII + 孤儿专项测试进 PR-M1-5；fd-hold 教训已纳入 | M1-5 验收项 |
| M0 未跑全量 workspace test | 交接基线以 touched crates 为准；全量回归挂在 PR-M1-1 后首个 commit 前 | PLAN §7 待回填项 |

## 八、关键文件索引

| 文件 | 状态 | 说明 |
|---|---|---|
| `docs/mcp-client-design.md` | 新增 | 设计权威：D1–D6、依赖策略、§5 核心机制（桥接/生命周期/投影/resources/权限）、§7 错误模型、§9 里程碑、§11 接触面清单 |
| `docs/PLAN.md` | 重写 | qaqh-mcp 实施 PLAN：§2 勘误表（E-1~E-8）、§3 阶段总览、§4 每 PR 出口命令、§5 质量门禁总闸 + 红线、§7 基线登记（进程组结论已回填） |
| `Cargo.toml`（根） | 修改 | workspace 成员 + rmcp 3.2 workspace 依赖（default-features=false） |
| `crates/qaqh-mcp/` | 新增 | Cargo.toml / src/lib.rs（M0 占位）/ tests/m0_spike.rs（in-memory 握手测试） |
| `crates/qaqh-types/src/config.rs` + `lib.rs` | 修改 | PersistentMcp* 持久层类型 + re-export |
| `crates/qaqh-config/src/config.rs` | 修改 | 运行时三类型 + `map_mcp_config` 校验器 + load/save 映射 + `load_from_paths_with` 转 pub |
| `crates/qaqh-config/src/dto.rs` | 修改 | to_dto 穷举映射补 mcp |
| `crates/qaqh-config-api/src/lib.rs` | 修改 | McpDto/McpServerDto + ConfigDto.mcp |
| `crates/qaqh-config/tests/mcp_config.rs` | 新增 | 10 用例（解析/默认值/6 类拒绝/往返/默认守卫） |
| `docs/handover-2026-09-07.md` | 只读参考 | 上一份交接（结构简化工程）；本文件是其后续 |
| `.qaqh/trash/PLAN.md.1788754970` | 归档 | 旧冗余收敛 PLAN 原文（owner 指示移除，可恢复） |

## 附录：快速核验命令

```bash
# M1-1 出口（PLAN §4）
cargo test -p qaqh-config --test mcp_config          # 10 passed
# M0 出口
cargo test -p qaqh-mcp                               # m0_spike 1 passed
# 全局无破坏
cargo check --workspace                              # 0 error
cargo clippy -p qaqh-types -p qaqh-config -p qaqh-config-api -p qaqh-mcp --all-targets  # 0 警告
# 勘误验收（PR-M0-0）
rg -c "取消契约|inflight == 0|全档位默认放行|\[secrets\.mcp\]" docs/mcp-client-design.md   # ≥4
# 契约红线静态检查（M1-5 起持续有效，当前应为零命中）
rg -n "set_cancel\(false\)" crates/qaqh-mcp/src
# 设计期文档检索（L1 管线，每题 ≤3 次）
npx --yes ctx7@latest library "rmcp" "<query>"
npx --yes ctx7@latest docs /websites/rs_rmcp_rmcp "<query>"
# rmcp 源码审计入口（清华镜像 vendor 目录）
ls ~/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-*/rmcp-3.2.0/src/transport/
```
