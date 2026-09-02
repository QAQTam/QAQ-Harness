# LLM 网关公共 Core + 特判适配 + TOML 优先设计（DRAFT）

> 状态：**DRAFT** 2026-09-01 · 面向 `qaqh-gate` → `公共 core + 特判参数 + TOML` 演进，暂不改码，仅作为后续 PR 的设计基准
> 上游约束：`docs/crate-boundary-proposal.md`（crate 边界）、`docs/PLAN.md`（执行计划）；本设计不推翻既有边界，仅细化网关内部分层
> 官方基线：`OpenAI Chat Completions / Responses / Anthropic Messages` 三纯协议（见 §1），各 provider 增量以参数标记

---

## 0. 目标与非目标

**目标**
- 以 `OpenAI SDK` 为纯端点协议基线，抽 `公共 core`（HTTP/SSE/重试/取消/消息转换骨架），`provider` 差异仅通过 `TOML` 声明式参数特判，不新增 `if provider=="xxx"` 分支
- 支撑当前已接入的 `deepseek / glm(bigmodel) / mimo / kimi(moonshot) / qwen(dashscope) / minimax / doubao / openai / openrouter / zcode / deepseek-web / opencode-go` 三协议并存，且新增 `provider/endpoint` 零代码
- 优先服务 `harness` 自身（`workspace` 版本内 `TOML merge`），为后续 `crate.io liteSDK` 抽离预留接口但不立即发布

**非目标**
- 不新增第四协议（`deepseek FIM / files` 属工具域，不入网关）
- 不改变 `qaqh-types::Message/ToolDef` 存储形态与 `store.rs` 持久化
- 本阶段不发 `crate.io`，不改 `cargo publish` 元数据与 `semver`

---

## 1. 纯端点协议基线（SDK 视角）

以官方文档为唯一真源，`harness` 仅做 `SDK` 语义的搬运：

| 协议 | 方法 | 官方文档 | 核心形态 | 终结语义 |
|---|---|---|---|---|
| `chat_completions` | `POST /chat/completions` | `https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create`（镜像 `https://api-docs.deepseek.com/zh-cn/api/create-chat-completion`）、`https://platform.claude.com/docs/en/api/messages/create` 对照 | `model, messages[{role:system/user/assistant/tool, content:string\|[{type:text\|image_url{url:dataURL\|https}, detail?}]}], tools[{type:function,function:{name,description,parameters}}], tool_choice, stream, temperature, top_p, max_tokens, response_format, stop` | SSE `data: [DONE]` |
| `responses` | `POST /responses` | `https://developers.openai.com/api/reference/resources/responses/methods/create`（镜像 `https://api-docs.deepseek.com/zh-cn/api/create-response` / `guides/responses_api`） | `model, input:string\|[EasyInputMessage{role,content:[input_text\|input_image{image_url,file_id,detail}\|input_file]} \| FunctionCall{call_id,name,arguments} \| FunctionCallOutput{call_id,output:string\|[input_text\|input_image\|input_file]} \| reasoning \| web_search_call \| custom_tool_call], instructions, reasoning{effort}, text{format}, tools, tool_choice, user, include, background, previous_response_id, conversation, store, max_output_tokens` | SSE `response.completed / incomplete / failed` 无 `[DONE]` |
| `anthropic/messages` | `POST /v1/messages` | `https://platform.claude.com/docs/en/api/messages/create` | `model, messages[{role:user\|assistant, content:string\|[Text\|Image{source:base64\|url\|file}\|Document\|Thinking\|ToolUse\|ToolResult\|ServerToolUse]}], system:string\|[Text], thinking{type,budget_tokens}, tools[{name,input_schema}], tool_choice, max_tokens, stream, stop_sequences, temperature, top_p, output_config` | SSE `message_start / content_block_delta / message_delta / message_stop` |

`vision` 与 `files` 仅为 `content` 的 `image_url / file_id / input_image / input_file / document` 形态差异，不单设协议。

当前 `qaqh-gate/src/{chat_completions_api.rs:1, responses_api.rs:1, message_api.rs:1}` 已按此三文件物理隔离，`lib.rs:44 chat_stream` 仅按 `ProviderKind` 分发，符合纯协议分层。

---

## 2. 共性抽取（已具备，保留）

