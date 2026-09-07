# QAQ-Harness 结构塑形与简化分析报告（structure-simplification-plan）

| 项 | 值 |
|---|---|
| 日期 | 2026-09-07 |
| 审计对象 | `/home/qaqtamsy/Projects/QAQ-Harness`（main @ `0e9596c`，工作区干净） |
| 范围 | 14 crates，~86k 行 Rust（非测试约 62k） |
| 分析方式 | codegraph 全局扫描（289 文件 / 5,692 节点 / 20,275 边）+ 4 个子代理分层审计（运行时/工具/LLM/基础层）→ 主会话交叉复核 |
| 上游文档 | `docs/redundancy-audit-2026-09-06.md`（上轮冗余审计，其 PLAN 已于 2026-09-06 全部收口；本轮为其遗留的「结构塑形」立项前期分析） |
| 方法论 | 继承上轮纪律：codegraph 仅作探索加速（存在 callers 漏报先例），**删除类操作的唯一判定标准是 `rg` 全量文本检索**；所有发现带 文件:行号 证据 |

## 结论速览

| Phase | 主题 | 预估收益 | 风险 |
|---|---|---|---|
| 0 | 死代码清零（纯删除） | **−455 行** + 移除 ureq 依赖 | 极低 |
| 1 | 僵尸抽象与依赖修正 | −350~450 行 + 修正 types→skills 反向依赖 | 低 |
| 2 | 六大巨型文件拆分 | 6 个 1200-2940 行文件解体（行数近似不变，结构收益） | 低→中高 |
| 3 | 协议/驱动层深度收敛 | −720~880 行 + 消灭 3 处行为漂移 | 中 |
| 4 | 注释与文档修订 | 30+ 项（含 1 个 release 缺陷） | 零 |

---

## Phase 0 · 死代码清零（纯删除）

全部经 rg 全文检索 + codegraph 双确认零调用。每删一批跑 `cargo test --workspace`。

| # | 项 | 位置 | 行数 | 证据 |
|---|---|---|---|---|
| 0-1 | `lease.rs` 整模块 | runtime/src/lease.rs（lib.rs:13 导出） | 157 | `LeaseManager`/`LeaseDecision` 全仓唯一引用 = lib.rs 导出行；真实 lease 逻辑在 `RingingLeaseStore`（axum_server.rs:41） |
| 0-2 | `push_tool_results_batch` | message/src/store.rs:754-853 | 100 | 定义外零出现 |
| 0-3 | `replace_tool_result` | message/src/store.rs:854-924 | 71 | 唯一"出现"是其内部 log 字符串 |
| 0-4 | `detect_os_info` | runtime/src/registry.rs:52（lib.rs:14 导出） | ~40 | 全仓零调用；其唯一副作用写的 `prompt::OS_INFO` 唯一读取点 prompt.rs:67 永走 `unwrap_or("")` 降级 → 机制实际失效（见决策点 D1） |
| 0-5 | `run_axum_with` | daemon/src/axum_server.rs:1826-1846 | 21 | `#[allow(dead_code)]`；server.rs:338-341 直接 build_router+serve，不经它 |
| 0-6 | `domain_failure`/`occurrence_id` 副本 | runtime/src/agent/engine_turn.rs:621/641 | ~30 | `#[allow(dead_code)]`；逐字相同副本在 backfill.rs:14/32、admit.rs:16/34（活） |
| 0-7 | `Effect::CallGate` 变体 | message/src/effect.rs:10 | ~3 | 全仓唯一出现 = 定义处；runtime 只 match None/TurnComplete（engine_turn.rs:1242-1260） |
| 0-8 | `register_exec_for_compat` | workspace/src/exec.rs:1549-1565 | ~25 | `#[allow(dead_code)]`，callers 为空，注释自称"旧调用链"已不存在 |
| 0-9 | `fetch_models` + ureq 依赖 | config/src/registry.rs:603-644 | 42 | 全仓零调用；删除后 qaqh-config 可去掉 ureq HTTP 依赖栈；`models_url_for`(591-601) 随之仅测试引用，一并处理 |
| 0-10 | `Config::is_ready` / `Config::protocol` | config/src/config.rs:754-756/759-761 | 6 | 全仓零调用 |
| 0-11 | `BalanceInfo` | types/src/config.rs:245-261 | 17 | 全仓零调用 |
| 0-12 | `TokenBreakdown` | types/src/token.rs:59-64 | 6 | 零外部调用；字段 `episodic` 是记忆系统残留词汇 |
| 0-13 | `RagConfig` + `Config.rag` 字段 + 6 default fn | config/src/config.rs:54-108,154 | ~55 | `.rag` 无任何读取方（先确认无 deny_unknown_fields，serde 默认忽略未知字段则安全） |
| 0-14 | `kill_process` | types/src/platform.rs:380-391 | 12 | 零外部调用（仅测试），连测试一起删 |
| 0-15 | `migrate_legacy_data_root_marker` 无后缀包装 | types/src/platform.rs:113-127 | 15 | 零外部调用，内部仅 `_at` 版本被用 |
| 0-16 | arg.rs 四函数 + re-export | types/src/arg.rs:29-56（lib.rs:47-49） | ~30 | codegraph callers=0、grep 全仓仅 re-export；workspace/lib.rs:671-680 只转发另外 3 个 |
| 0-17 | `CacheEntry.json` 字段 | workspace/src/file_cache.rs:16 | ~3 | 写入缓存从不读出（删前读码确认序列化影响） |
| 0-18 | `Shell::derive_exec_args` | workspace/src/exec.rs:152-158 | 7 | `#[cfg_attr(not(test), allow(dead_code))]` 仅测试使用 → 移入测试模块 |
| 0-19 | 过时 allow 属性 | workspace/src/edit/locate.rs:139、runtime/src/service.rs:1141 | 0 | 属性过时（函数实际被调用/活跃），删属性留函数 |
| — | **合计** | | **~455** | |

