# QAQH 设计：动态工具与 tool_search（Tool Manager 演进）

状态：**设计稿（待 owner 评审）** · 2026-09-08 · 作者：Omen Alpha（QAQ-Harness Agent）· 决策人：项目 owner

> 前置阅读：`docs/mcp-client-design.md`（MCP 客户端，已实施）。本文是工具环
> （Tool Manager）的演进设计：吸收 Claude Code / Codex 两家的 deferred 工具
> 思想，解决 owner 三项诉求——见 §1。

## 1. 背景与 owner 诉求（2026-09-08 确认）

| # | 诉求 | 现状痛点 |
|---|---|---|
| U1 | **不要把全部工具塞进上下文** | 18 个内置工具全量 Direct：实测 schema 15,517 B ≈ 4.4–5k tokens **每请求常驻**；MCP 多 server 后线性膨胀 |
| U2 | **聚合工具调用质量**：todo/skills 是 action+参数二层调用，模型表现"褒贬不一" | `todo` schema 2705 B（全库最大）：4 个 action 的 11 个参数平铺，参数↔action 关联靠 oneOf+文字说明，模型需自行拼装 |
| U3 | **tool_search 后 tool_result 返回工具名+schema**，模型按需发现与调用 | 无此机制——工具要么全量在 schema 里，要么不存在 |

另有贯穿性关切（M1-P2 已部分解决）：**热更新 MCP 不应反复打 prompt cache**。
现状：MCP 工具集变化（热重载 added/removed）→ tools 数组变化 → 该 endpoint
缓存前缀一次性失效。deferred 层可将其归零（§5.2）。

## 2. 竞品实证（2026-09-08 逆向研究）

### 2.1 Claude Code（@anthropic-ai/claude-agent-sdk 0.3.263 + 206MB 编译二进制）

| 机制 | 证据 | 语义 |
|---|---|---|
| **全量 tools 数组（默认）** | MCP 工具转 `{type:"custom", name, input_schema, description}`；命名 `mcp__<server>__<tool>`；权限规则通配 `mcp__server__*` | 与 QAQH 同构 |
| **Tool Search（deferred loading）** | `defer_loading` ×36；`tool_search_tool_regex`/`tool_search_tool_bm25`；`isDeferredTool`/`isDeferredToolInConversation`/`formatDeferredToolLine` | tool search 启用时 **MCP 工具默认全部 `defer_loading: true`**——schema 不进请求，模型经两个搜索工具按需发现（**Anthropic API 原生承担搜索与展开**——供应商锁定） |
| **alwaysLoad（per-server 豁免）** | 配置 schema 描述原文："all tools from this server are always included in the prompt and **never deferred**… Equivalent to setting `defer_loading: false`"；副作用：**该 server 阻塞启动直到连接（5s 上限）**——defer 模式下 MCP 启动才是非阻塞的 | 关键 server 的 schema 常驻的语义出口 |
| **RefreshMcpTools 元工具** | 三态 `refreshed \| error \| not_connected`；"**this tool never dials**"（不建连）；error 时**保留旧工具集** | 模型可主动刷新已连接 server 的工具清单（失败保旧集——与 QAQH 热重载的 owner 触发互补：这是模型侧触发） |
| **资源元工具** | `ListMcpResourcesTool`/`ReadMcpResourceTool`/`ReadMcpResourceDirTool` | 与 QAQH 聚合工具 `list_resources`/`read_resource` 同构 |
| **缓存失效遥测** | 失效原因枚举含 `defer_loading_changed`、`ttl_expired_5m/1h`、`betas_changed`… | deferred 集合变化（工具被展开）本身被跟踪为缓存失效原因 |
| 会话内展开状态 | `isDeferredToolInConversation` | 已展开的 deferred 工具在会话中状态可见（对应 QAQH 落地的词汇表语义问题，见 §6-D3） |

### 2.2 Codex（openai/codex codex-rs，真源码）