| 层 | 共性 | 位置 | 说明 |
|---|---|---|---|
| 传输 | `reqwest` 复用 `GLOBAL_CLIENT`、`tokio` 单线程 `FALLBACK_RT`、`SseDecoder`、`SSE_POLL_INTERVAL 50ms + cancel AtomicBool` 轮询、`MAX_RETRIES 5` 指数退避 `2/4/8/16/32s`（对 `429/500/502/503` 及 `EmptyStreamEof`） | `chat_completions_api.rs:28,61,321` `message_api.rs:31` `responses_api.rs:25,60` | 三协议完全复用，仅 `decoder` 实现略有差异（`sse.rs:4` 注释已说明） |
| 消息 | `qaqh_types::Message {role, content: Vec<ContentBlock>}` + `ToolDef` 为网关内外统一 `IR`，`store.rs:606` 追加 `ContentBlock::Image`，`gate` 仅做 `IR -> provider JSON` 投影 | `qaqh-types/src/message.rs:13` `qaqh-types/src/provider.rs:56` `qaqh-message/src/store.rs:606` | 不引入 `provider` 私有 `IR` |
| 工具 | `tools: [{type:function,...}]` ↔ `tool_calls / function_call / tool_use` 互转，`input_schema / parameters` 透传，`sanitize_openai_schema` 仅 `responses` 需要 | `chat_completions_api.rs:154` `responses_api.rs:459` `message_api.rs:345` | `sanitize` 限 `responses`，其余直通 |
| 推理 | `EFFORT_LADDER [low,medium,high,xhigh,max]` + `EFFORT_OFF -> low` 归一 (`types.rs:6`) + `clamp_effort_to_allowlist` 稀疏钳位 | `qaqh-gate/src/types.rs:6,30` | `harness` 强制思考，仅 `allowlist` 钳位 |
| 配置 | `EndpointSpec` 26字段 + `ResponsesCompat` 7字段 为唯一特判载体，`gate` 仅读 `ProviderConfig`，不写 `if provider_id=="..."` | `qaqh-types/src/provider.rs:56` `qaqh-gate/src/types.rs:165,216` | 新增 `provider` 只改 `registry.rs` |

---

## 3. 特判参数化清单（`TOML` 可声明，代码零分支）

> 全量字段即 `EndpointSpec::default()` (`provider.rs:184`)，缺省即纯协议语义；`TOML` 仅覆盖增量

| 类别 | 字段 | 纯协议缺省 | 典型增量（已验证） | 影响面 |
|---|---|---|---|---|
| 路由 | `protocol` | `openai` | `responses / anthropic` | `lib.rs` 分发 |
| 路由 | `base_url` | — | `deepseek https://api.deepseek.com, glm https://open.bigmodel.cn, qwen https://dashscope.aliyuncs.com, kimi https://api.moonshot.cn, mimo https://api.xiaomimimo.com` | `build_*_url` |
| 路由 | `chat_path / responses_path / anthropic_path / models_url / balance_path` | `/chat/completions / /responses / /v1/messages / /models` | `qwen /compatible-mode/v1/chat/completions, glm /api/paas/v4/chat/completions, qwen responses /compatible-mode/v1/responses, zcode /api/anthropic/v1/messages` | `build_*_url` |
| 思考 | `thinking_mode` | `OpenAi {type:enabled}` | `Qwen QwenEnableThinking, MiniMax MiniMaxAdaptive` | `chat_*_openai` 请求体 |
| 思考 | `supports_thinking` | `true` | `deepseek responses false, openrouter false, opencode-go false` | 是否发 `thinking` |
| 思考 | `thinking_budget_large` | `false (1k-16k)` | `zcode true (16k-96k)` | `message_api.rs:792` |
| 思考 | `supports_reasoning_effort` | `true` | `openrouter false` | 是否发 `reasoning_effort` |
| 思考 | `effort_allowlist` | `None` | `openrouter [max,high,low], zcode [low,medium,high,xhigh,max]` | `clamp_effort_to_allowlist` |
| 思考 | `responses_effort_max / responses_send_include / responses_echo_reasoning_content` | `high / true / true` | `deepseek max/false/true, mimo high, kimi max` | `responses_api.rs:527` |
| 缓存 | `cache_field` | `PromptCacheHitTokens` (DeepSeek) | `qwen/glm/zcode PromptDetailsCached, kimi UsageCachedTokens, mimo/minimax None` | `usage` 解析 |
| 流 | `include_stream_usage` | `false` | `deepseek openai true` | `stream_options.include_usage` |
| 工具 | `tool_call_content_null` | `false` | `openrouter true` | `assistant tool_calls` 补 `content:null` |
| 工具 | `supports_reasoning_content` | `true` | `openrouter/mimo/qwen responses false` | 历史 `reasoning_content` 是否回放 |
| 工具 | `require_provider_parameters` | `false` | `openrouter true` | `provider.require_parameters` |
| 工具 | `responses_web_search / responses_echo_web_search_call / responses_search_function_alias / responses_supports_user` | `true/true/true/None/true` | `deepseek alias=qaqh_search, opencode-go responses web_search false` | `responses compat` |
| 视觉 | `supports_image_tool` | `false` | `glm/zcode/openrouter/opencode-go true, deepseek 待补 true` | `registry.rs:533 image_tool_enabled` 决定 `read_image` 是否暴露 |
| 视觉 | `image_models` | `None` (=全量) | `glm [glm-5.3-flash, glm-5v*,glm-4.6v*...], openrouter [google/gemini*,openai/gpt-4o*...], deepseek [deepseek-v4-flash-vision-exp]` | `image_model_supported` 前缀通配 |
| 视觉 | `detail` (工具入参) | `auto` | `deepseek responses low/high/original/auto, anthropic 无, chat 无` | `read_image` 透传（新增） |
| 状态 | `stateful` | `false` | `deepseek-web cdp true` | `filter_stateful_messages` 仅发增量 |
| 状态 | `supports_tail_system` | `true` | `—` | `skill envelope` 尾 `system` 合法性 |
| 其它 | `has_balance / do_sample / beta / user_id_mode / prompt_cache_key` | `true / None / false / None / None` | `glm do_sample=false, qwen has_balance=false, opencode-go prompt_cache_key=seed` | 旁路 |