## Phase 1 · 僵尸抽象与依赖修正（低风险）

1. **`CompactEngine` 去壳**（runtime/agent/engine_compact.rs:79）：unit struct（`new()->Self`、`reset(){}` 空体），`build_prompt_and_meta`/`apply_result` 不读实例状态 → 自由函数化，删 `Loop.compact` 字段（loop_core.rs:189/247），调用点 :946/:1093 改自由函数。reset_all_engines(:344-353) 本就不重置它。
2. **`Effect` 降级**（message）：`enum Effect{None,TurnComplete,CallGate}` → `turn_completed(): bool`（判定已存在于 store.rs:55-61）；改 13 处返回点 + context_flow 1 处 + runtime 2 处 match（engine_turn.rs:1242-1260）。
3. **types→skills 依赖反转（方案 A）**：skills/src/session_state.rs（62 行纯 serde 类型，注释自述"Movided verbatim from qaqh_types::session"）下沉回 types/src/session.rs；skills Cargo.toml 加 `qaqh-types = { path="..", default-features = false }`（**避免 tokenizers 拖入 skills**）；skills lib.rs 留 3 行 re-export shim；删 types/Cargo.toml:8 的 skills 依赖。消除 runtime 同类型双 import 路径（agent/state/agent.rs:43 vs :672）。
4. **tool_mode 三僵尸**：`is_minimal_dsh`（恒 false，tool_mode.rs:65-67）+ `model_tool_name`/`internal_tool_name`（恒等映射，:85-94）+ runtime 死分支（prompt.rs:64、agent.rs:606/461/498）一并内联删除。
5. **client 平台代码去重**：client/discovery.rs 自带 `data_dir`(:47-62)/`discovery_path`(:66-68)/`process_is_running`(:216,239) 与 types/platform.rs 逐语义相同 → 改用 `qaqh_types::platform`。
6. **`TodoActivationItem`→`TodoItem`**（domain/event.rs:269-275 vs :130-136 同文件孪生，字段完全一致）：runtime/agent/types.rs:226 改用 TodoItem。
7. **工具错误协议统一**（workspace，−200~300 行）：4 种错误表达 → `ToolResult::error_with/error_data` 单一方式。需处理：copy_range.rs:268-300 手搓 JSON 信封、apply_patch.rs:163-172、edit/handler.rs:16-28 fail 闭包双份塞字段、todo.rs `Result<String,String>` 字符串穿透（38 处）、file_mutate.rs:393 前缀嗅探。**前置**：确认 bindings/qaqh 前端无字符串 JSON 耦合（仓库内仅 runtime/engine_tool.rs:526 自产自销）。
8. **`JsonArgs` trait 推广**：lib.rs:146-168 已有 s()/s_or()/opt_bool()，仅 11 处使用、68 处手搓 `args.get(k).and_then(as_str)` 且缺键语义三样（静默空串 vs 报错）→ 统一。

