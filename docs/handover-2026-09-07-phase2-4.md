# QAQ-Harness 结构简化工程 · 交接报告 Phase 2–4（handover supplement）

| 项 | 值 |
|---|---|
| 日期 | 2026-09-07（接 `docs/handover-2026-09-07.md`，覆盖 Phase 0–1） |
| 范围 | Phase 2（六大巨型文件拆分）+ Phase 3（协议/驱动收敛）+ Phase 4（注释修订） |
| 质量基线 | `cargo test --workspace` **861 passed / 0 failed** · `clippy --all-targets` **0 警告** · `fmt --check` clean（与 Phase 0–1 基线完全一致） |
| 对外可见面 | 全程不动：JSON wire shape、daemon.json、bindings TS 面、workspace CLI 词表、Ringing V1 协议、错误码语义 |

## Commit 清单（Phase 2–4 共 11 个）

| # | Commit | 内容 |
|---|---|---|
| 2-1 | `faae93a` | service.rs 1214 → service.rs(790) + service/{params,fs_git,stats,plan,common} |
| 2-2 | `f51ed3e` | exec.rs 2507 → exec/{shell,pipe,truncate,direct,handler,register} + tests.rs 外移 |
| 2-3 | `4b397eb` | todo.rs 1582 → todo/{model,store,parse,actions,dispatch} + tests.rs 外移 |
| 2-4 | `8482bfc` | axum_server.rs 2413 → axum_server.rs(472) + axum_impl/{auth,command,timeline_api,content,service_api,sse,debug_control}；22 处 Ack 字面量收敛为 reject_ack/accept_ack/ack_response |
| 2-5 | `5e3ee3b` | loop_core.rs 1901 → loop_core.rs(645) + loop_{injection,dispatch_control,dispatch_conversation,dispatch_tool,outcome}；dispatch_ringing_one 收敛为 3 个 on_* 调用 |
| 2-6 | `7912b9c` | hub.rs 2940 → hub.rs(2146) + ringing/{timeline_hub(552),orphan_seal(270)}；Drop/锁原位 |
| 3-1 | `2827041` | gate transport.rs(177)：运行时单例/cancel/退避/错误描述/envelope/filter/SseTrace 单源化；净 −349 行 |
| 3-2 | `b1cb1ee` | context_flow：删 Visibility.timeline/persist 死字段 + single door 文档修正 |
| 3-3 | `01e99b5` | session with_meta_locked：12 处 lock+dir+load 样板收敛（save_full 除外） |
| 3-5 | `214ffcd` | D3 关闭（双向差异注释）+ client drain_frames 合一 |
| 4 | `96407f0` | Phase 4 注释修订 18 文件 + locate 死副本删除 |

## 决策点终局

| # | 结论 |
|---|---|
| D1 | 已接线（Phase 1），无异常 |
| D2 | **仍未决**：TOOL/ENV/Annotation/dedupe 全部保留（语义收缩需拍板）。Phase 3-2 只删了无争议死字段 |
| D3 | **关闭**：SseDecoder 暂缓合一，双向差异注释已写（产出类型/event 语义/data 空白/EOF 四项差异） |
| D4 | **仍未决**：PersistentConfig 弃用窗口 |
| D5 | **仍未决**（建议暂缓）：RingingEvent/Command 别名 |
| D6 | **关闭**：已证伪——PR-2-1 共享客户端自带 30min 总超时，三协议全经它发请求，无 bug |

## 路线图偏离（有理由，3 处）

1. `agent/loop/` → `agent/loop_*.rs`：`loop` 是 Rust 关键字；且与 `engine_*.rs` 平铺惯例一致。
2. hub 未做 TimelinePersistence 子结构体字段收拢：方法迁移已达目标，结构重组另立项。
3. 重试主循环未收敛为 `retry_engine::run`：三循环请求构造/auth/成功路径差异真实，闭包引擎风险 > 收益；零件层已单源。

## 执行事故（已修复，纪律沉淀）

1. **axum Ack 语义事故**：批量脚本把 14 处 Rejected 写成 accept_ack（status 解析残留逗号）。提交前逐项核对拦截。纪律：脚本改 20+ 处必须逐项 diff 复核。
2. **正则误伤函数参数**：`^fn `→`pub(crate) fn` 把多行签名里的参数也改了；`\.into\(\)\)` 懒匹配陷阱。纪律：可见性批量改后必须 `cargo check` 逐轮清。
3. **子代理 6 个全被取消**：改亲手 rg/sed/python 复核，反而避开了 handover §六的数字坑。

## 遗留（D2/D4/D5 待用户拍板；无代码遗留）

- D2：tool 结果是否统一走 ingest(TOOL)——决定 TOOL/ENV/Annotation/dedupe 去留。
- D4：PersistentConfig 6 扁平字段需发版窗口后删。
- D5：RingingEvent/Command 别名（收益 <90 行，建议暂缓）。
- 本地 main 共 33 个未推 commits（含上轮 22 个），push 前确认。