| 机制 | 证据 | 语义 |
|---|---|---|
| **六档 ToolExposure** | `tools/src/tool_executor.rs`：`Direct / Deferred / DeferredModelOnly / DirectModelOnly / CodeModeOnly / Hidden` | 暴露维度一等公民。**分发与暴露解耦**：`Hidden` 注释原文 "Keep this tool registered **for dispatch** without exposing it to the model" |
| **MCP 分配规则** | `core/src/mcp_tool_exposure.rs`：`exposure = search_tool_enabled ? Deferred : Direct`；`search_tool_enabled = model_info.supports_search_tool && provider.capabilities().namespace_tools` | **按模型+provider 能力自动切换**，非配置开关 |
| **本地 BM25 tool search** | `core/src/tools/handlers/tool_search.rs`：`bm25` crate 本地索引 deferred 工具的 `search_info` → 模型调 `tool_search` → **tool result 返回命中工具的完整 `LoadableToolSpec`**（模型下一轮直接 tool_call，无需二跳） | **供应商无关的 deferred**——不依赖 OpenAI/Anthropic 专有特性。`ToolSearchHandlerCache` 按源缓存，registry 变化才重建 |
| **schema 字节预算** | agent-plugin 来源：单工具 spec > 8 KB 或累计 > 64 KB → **`Hidden`**（注册可分发但不对模型暴露） | schema 膨胀的硬防护 |
| **`tool_is_model_visible`** | 遵循 **MCP ext-apps 规范** `_meta.ui.visibility`：server 可声明工具"仅 UI、模型不见" | QAQH 现状**忽略**该元数据（全可见） |
| **Namespace 聚合** | `coalesce_loadable_tool_specs`：同 namespace 工具合并为单个 namespace 条目（Responses API 特性，provider capability 门控） | schema 压缩的另一杠杆 |
| **API 原生 defer 透传** | `ResponsesApiTool.defer_loading: Option<bool>` | provider 支持时用原生，不支持时落本地 BM25 |
| **工具目录缓存** | `McpToolCatalogCache`：LRU + generation 计数 | 重拉防抖 |

### 2.3 对齐结论

三家同构点：`mcp__server__tool` 命名、默认全量 tools 数组、资源元工具聚合。
分歧点在**降级路径**：Claude Code 靠供应商原生（锁定 Anthropic）；Codex 自研
本地 BM25（供应商无关）；QAQH 现有聚合工具是**间接寻址**（二跳），低于两者。

**对 QAQH 的判断**：owner 诉求 U3 的正确形态 = Codex 路线（本地检索 +
tool_result 返回完整 schema + 注册/暴露解耦）。聚合工具二跳（现状 `mcp`
聚合、`todo`/`skills` action 模式）是质量次优解，应各归其位：
- **调用质量问题** → 拆分单一职责（W1）
- **工具规模问题** → deferred + tool_search（W2）

## 3. QAQH Tool Manager 现状（2026-09-08 盘点）

### 3.1 四层结构与实测

```
暴露层  filtered_defs() = all_defs() ∩ allowed → tool_defs → 每请求 tools 数组
授权层  category_of()（两层查）+ risk + D5 mcp__ 快路径
分发层  route: handlers.get || dynamic.get        ← 与 allowed/tool_defs 无关（已解耦 ✓）
注册层  handlers: BTreeMap（内置, ToolHandler）
        dynamic:  BTreeMap（MCP,   DynamicTool + allowed_raw 重应用）
```

**实测 schema**（临时探针，serde 序列化全量 ToolDef）：
18 工具 / 15,517 B ≈ 4.4–5k tokens 每请求。大头：`todo` 2705、`edit` 1845、
`pwsh` 1087、`bash` 1058、`read` 1054、`skills` 967。

### 3.2 关键事实（决定改造面大小）

- **分发已解耦**：`resolve route` 直查注册表，不看 allowed/tool_defs——deferred
  模式的"注册即可调"地基**已经存在**，只需动暴露层。
- **"Unknown tool" 双语义未拆**：`prepare_req` 的 allowed 检查（子代理白名单）
  与词汇表缺失都报 Unknown——deferred 需要把"不在暴露面"与"不可调用"拆开。
- **MCP 动态层已有完整生命周期**：投影批次（回合边界全量重建）、热重载
  （apply_config diff 保连 + reloader 补 prime）、allowed_raw 重应用（观察项①）。
  deferred 只需在投影出口加一档过滤。

## 4. 设计总览：三条独立工作流

```
W1 聚合工具拆分（todo/skills）     ← U2，调用质量，独立可先行
W2 exposure + tool_search         ← U1/U3 + MCP 缓存关切
W3 缓存失效诊断（可观测性）        ← 对齐 Claude Code 遥测
```

三条无相互阻塞：W1 与 W2 正交（拆分后的工具 Direct 在场，见 §5.1 决策）；
W3 纯观测。

## 5. 设计细节

### 5.1 W1：聚合工具拆分（先做，收益最直接）