## Phase 2 · 巨型文件拆分（机械迁移，每项独立 PR）

Rust 同 crate 多 `impl` 支持，对外 API 全部不变；顺序 = 风险升序：

| 文件 | 现状 | 方案 | 风险 |
|---|---|---|---|
| service.rs 1215 | handle 407 行 47 分支 | `service/{params,fs_git,stats,plan}.rs`（自由函数归位 L731-1170）+ handle 按前缀族（workspace/session/fs_git/config+profile+skills/plan+todo+stats）拆 5 组薄路由；分支互不共享状态 | 低 |
| axum_server.rs 2439 | handle_command 562 行 | 先删 run_axum_with(0-5) → 抽 RingingCommandAck builder（≥6 处字面量 L359-378 等）→ `daemon/http/{auth,command,timeline_api,content,service_api,sse,debug+control}.rs` 7 文件（行段映射见审计原文）；约 20 个 HTTP 测试兜底 | 中 |
| loop_core.rs 1904 | dispatch_ringing_one 431 行三层 match | `agent/loop/{injection(L407-712),dispatch_control(L1245-1440),dispatch_conversation(L1441-1576),dispatch_tool(L1577-1665),outcome(L1666-1873)}.rs`；Loop 字段提 `pub(super)`；主文件留 ~600 行 | 中低 |
| exec.rs 2527 | 6 件事混装 | `exec/{shell(L22-343),pipe(L345-714),truncate(L717-810),direct(L811-1122),handler(L1124-1444)}.rs` 按既有分段切 + 962 行测试外移 `exec/tests.rs`（照 edit/tests.rs 模式） | 低 |
| todo.rs 1578 | 四层焊死 | `todo/{model(L23-80),store(L83-312),parse(L313-429),actions(L430-941)}.rs`；`todo_set_for`(221行) 拆 parse_updates/parse_ids/apply 三段各 ≤60 行；store 层未来移交 qaqh-session | 低 |
| hub.rs 2940 | 三频道+timeline+content 混装 | `ringing/timeline_hub.rs`：TimelinePersistence 子结构体持 6 个 timeline 字段 + ~670 行方法；`ringing/orphan_seal.rs`：孤儿收尾族 ~300 行；RingingHub pub API 一行转发（lib.rs:15 导出面不变）。风险点：impl Drop(:1726) 与 ensure_seed_loaded/ensure_timeline_loaded 共享 lazy_load 锁；20+ hub 测试兜底；**放最后做** | 中高 |

## Phase 3 · 协议与驱动层深度收敛（中高风险，需决策）