`Files API`（`deepseek file{file_id,file_data} / anthropic source.type=file + header files-api-2025-04-14`）暂不入 `core`，`vision` 仅 `base64` 已满足 `5MiB/2000px` 归一预算；如后续放宽再以 `supports_file_api:bool + file_upload_path` 增量参数接入，失败回退 `base64`。

---

## 4. TOML 优先架构

```
assets/providers.toml          # 随 crate 发布的缺省全量（include_str! 兜底，版本化）
  └─ providers: Vec<ProviderSpec>  # 即 provider.rs:231 的序列化形态

~/.config/qaqh/providers.override.toml  # 用户覆盖（可选，watch 监听）
~/.config/qaqh/config.toml [providers]  # 兼容旧路径，优先级：override > config.toml > assets

qaqh-config/src/registry.rs:497 providers() 
  = parse assets TOML 
  -> merge(override TOML)  # 按 (provider_id, endpoint_id) 去重，后者覆盖非空字段
  -> Vec<ProviderSpec>      # 对外 API (find_provider/find_endpoint/image_tool_enabled) 保持不变

qaqh-gate/src/types.rs:165 ProviderConfig::from(EndpointSpec) 纯映射，无 provider 特判
```

**TOML 形态即 `EndpointSpec` 的 `serde` 形态**（`provider.rs:56` 加 `#[derive(Serialize,Deserialize)]`），字段名与 Rust 字段 `snake_case` 一一对应，缺省字段自动 `Default`，未知字段 `deny_unknown_fields=false` 以兼容未来增量。

**示例（与 `registry.rs:13` 现有 `deepseek()` 等价）：**

```toml
[[providers]]
id = "deepseek"
display = "DeepSeek"

  [[providers.endpoints]]
  id = "openai"
  display = "OpenAI-compatible"
  protocol = "openai"
  base_url = "https://api.deepseek.com"
  models_url = "https://api.deepseek.com"
  user_id_mode = "Body"
  include_stream_usage = true
  supports_image_tool = true
  image_models = ["deepseek-v4-flash-vision-exp"]

  [[providers.endpoints]]
  id = "responses"
  display = "Responses API"
  protocol = "responses"
  base_url = "https://api.deepseek.com"
  responses_path = "/responses"
  supports_thinking = false
  supports_reasoning_content = false
  responses_send_include = false
  responses_effort_max = "max"
  responses_search_function_alias = "qaqh_search"
  beta = true

[[providers]]
id = "glm"
display = "GLM (智谱AI)"
  [[providers.endpoints]]
  id = "openai"
  protocol = "openai"
  base_url = "https://open.bigmodel.cn"
  chat_path = "/api/paas/v4/chat/completions"
  models_url = "https://open.bigmodel.cn/api/paas/v4"
  cache_field = "PromptDetailsCached"
  do_sample = false
  has_balance = false
  supports_image_tool = true
  image_models = ["glm-5.3-flash", "glm-5v*", "glm-4.6v*", "glm-4.5v*", "glm-4v-plus*"]
```

**`qwen` 多模型复用**（`dashscope` 同时代理 `qwen/kimi/glm/deepseek`）：`base_url` 统一 `https://dashscope.aliyuncs.com`，`QwenEnableThinking` 与 `cache_field` 仍由 `endpoint` 声明，不按 `model` 动态切换；如后续需 `model` 级差异，再以 `model_overrides: Map<String, EndpointPatch>` 增量，不改 `core`。

