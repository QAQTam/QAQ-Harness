# QAQH 设计：edit 工具收敛（记录 + PR-E-3 尾巴）

状态：**PR-E-3 待评审（有时间再议）** · 2026-09-08 · 决策人：owner

## 1. 已完成收敛（背景记录）

对标 Claude Edit（4 字段极简单形态 + result 回显 originalFile/structuredPatch
基线）两步收敛：

**一刀（`4eb5738`/`5bceb7b`）**：移除 `overwrite` kind（归 write）+ read 模式
（行号系统归 read 工具；edit/read.rs 400 行死代码删除——file_query 为独立
实现非共享）。kind 7→6，hunks 变 required。

**二刀（`a92626d`）**：
- **kind 6→3**：删 `insert_after`/`insert_before`/`replace_inline`——replace
  带 context 表达插入（old=锚行，new=锚行+新内容）；regex 替换走 bash/python
- **partial → strict-only**：`Mode` 枚举删——半改状态比全失败更难恢复；
  `new_hash` 续接协议不变（只重发失败项）
- **PR-E-1 patch 回显**：edit 成功的 `data["patch"]` = unified diff **进模型
  投影**——模型下一轮持内容基线，修复"盲改循环"（此前 diff 注释明确
  "绝不进入模型投影"，模型只拿 hash 元数据，old 复述全靠记忆）

edit 模块累计 4349 → 2571 行（-41%）。当前形态：**3 kind**（replace /
prepend_file / append_file）+ replace_all + hint_line 窗口 + expected_hash
CAS + dry_run/confirm_apply 两段式。

## 2. PR-E-3（待评审）：fuzzy tier3 淘汰 + 失败提示形态

### 2.1 现状定位链

```
tier1 精确逐行全等 → tier2 缩进形状（剥公共最小缩进比对） → tier3 相似度+margin
```

- **tier3 的实测问题**：margin 淘汰的诡异失败（本会话 score 0.98 因
  margin<0.10 被拒，错误信息难自纠）；误命中风险不可控（replace_all 已明确
  仅 Tier1 生效，说明 tier3 的多位置风险被认可过）
- **tier2 的实测价值**：模型缩进失误是高频真实场景（`indent_shape_matches_on_
  tier2` 的用例），剥形状比对有真实救场效果——**不是模糊猜测，是结构对齐**

### 2.2 提案

1. **删 tier3**（相似度+margin）：locate_replace/locate_anchor/locate_replace_all
   的 tier3 回退分支 + `ratio`/`tier3_probe`/`tier3_candidates`
2. **tier2 保留**（数据决策：跑一段真实会话统计 tier2 命中率，若 <5% 再砍）
3. **NO_MATCH 提示形态**（替代 tier3 的自纠功能）：
   - 失败时 hint = "Use the read tool to re-fetch current content, then retry
     with exact text."（指引重读——配合 PR-E-1 的 patch 回显，模型有基线可对）
   - 可选增强（做不做看 tier2 数据）：返回**行数 + 首尾行摘要**帮模型定位漂移
4. **HunkReport 的 tier/score 字段**：tier3 删后 tier 只会是 1/2/4（hint 窗口
   命中）——字段保留（观测用），score 恒 1.0 可留

### 2.3 出口

`cargo test -p qaqh-workspace --lib edit`（tier3 测试删、NO_MATCH 提示断言新
增）+ 真实会话冒烟（edit 连续编辑不再出现 margin 淘汰类失败）。

## 3. 开放问题

- **O-E1**：tier2 命中率的数据收集方式（journal 里 HunkReport.tier 已落库——
  统计脚本即可，无需新埋点）。
- **O-E2**：replace 的 `new` 缩进补偿（`reindent`，tier≥2 触发）在 tier3 删除
  后逻辑不变（tier2 仍补偿）——确认无隐式耦合。
- **O-E3**：strict-only 后模型的多 hunk 重试模式（失败项重发）是否需要在
  description 里显式引导——观察真实行为再定。