1. **gate 提取 `transport.rs`**（−540 行，gate 非测试 −12%）：三协议字节级相同件上提——`block_on`+`FALLBACK_RT`（3 个独立 current-thread runtime！）、`sleep_with_cancel`/`is_cancelled`/`is_retryable`/`backoff_delay`/`http_error_description`/`normalize_skill_envelope`/`filter_stateful_messages`/`SseTrace`（100% 相同，message_api.rs:99 注释自认复制）；流式重试主循环 3 份逐分支对齐（chat:215-311/message:894-987/responses:640-727）→ `retry_engine::run` ~70 行；SSE 读循环外壳 3→1。**同时消灭 3 处行为漂移**：3 个 runtime 实例、responses backoff 无 30s cap、**responses_api GLOBAL_CLIENT 无总超时（接近 bug，见 D6）**。协议本质差异（convert_messages ×3、frame handler ×3、convert_tools ×3）不合并，仅抽零件（图片占位符格式串 6 处逐字重复、ImageRef 降级路径 3 份、ToolResult envelope 3 份 → `convert_common.rs`）。
2. **gate/client `SseDecoder` 合一**（−60 行）：核心 ~45 行语义同构（连注释都复制粘贴级相似），泛型 sink `SseDecoder<P: FrameSink>` 同时满足 String 聚合（gate）与 SseFrame{id,event,data}（client）；落点需拍板（D3）。
3. **context_flow.rs 瘦身**（−120~150 行）：删 `Visibility.timeline/persist`（从不读取，注释承诺未兑现）、`dedupe_key` 机制（生产恒 None）、`Sink::Annotation`（自认未实现）、`builtin::TOOL/ENV`（外部 ingest 0 次，tool 结果实际走 runtime `push_tool_result_direct_with_attachments` 绕过 flow）；修正 "single door" 失实文档（context_flow.rs:5-6）。`ContextSource` trait 本身保留（暂不强改 enum，先把死重清掉）。
4. **manager.rs `mutate_meta` 收敛**（−120 行）：13 处 `load_meta().unwrap_or_default()` 样板（:435-778）提取 `mutate_meta(seed, index, f)`；顺带统一错误策略（`persist_tool_mode` 返回 Result vs `persist_mode` 吞错）——保住 CK-PERSIST 检查点语义（:447 注释）。
5. **client `drain_frames` 双份**：sse.rs:153-165 与 timeline.rs:258-270 逐字相同（含注释），dispatch 各异——decoder 已单源，drain 循环可随之合一。

## Phase 4 · 注释与文档修订（30+ 项精选）

**缺陷级（立即）**
- `chat_completions_api.rs:822`：`eprintln!("[filter] 输出: ...")` —— `#[cfg(debug_assertions)]` 只盖输入侧，**release 也打**，每个 stateful 请求污染 stderr。删除或补 gate。

**README 与 crate 声明**
- README.md:3 "20 个内置工具" vs :23/:51 "19 个" —— registration.rs:59-80 断言 18 + spawn_subagent = **19**，统一为 19
- config/Cargo.toml:4 description 含 "prompts"（本 crate 无任何 prompt 逻辑，prompts 在 runtime/agent/prompt.rs）
- README.md:40 "domain 不依赖 wire 类型" 基本属实但需补一句"复用 qaqh-types 的规范工具结果模型（ContentRef/ToolResult/UsageInfo，event.rs:11-12 re-export）"

**失实/过时声明**
- types/platform.rs:28-29 "前端禁止自建 FNV 公式" —— DataRootMarker/normalized_path_text/data_root_id 无外部调用方、无 TS 导出，声明夸大 → 改契约文档措辞或真给 ts 导出
- runtime/ringing/hub.rs:11 引用全仓零定义的 `EventBus`；hub.rs:3-8 职责清单漏 timeline 持久化 + content store（实占 24%）
- types/session.rs:3 "pub use qaqh_skills::..."（Phase 1.3 完成后此文件改写）
- skills/session_state.rs:2-4 "Moved verbatim from qaqh_types::session"（迁移史，Phase 1.3 后同步改写）
- domain/lib.rs:10 ASCII 架构图仍画出已删除的 `LegacyProjector → Agent2Ui` 路径
- message/effect.rs:25-37 "an injected the session manager singleton" 全局替换事故语法
- engine_turn.rs:123/709/889/1243、util/mod.rs:136 过程性迁移标记（"moved to turn_lap (A2 stepN)"）清理
- loop_core.rs:17-18 "Stateless engines" 分组与实际不符（misc 有 reset 语义、compact 是空壳）
- prompt.rs:29 "OS_INFO (set at startup via agent_bridge)" —— agent_bridge 全仓零匹配，机制不存在（随 0-4 一并处置）

