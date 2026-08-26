# ADR：移除 bun+ts 技术栈 & ToolProgress 旧通道移除记录

> 日期：2026-08-24 · 状态：已决策 + 移除已落地 · 附调查纠错记录
> 影响：QAQ-Harness（domain/msgloop/ringing/runtime）

## 决策一：移除 bun + TypeScript 全栈

| 领域 | 替代 |
|---|---|
| TUI | ratatui（584 行 vs 2804 行、交互门齐全、qaqh-client 协议同源） |
| Web | axum 直接服务 |
| ACP/MCP 对接 | 引入 bun 的原始动机；原生 Rust 库出现后动机消失 |

## 决策二：彻底移除 ToolProgress 直发通道（agent2ui 传统包装）

背景：该协议 8 月起事实上迁移至 timeline 双轨并存，直发版**零消费者**
（WinUI/ratatui/webui 三前端均无此事件分支），但代码残留导致每次重新开发
LLM 反复尝试从它接线。2026-08-24 连根拔除：

| 文件 | 改动 |
|---|---|
| `qaqh-domain/event.rs` | 删 `ToolEvent::ToolProgress` 变体 + delivery/tool_call_id match 臂 + 测试 |
| `msgloop/engine_tool.rs` | 删 emit_progress_tail 直发段；**tails 4KB 尾窗缓冲整体删除**（仅直发版消费）；drain 循环保留批量收取，改发 timeline 增量 |
| `qaqh-ringing/envelope.rs` | round_trip 测试样本换 ToolCallPrepared |
| `runtime/ringing/router.rs` | replaceable_key_for 删臂；`ReplaceableKey::ToolProgress` → `ToolPrepared`（键被 ToolCallPrepared 复用，终态 flush 语义不变）；流控测试样本换 |
| `runtime/ringing/hub.rs` | first_progress 持久化特判删；测试 helper/断言换 prepared |
| `runtime/ringing/outbox.rs` | progress helper → prepared（7 调用点） |
| `runtime/ringing/tool_progress.rs` | **整文件删除**（16ms Coalescer，273 行零使用死代码） |

验证：5 crate check 全绿 + 全部测试套 ok（0 FAILED）；winui-app/tui-rs 两仓 check 不受影响。
保留：timeline 新轨道全链（TimelineIntent → 存储 append → TimelineEvent 广播）与 exec 生产管道。

## 调查纠错记录（重要，防以讹传讹）

初版结论「daemon 发 4KB 窗口快照、前端当增量拼、四层膨胀」**有一半错误**：

- ❌ 错的一半：「timeline 版也发窗口快照」。实际 `emit_timeline_tool_progress`
  传的是 `event.chunk` **原始增量**；`runtime/timeline.rs append_tool_progress`
  的 push_str 与类型注释 "appended to progress buffer" **语义自洽且正确**；
  三前端的 push_str 拼接的是真增量 = 工具完整输出累积，属设计行为非泄漏。
- ✅ 对的一半：直发版 chunk 确为 4096B 滑动尾窗快照（seq_start/seq_end/
  truncated 元数据），若被消费会线性膨胀——但它从未被任何前端消费，
  现已连同尾窗缓冲一起移除，混淆源不复存在。
- ⚠️ bun-tui「prepare 态内存线性增长」真凶未定：progress 无辜出列后，
  Ink spinner 每帧全量重绘 / SSE 空转重连循环嫌疑回升。webui 栈将整体
  移除，不再追查。
- 潜在改进挂账：timeline 存储的 tool.progress 无上限（大输出工具会让
  会话文件变大），如需治理另行立案（建议 CONTENT_TAIL_BYTES 同款尾部截断）。

---

## 决策三：统一 XML 信封方言（2026-08-24 追加）

多运营商背景下，废弃 DeepSeek 专属的 `<｜DSML｜...>` 全角竖线形态思路，
建立纯字母数标签名的统一信封方言（避开 `<|...|>` special-token 命名空间，
ChatML/Llama/DeepSeek 均已占用竖线形态）：

| 载荷 | 形态 |
|---|---|
| 工具结果 | `<qaqh_tool_result status="ok" truncated="false" error_code="X" retryable="true">` 正文 `</qaqh_tool_result>` |
| 子代理回传 | `<qaqh_subagent_result name="x" state="completed\|error\|timeout\|cancelled" exit="N">` 正文 `</qaqh_subagent_result>` |
| skills 注入 | 维持既有 `<skill_context_envelope version="2">`（本就是 XML，纳入同一方言） |

落地：
- `ToolResult::render_xml_envelope()` 单一实现，正文零转义省 token；
  正文字面闭合标签替换为 `<\/qaqh_tool_result>` 防嵌套假闭合；
- 三消费点切换（gate/openai.rs、gate/responses.rs、message/context_flow.rs）；
- subagent 注入文本换信封；WinUI `parse_subagent_injection` **新旧双读**
  （历史 transcript 重放兼容），`[SUBAGENT ...]` 旧前缀保留解析直至
  dsml_compat_count 同款观察期后再清；
- DSML 入站救援解析器（tool_parser.rs）**原样保留**——它救的是模型输出侧
  的伪调用，与出站格式无关，待触发率归零再立案移除。