**拆分对象**：action 枚举封闭的聚合工具。`todo`（create/insert/set/list）、
`skills`（activate/list/resource/validate）。`mcp` 聚合**保留**（action 参数
是动态 server/uri/name，无法静态拆分）。

**拆分表（todo 为例）**——命名**下划线**（provider 工具名 regex
`^[a-zA-Z0-9_-]{1,64}$` 不允许点分）：

| 新工具 | 参数（单一职责 schema，结构即语义） | 旧 action | 估算 bytes |
|---|---|---|---|
| `todo_create` | `items[{title, description?}]` ≤20（或单条 title/description） | create | ~350 |
| `todo_insert` | `items` + `after_id?` / `before_id?` | insert | ~400 |
| `todo_set` | `ids[] + status`（批量同状态）或 `updates[{id, status?, evidence?, title?, description?}]` | set | ~550 |
| `todo_list` | `status?` | list | ~180 |

**owner 拍板：todo v3 混合制形态（2026-09-08，第三轮收敛）**——研究
Claude/Codex 后发现两家独立收敛**全量重写制**（TodoWrite/update_plan：
每次传整个列表、无 ID、无 insert、三态；清空=传空数组，无特判）；
Claude 另有 Task* 四件套=持久 issue-tracker（taskId+可选字段 patch，
与 QAQH 单一形态 todo_set 同构）。QAQH 取混合制：**保留 ID 分配**
（高水位单调不复用；**例外：显式清空重置序列回 T1**——owner 拍板
"清空=全新清单"，旧 ID 引用随 compact 消失，混淆窗口可忽略）+ **`todo_write` 追加语义**（items 非空=追加新
ID 条目；**空数组=显式清空**，items 缺省报错防误清空——追加制的特例
成本，两家全量制无此特判）+ **`todo_update` 单条状态**（原 todo_set
改名）+ **`todo_list` 保留**（plan 模式只读）。**删**：todo_insert
（全量/追加制下显示顺序精修无工具——回填=追加）、单条 title 便利形态
（items-only）、模型面改 title/description 路径（修改=cancel 旧条+
write 新条）。**直接替换**（无软迁移并存——产品早期无外部依赖，新旧
语义重复徒增 schema）；底层契约保留全量（HTTP/CLI 直访不受限）。
**实测**：三件套 1576B（write 708/update 534/list 334）vs 原聚合
2705B——**净省 1129B**（约 280 tokens/请求）。

**owner 追加拍板（2026-09-08，PR-DT-1 复盘）**：`todo_set` 收敛为**单一
形态** `{id, status, evidence?}`（required [id,status]，一次一条）——
`ids[]` 批量与 `updates[]` 逐条编辑从模型面移除，批量场景循环调用
（简单 schema × 循环 > 复杂 schema × oneOf）；代价：模型面不再有中途改
title/description 的路径（底层 HTTP/CLI 直访的 ids/updates 分支保留，
程序化调用不受限）。reject_fields 禁 `ids/updates/title/description/
items/after_id/before_id`。

**实测（PR-DT-1 落地后探针，2026-09-08）**：拆分四件合计 **3336 B**
（create 737 / insert 1001 / **set 单一形态后 531** / list 334；单一形态拍板前 set 为 1264）vs 聚合 2705 B——
**净 +631 B**。估算偏差根因：每工具 ToolDef 序列化的固定开销
（name/description/包装 ~300 B）×4 + `todo_set` 的 updates 嵌套 schema
结构开销；"oneOf 与参数归属说明是纯开销"的判断成立，但固定开销 4 份
抵消了参数瘦身。**W1 的真实收益是调用质量**（结构即语义、无 oneOf 弱
约束、错误消息带工具名而非 action 名）。**todo_set 单一形态拍板后字节
反转**：四件合计 2603 B < 聚合 2705 B——净 **-102 B**（updates/ids schema
结构开销是聚合膨胀的主因）。
`skills` 同构拆分排后（见 O4）。

**迁移策略**：软迁移一版（新工具 register + 旧聚合 description 尾部
`deprecated: use todo_*`）→ 稳定后删聚合。历史安全：旧会话的
`tool_call(name="todo")` 只存在于历史/审计，无重放执行路径。

**实现落点**：`todo/dispatch.rs` 的 action 分支改 4 个薄壳 register（内部
逻辑全复用）；`skills` 同构。半天 ×2 含测试。

### 5.2 W2：exposure + tool_search（本 spec 核心）