---

## 5. 公共 Core 抽取边界

- **保留在 `harness`**：`qaqh-types::Message/ToolDef` 存储形态、`qaqh-message/src/store.rs` 持久化与 `read_image` 归一（`read_image/mod.rs:66 normalize 5MiB/2000px`）、`opencode_headers/stateful/skill envelope` 等 `harness` 私有策略
- **归入 `core`**：`qaqh-gate/src/{chat_completions_api,message_api,responses_api,sse,tool_parser,types}` 的纯协议转换与 `retry/cancel`，`ProviderConfig` 仅为 `EndpointSpec` 的运行时视图
- **后续 `liteSDK` 发布条件**：`qaqh-types` 去 `qaqh-skills/tokenizers` 依赖、`gate` 去 `anyhow/log + Arc<AtomicBool> callback` 改 `async Stream + thiserror`、`Cargo.toml` 补 `license/readme/repository/keywords` 与 `features [chat,responses,anthropic,rustls]`，当前不做

---

## 6. 实施步骤（不改码阶段 → 改码阶段）

**Phase 0 — 设计冻结（本文件）**：评审 `§3` 字段完备性与 `§4` 合并优先级，确认 `detail` 透传与 `mile file_id` 暂缓。

**Phase 1 — TOML 化（`qaqh-types` + `qaqh-config`）**：`provider.rs:56` 加 `Serialize/Deserialize`，新增 `assets/providers.toml`（由 `registry.rs` 现有 `providers()` 函数生成初版），`registry.rs:497` 改 `TOML parse + merge`，`config.rs:276 load_from_paths_with` 接 `watch::publish` 热重载；对外 `find_*` 单测保持 `registry.rs:660` 全绿。

**Phase 2 — Gate 零分支验证**：`types.rs:165 ProviderConfig` 仅映射，不新增 `if`；`chat_completions_api/message_api/responses_api` 三文件各保留一个 `responses_compat` 分支之外的特判即视为回归；`cargo test -p qaqh-gate --lib` 80 项 + `registry.rs:660` 10 项全绿即出口。

**Phase 3 — 可选 liteSDK 抽离**：`crates/llm-core` 新 `crate`，`qaqh-gate` 依赖 `llm-core` 薄封装 `harness` 私有逻辑，`cargo publish --dry-run` 通过后再考虑 `crate.io`。

---

## 7. 验收与风险

**验收**
- `grep -rn "provider_id ==" crates/qaqh-gate/src` → 0（除 `tool_parser` `DSML` 兼容外）
- `cargo test -p qaqh-config -p qaqh-gate --lib` 全绿；`TOML` 缺省启动与 `override` 覆盖均可 `cargo run -p qaqh-config --example load_registry` 打印 `EndpointSpec` 与 `registry.rs` 现 `providers()` 逐字段 `diff=0`
- 新增 `provider` 仅改 `assets/providers.toml` 即可 `chat_stream` 通联（以 `deepseek vision` `read_image` 为首个 `TOML` 验证用例）

**风险**
- `TOML` 未知字段兼容：`deny_unknown_fields=false` + `#[serde(default)]` 避免旧 `harness` 读新 `TOML` 崩溃
- `base_url` 注入：`TOML` 仅本机可写，`override` 校验 `url::Url::parse` 且 `scheme ∈ {https,http}` 且 `http` 仅 `localhost`
- `qwen` 代理多模型：`endpoint` 粒度不足时，`model_overrides` 再说，不提前引入 `model` 级分支

---

## 8. 参考

- 官方纯协议：`https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create`、`https://developers.openai.com/api/reference/resources/responses/methods/create`、`https://platform.claude.com/docs/en/api/messages/create`、`https://platform.claude.com/docs/en/api/messages`（含 `Image/Document/Thinking` 全量）
- 兼容明细：`https://api-docs.deepseek.com/zh-cn/guides/{responses_api,anthropic_api,vision,files_api}`、`https://docs.bigmodel.cn/api-reference/模型-api/对话补全`、`https://mimo.mi.com/docs/zh-CN/api/chat/{openai-api,responses,anthropic-api}`、`https://platform.kimi.com/docs/api/{chat,responses,messages}`、`https://platform.qianwenai.com/docs/api-reference/chat/{openai-chat,openai-responses,anthropic}`（已于 2026-09-01 批量拉取归档于本设计 `§1`）
- 现状参数表：`crates/qaqh-types/src/provider.rs:56`、`crates/qaqh-gate/src/types.rs:165,216`、`crates/qaqh-config/src/registry.rs:13`