**仓外腐烂引用（保留结论、删行号级引用）**
- message_api.rs:4-9（proxybun/src/index.ts:292）、chat_completions_api.rs:1-2 / responses_api.rs:20（opencode session/retry.ts）、registry.rs:159/179-180（proxybun、out/host/index.js 构建产物）

**过时 allow/文档错位**
- edit/locate.rs:139 `#[allow(dead_code)]` 挂在被 4 处调用的 `parse_hint_line` 上（误导）
- runtime/service.rs:1141 allow（函数 L100/L152 活跃调用）
- permission.rs:343-351 `needs_permission` 的文档挂在了 `is_sensitive_session_path` 上；真身(:370)裸奔
- serve.rs:250-253 与 257-259 同段注释连写两遍
- execution.rs:835-839（测试）自问自答式遗留注释，且引用的 PLAN_BLOCKED 名单是错的（实际 lib.rs:353 为 `["edit","exec","process","todo"]`）
- effort 词表注释三处过期（"high/max" → 实际 5 档 low|medium|high|xhigh|max）
- wal.rs:60 / migrate.rs:38 serde 字段 allow 建议改注 `// kept for format compatibility`
- skills/runtime.rs:162/692 双否定注释改正向陈述
- gate/sse.rs:9 "~143MB/s" 无 bench 支撑 → 改"目标：O(n) 无搬移；数字需 bench 支撑"；:1 "两条路径"实为三条

---

## 待人工确认决策点

| # | 决策 | 建议 |
|---|---|---|
| D1 | `detect_os_info` 删除后 `prompt::OS_INFO` 读取逻辑一并删（行为等价，恒走降级）还是留？ | 一并删，行为完全等价 |
| D2 | tool 结果是否未来统一走 `ingest(TOOL,…)`？决定 context_flow TOOL/ENV source 删还是补 | 若无计划，删 |
| D3 | SseDecoder 合一落点：qaqh-types / 新微 crate qaqh-sse / 不合 | 建议不合，仅互加差异注释（上轮审计同结论） |
| D4 | `PersistentConfig` 6 个扁平字段（types/config.rs:14-37）删除需弃用窗口 | 下个版本窗口后删，−120~150 行 |
| D5 | RingingEvent/RingingCommand 收敛为 type alias（−90 行）——前端 bindings 改名联动 | 暂缓，收益 < 90 行 |
| D6 | responses_api 缺总超时（reqwest 默认无限流挂起风险）——是否按 bug 提级？ | 建议提级，Phase 3.1 一并修 |

## 执行纪律

1. 先死库存（Phase 0 零语义变化）→ 僵尸与依赖（Phase 1）→ 结构塑形（Phase 2）→ 深度收敛（Phase 3）→ 注释收尾（Phase 4）；
2. 每批删除独立 commit 便于回退；每批后 `cargo test --workspace` + `just clippy`；
3. codegraph 仅探索加速，删除判定以 rg 全文检索为准；实施前 `codegraph sync`；
4. 对外可见面不动：JSON wire shape、daemon.json 格式、bindings/qaqh TS 面、workspace CLI 词表、Ringing V1 协议。

## 附录：分析过程

- 子代理 4 个：analyze_runtime_layer（首轮超时，v2 以「结构扫描+定点精读」纪律重跑成功）、analyze_tool_layer、analyze_llm_layer、analyze_foundation_layer；全部只读，未修改文件。
- 交叉对齐：上轮审计已修项（file_core/qaqh-proto/wire.rs/GLOBAL_CLIENT/sha256_hex/file_edit_v2）已从本轮清单剔除；types→skills 由两份报告独立发现并收敛为同一方案 A。
- 基线快照：cargo check --workspace --all-targets 全绿（2.86s）；869 个 #[test]。