#### 5.2.1 exposure 维度（学 Codex，砍到两档起步）

```rust
pub enum ToolExposure { Direct, Deferred }   // 未来按需扩 Hidden（预算超限）

// ToolHandler（内置，编译期定）与 DynamicTool（MCP，投影时定）各加字段；
// 默认 Direct —— 不配置时行为与现状完全一致（零回归）。
```

改动面（分发/授权层**零改动**——已解耦）：
- `filtered_defs`/`all_defs`：排除 Deferred（不进模型面 tools 数组）
- `set_allowed` 的 known 过滤：断言加 deferred 层（allow 名单认可 deferred 名）
- 新注册入口：`register_with_exposure` / DynamicTool 带 exposure

#### 5.2.2 `qaqh_tool` 内置工具（U3 的直接实现，**owner 拍板命名**）

**命名**：`qaqh_tool`（owner 指定，替代 tool_search）；参数单字段 `{ query: string }`。

**第一版语义（全载入默认下的定位）**：schema 查询/发现工具——工具集默认
**全 Direct 在场**（owner 拍板，见 D2），`qaqh_tool` 用于：模型不确定参数
形状时拉取完整 schema、确认某能力是否存在、以及未来 deferred 模式的展开
入口。返回形态不变：

- **tool result 里的 schema 持续可见**（历史消息），一次搜索多轮复用；
  compact 摘要掉后重新 search 自愈
- 供应商 API 对 tool_call name **不校验**是否在 tools 数组（模型输出自由
  透传，分发在 client）——已由 Codex 实践验证
- 检索实现分阶段：**先线性**（name/description 子串+关键词匹配，≤50 工具
  足够，零依赖）；工具数 >100 再引 `bm25` crate（Codex 同款）
- **description 内列工具集（family）名单**（owner 提出的"从 prompt 告知模型
  有什么工具集"的轻量替代：system prompt 不动，`qaqh_tool` description 自身
  声明可查的 family 清单）——否则模型不知道存在什么可查；后续若需要
  再升级到 system prompt 引导（见 O3）
- 返回条目带 `exposure` 语义提示（"调用的参数 schema 如下"）——不需要
  "已展开"状态机（Claude Code 的 `isDeferredToolInConversation`）：QAQH
  的分发不看暴露面，重复 search 幂等无害

#### 5.2.3 MCP 投影接入（deferred 的主战场）

- `projection_mode` 配置（`[mcp] projection = "full" | "deferred"`，默认 full）：
  - full：现状——MCP 工具全 Direct + 聚合钉底
  - deferred：MCP 工具全 Deferred + **聚合工具保留 Direct**（发现入口）
- **收益（对贯穿性关切）**：MCP 热重载 added/removed/list_changed → deferred
  索引变化，**tools 数组不变 → prompt cache 不失效**（变化成本降为按需搜索）
- `alwaysLoad` 语义（学 Claude Code）：per-server 配置
  `[mcp.servers.x] always_load = true` → 该 server 工具强制 Direct。
  不实现启动阻塞语义（QAQH 的 prime 已是 fire-and-forget；schema 缺失时
  该 server 工具自然不出现在投影，下次 refresh 补上——与现状一致）
- 未来可选（登记不实施）：`tool_is_model_visible`（尊重 `_meta.ui.visibility`）、
  schema 字节预算超限 Hidden（8 KB/64 KB 阈值学习 Codex）

### 5.3 W3：缓存失效诊断（可观测性）

现状 `prepared_request_key(messages, tools)` 已感知变化但无**原因**。补一个
原因枚举对齐 Claude Code 遥测：

```
tools_changed / mcp_resources_changed / system_changed / messages_compacted / model_or_provider_changed
```

落点：回合边界 tool_defs 与上轮快照 diff（token_calibration.capture 已有
`changed.push("tool_defs")` 雏形——扩展为枚举 + 日志/SSE 事件）。小 PR。

## 6. 关键设计决策（待 owner 拍板）

