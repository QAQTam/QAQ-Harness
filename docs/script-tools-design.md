# QAQH 设计：script 工具（模型自产脚本的第一等注册）

状态：**设计稿（待 owner 评审，有时间再议）** · 2026-09-08 · 前置讨论：本会话
（"qaqh_ext_tool 或 script"提案 → 三档信任模型 → 注册契约）

## 1. 定位：第四层

| 层 | 本质 | 现状 |
|---|---|---|
| 内置工具 | 编译期固定 | 20 个，改一行要重编 |
| Skills | **知识**（怎么做的说明书） | ✓ 已有 |
| MCP | **外部进程**提供的工具（重：协议/连接/子进程生命周期） | ✓ 已有 |
| **script** | **模型自产的本地工具**（轻：无进程常驻、改完即生效） | ❌ 本 spec |

价值：模型反复执行的多步操作（跑门禁四连、日志模式查询、格式转换）固化成
脚本一次调用，且模型自己迭代到稳定（写→跑→修闭环全自动）。skills 教模型
**怎么做**，script 是**做好的成品**。安装前置：python（开发者生态缺省存在，
安装器 recommend）。

## 2. 三档信任模型（激励设计的核心）

**原则：不用 prompt 说教，用权限差异造结构性激励**——bash 万能且便宜时模型
永远选 bash；让 trusted script 通道比 bash 便宜，行为自然收敛。

| 档 | 来源 | 执行权限 | 模型的理性选择 |
|---|---|---|---|
| **trusted script** | 安装预置 或 owner 审查后标记 | **AutoApprove**（快） | ✅ 优先用 |
| **untrusted script** | 模型刚生成的 | 走 exec 审批链（与 bash 同） | 生成后请 owner 确认升级 |
| **bash** | 每次都是新命令 | 每次都审 | 兜底 |

**安全红线（上轮讨论的修正，必须遵守）**：

1. **trust 标记不得放在脚本文件里**——脚本文件模型可写（热更新的前提），
   模型可以自己写 `trusted: true`（特权提升漏洞）。trust 状态独立存
   `~/.config/qaqh/scripts/trust.json`（`{"trusted": ["lint_gate", ...]}`），
   模型工具面不可达（write 的 workspace 边界之外，与会话资产同级保护）。
2. **hash 绑定降级**：trust.json 存脚本的 content hash——文件被改（无论谁改）
   自动降级 untrusted。防"先信任再偷改"。
3. "防脚本删文件"的落点：untrusted 走审批链时内容人眼可见（含 `rm` 即拦）；
   trusted 的内容是 owner 亲眼审过才标——信任链无快捷方式。

## 3. 注册契约（三个部分）

**① 文件**：`~/.config/qaqh/scripts/<name>.py`，头部 **QAQH-SCRIPT
frontmatter**（学 skills 的 SKILL.md 模式）：

```python
#!/usr/bin/env python3
"""运行 QAQH 四门禁并汇总结果。

QAQH-SCRIPT:
  name: lint_gate
  description: Run the four QAQH gates (check/clippy/fmt/test) and summarize
  params:
    crate:   {type: string, required: true, description: "crate name"}
    fix:     {type: boolean, required: false, description: "auto-fix"}
"""
import json, sys
args = json.load(sys.stdin)   # 参数走 stdin JSON（无 shell 注入面）
...
print(result_text)            # stdout = tool result（fold 兜底）
```

**② 调用契约**：入参 **stdin JSON**（按 frontmatter params 校验——第一版
required + 基本型）；**stdout 文本直通**为 tool result；exit code 语义
（0=ok / 非 0=error，stderr 附加）；超时/取消复用 exec 基建（进程组/超时
硬顶/取消/24K fold）。

**③ 执行模型**：每次调用 spawn python（冷启 ~100ms，简单可控）——**不做
常驻 worker**（第一版；热度证明后再议）。

## 4. 注册管线（与 ToolManager 的结合方式）

```
scripts/ 扫描 → frontmatter 解析 → 投影 ToolDef (name/description/input_schema)
  → register_dynamic（MCP 投影同款：碰撞拒绝 / allowed_raw 重应用）
  → 文件 watch → 单脚本增量重注册（比 MCP 全量 replace 轻）
```

- **坏脚本跳过 + 日志告警**，不炸 daemon（脚本库是用户资产；与 config 的
  fail-fast 语义相反：config 错 = 拒起，脚本错 = 跳过该脚本）
- 描述语义与内置/skills/MCP **收敛到同一份 ToolDef**——四条来源一份词汇，
  `qaqh_tool`（PR-DT-4）检索零适配

## 5. PR 拆分

| PR | 内容 | 出口 |
|---|---|---|
| **PR-SC-1** | frontmatter 解析 + 注册管线 + 坏脚本跳过 + 动态注册单测 | `cargo test -p qaqh-workspace --lib script` |
| **PR-SC-2** | 执行路径（spawn/stdin JSON/超时取消/fold）+ 权限接入 + trust.json 查询 | 集成测试（trusted 免审 / untrusted 审批 / hash 降级） |
| **PR-SC-3** | watch 热更新 + 预置 2–3 个示范脚本（跑门禁/日志查询）+ `qaqh_tool` 索引接入 | 热更新集成测试 + qaqh_tool 搜到 script |

## 6. 顺序依赖

`qaqh_tool`（PR-DT-4）**先于** script——脚本一多，"发现有哪些脚本/参数形状"
正是 qaqh_tool 的本职，script 是它的第一批高价值客户。

## 7. 开放问题

- **O-S1**：脚本库是否需要分组/命名空间（防模型自造脚本名撞预置）——第一版
  平铺 + register_dynamic 碰撞拒绝兜底。
- **O-S2**：脚本积累的清理策略（30 天未用？）——缓，观察真实使用密度。
- **O-S3**：脚本输出是否支持结构化 JSON 模式（`{"text": ...}` 约定）——第一版
  纯文本，需要时再加。
