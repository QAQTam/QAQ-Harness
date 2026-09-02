# 图片传递机制调研（A-0）与端点策略决策备忘录（A-1）

调研日期：2026-09-02。信息来源：OpenAI 官方 OpenAPI 规格 v2.3.0（openai-openapi master）、
Anthropic 官方文档（platform.claude.com/docs，vision.md + files.md，当日拉取）。

## 一、能力矩阵（三端点 × 图片传递机制）

| 机制 | OpenAI Chat Completions | OpenAI Responses | Anthropic Messages |
|---|---|---|---|
| inline base64 | ✅ data URI（`image_url.url`） | ✅ data URL（`input_image.image_url`） | ✅ `source: {type: base64}` |
| URL 源（provider 主动拉取） | ✅ http(s) URL | ✅ 完全限定 URL | ✅ `source: {type: url}`（仅公网可达；Bedrock/Vertex 不支持） |
| file_id 引用（上传一次多次引用） | ❌ 不支持（`image_file`+file_id 仅存在于已 deprecated 的 Assistants API） | ✅ `input_image.file_id`（Files API `purpose=vision`） | ✅ `source: {type: file, file_id}`（Files API；jpeg/png/gif/webp） |
| detail 粒度 | auto/low/high | auto/low/high/**original** | —（固定价） |

### 关键佐证
- OpenAI spec `ChatCompletionRequestMessageContentPartImage`：`url` 仅 "URL of the image or
  the base64 encoded image data"，无 file_id 字段。
- OpenAI spec `InputImageContent`（Responses）：`image_url`（URL/data URL）**与** `file_id`
  （"The ID of the file to be sent to the model"）并列可选。
- Anthropic vision.md：image content block 支持三种 source（base64 / url / file_id），
  并明确 Files API 的适用场景：**"In multi-turn conversations and agentic workflows, each
  request resends the full conversation history. If images are base64-encoded, the full
  image bytes are included in the payload on every turn... Uploading images to the Files
  API and referencing them by file_id keeps request payloads small regardless of how many
  images accumulate in the conversation history."** —— 官方定位与 qaqh 的 A 阶段目标
  （多轮会话图片 payload/内存膨胀）完全重合。
- Anthropic Files API 限制：Bedrock / Google Vertex 不可用（仅 base64）；ZDR 不适用；
  file 为 **workspace 级作用域**（非会话/用户级，官方警告多租户需 workspace 隔离）；
  有 `expires_at` TTL 管理。

## 二、对 qaqh 的含义

1. **URL 源对我们无用**：需要 provider 能公网访问该 URL；本地磁盘文件做不到（除非公网
   托管，隐私不可接受）。排除。
2. **file_id 是真正的 payload 优化**：Anthropic + OpenAI Responses 两家官方 API 支持，
   且 Anthropic 官方文档直接指明这是多轮会话图片膨胀的解法。但覆盖面仅限两家官方端点；
   qaqh 支持的大量 OpenAI 兼容第三方端点（DeepSeek/Qwen/GLM/OpenRouter/vLLM/Ollama…）
   走 chat_completions，**只有 base64/data URI**。
3. **base64 是唯一全端点通用机制**。任何"图片外置"方案若想覆盖全部端点，请求构建时
   仍需产出 base64 —— 外置化的收益在**本地常驻内存**（daemon RAM），而非线上 payload。
4. 成本视角：file_id 并不降低图片 token 计费（图片每次请求仍按 token 计），收益主要是
   请求体积/延迟；配合 provider 端 prompt caching 可摊薄重复成本。

## 三、端点策略：三端点并存 vs 锁定一个

**结论：不锁死。三端点 = 三个传输适配器，覆盖三个生态，缺一不可。**

- chat_completions：**兼容生态的事实标准**（几乎所有第三方模型服务与本地推理栈都实现
  它）。锁死 Responses/Anthropic 会砍掉大部分第三方模型场景。
- Responses：OpenAI 官方未来方向（stateful、原生工具、file_id），但第三方兼容层基本
  不实现。锁死它 = 放弃兼容生态。
- Anthropic Messages：Claude 原生能力最全的通道（cache_control、Files API、thinking）。
- 三者已共享内部统一消息模型（`ContentBlock`）与薄适配器（gate lowering），并存的维护
  成本 = 各端点 lowering 的特性差（本就存在且已隔离在各自适配器内）。

**架构姿态**：内部模型统一 + 每端点薄适配 + **能力探测**决定图片机制分层：
- L0（基线，全端点）：磁盘外置 + 请求构建时 base64 降格 —— 三端点零分叉；
- L1（增强，按端点能力 flag）：Anthropic `file_id` / OpenAI Responses `file_id`，
  命中时 payload 只带引用；需处理上传生命周期（TTL/失效重传）与不可用回退。

## 四、A-2 基线设计（磁盘外置 + 请求时按需加载）

- 新增 `ContentBlock::ImageRef { image_id, mime_type, width, height, bytes_len, sha256 }`
  持久化进消息；字节落盘 `{data_dir}/images/{sha256}.{ext}`（**内容寻址去重**：同一图片
  多轮引用只存一份）。旧 `ContentBlock::Image` 变体保留（读侧兼容），写侧一律 ImageRef。
- gate 三端点 lowering：ImageRef → 读盘（几 MB，ms 级）→ 走既有 base64/data URI/
  Anthropic source 路径，输出与现在 byte-exact。
- `IMAGE_REGISTRY` 索引化：store_image 存 ref（sha256 → 磁盘路径），peek_image 读盘编码
  —— registry 从"全量 base64 常驻"变"索引表"，直接消除 D 阶段诊断的结论②的根因。
- lifecycle 重建（state/lifecycle.rs:145-162）：从 ImageRef 重建 registry（校验磁盘文件
  在场），`[Image #N]` 时序稳定性语义不变。
- UI 显示：图片字节经磁盘流式端点按需取（复用 `/ringing/v1/content` 的鉴权模式，
  但直读磁盘、不进内存 ContentStore，避免重新引入常驻）。
- 归一化时机不变：入口（上传/read_image 产出）normalize 一次 → 落盘；读侧不再变换，
  保证 byte-exact。

## 五、来源
- OpenAI 规格：https://github.com/openai/openai-openapi （master openapi.yaml v2.3.0；
  L31481 image_url part、L43685 Assistants image_file（deprecated）、L68133 Responses
  InputImageContent、L37457 Files purpose 含 vision）
- Anthropic：https://platform.claude.com/docs/en/build-with-claude/vision 、
  https://platform.claude.com/docs/en/build-with-claude/files （2026-09-02 拉取）