| # | 决策点 | 提案 | 理由 |
|---|---|---|---|
| D1 | todo/skills 拆分的暴露档 | ✅ 拍板：**全 Direct**（2026-09-08，拆分后 schema 打平或更省，高频工具 deferred 反增一跳） |
| D2 | 哪些内置工具可 Deferred | ✅ 拍板（2026-09-08）：**第一版全部 Direct 全载入**，deferred 不默认开；`qaqh_tool` 上线为 schema 查询工具（在场工具也可 query 确认 schema）；后续再考虑 prompt 告知哪些需要 query（见 O3） |
| D3 | 词汇表语义 | `tool_defs`（暴露面）与"可调用集合"（registry）**显式分离**；`authorize_call`/分发查 registry | 对齐 Codex 注册/暴露解耦；本就半解耦，补齐即可 |
| D4 | allow 即 Direct | `set_allowed` 显式点名的工具**自动升 Direct**（学 Claude Code alwaysLoad） | custom 工具模式语义不变（点名的要在场）；白名单与暴露面的绑定关系保持 |
| D5 | `mcp` 聚合工具去留 | **保留 Direct**（deferred 模式下是唯一发现入口） | 动态参数无法静态拆分；`RefreshMcpTools` 思路并入它的 `refresh` action |
| D6 | 检索实现 | ✅ 拍板（2026-09-08）：线性先行（`qaqh_tool` 第一版即线性），>100 工具再 BM25 |
| D7 | 模型侧刷新 | 聚合工具加 `refresh_tools` action（重拉已连接 server 工具清单，失败保旧集——学 Claude Code "never dials"） | 补齐"模型可调"闭环；owner 触发（热重载）之外的模型自愈路径 |

## 7. 分步 PR 规划

| PR | 内容 | 出口命令 |
|---|---|---|
| **PR-DT-1** | W1：todo 拆分（4 工具 + 软迁移标注 + 旧聚合 deprecated） | `cargo test -p qaqh-workspace --lib todo` 全绿 + schema 探针复查（总字节下降） |
| **PR-DT-2** | W1：skills 拆分（同构） | `cargo test -p qaqh-workspace --lib skills` 全绿 |
| **PR-DT-3** | W2-①：ToolExposure 维度 + filtered_defs 过滤 + allow 即 Direct（D4） | 既有测试全绿（默认 Direct 零回归）+ exposure 单测 |
| **PR-DT-4** | W2-②：`qaqh_tool` 工具（线性检索 + tool result 返回完整 ToolDef + description 列 family 名单） | 新增 `qaqh_tool` 集成测试（搜索→返回 schema→模型视角可直接调用） |
| **PR-DT-5** | W2-③：MCP `projection_mode` + deferred 投影 + `always_load` per-server | `cargo test -p qaqh-mcp --test projection_modes` + 热重载下 tools 数组不变断言（缓存不失效实测） |
| **PR-DT-6** | W3：缓存失效原因枚举 + 聚合工具 `refresh_tools` action | 单测 + daemon 日志验证 |
| **PR-DT-7**（可选） | BM25 检索升级 + `tool_is_model_visible` + 字节预算 | 触发条件：MCP 工具总数 >100 / 出现真实可见性需求 |

## 8. 风险与开放问题

- **R1（deferred 的行为风险）**：模型对未在 tools 数组的工具调用意愿依赖
  search 质量与模型习惯——deferred 模式默认关、引导开启，full 行为不变。
- **R2（allow 即 Direct 的边界）**：D4 使 allowed 名单成为"强制可见"开关，
  子代理白名单（prepare_req 检查）语义需同步审查——子代理工具面是否也应
  支持 deferred（暂不，子代理工具数小）。
- **R3（W1 与 TUI/webUI 的联动）**：todo 拆分影响工具清单展示（UI 侧无硬
  依赖，纯展示）；webUI 无 MCP 管理面（缓行中），无冲突。
- **O1**：`skills` 拆分的粒度（4 action 的 schema 形状待拆时细化）。
- **O2**：deferred 模式下 token_calibrator 的 request_key 语义（tools 数组
  稳定 → key 更稳，校准命中应提升——W3 可观测验证）。
- **O3（owner 提出待设计）**：从 prompt 告知模型"哪些需要 query / 工具集
  清单"——第一版以 `qaqh_tool` description 列 family 名单替代，是否升级
  到 system prompt 引导后置观察。
- **O4（owner 担忧 + 澄清）**：skills 的嵌套 query 稳定性——skills 本身已是
  二级路由（activate → skill 内部资源发现，有嵌套 query 情况）。**澄清：
  `qaqh_tool` 只负责工具 schema 发现，不参与工具内部的路由/内容发现**——
  skills 拆分后 activate/list/resource/validate 均为一级工具，其嵌套资源
  发现是工具运行时行为，与 query 层正交不叠加；据此 skills 拆分（W1-2）
  不被阻塞，但排后实施，先观察 todo 拆分实战效果再定。
