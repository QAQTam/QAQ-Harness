# 配置组件升级计划（config-revamp PLAN）

> 状态：**P0/P1/P2 已全部落地**（2026-08-26）——B1-B5、C1-C4、D1-D3 完成并全绿；
> 剩余：C5 手动走查（用户）、T23 复测确认、P3 挂账表另排。（2026-08-25 定稿）
> 动机：2026-08-25 设置页「保存后回显旧值 + 运行中会话压缩阈值不生效」事故复盘（见 §1）；
> 以及多端路线图（web=axum、ratatui）对配置契约的前置要求（见 §2）。
> 跟踪：todo 面板 T1–T14；本文档为唯一权威规格，实现偏离需先改文档。

---

## 1. 事故复盘（2026-08-25，已实锤）

现象：设置页改 `context_limit=1M / effort=max / compact=0.95` 保存后，重进页面回显 `10000 / low / 0.3`；运行中会话仍按旧阈值提前压缩；用户检查 config.toml「未见改动」。

实证结论（daemon 日志 + 磁盘 mtime + 活体 lease 探测）：

| # | 根因 | 位置 | 定性 |
|---|---|---|---|
| R1 | `reload_config` 手工逐字段拷贝，**漏掉 `auto_compact_threshold`**；压缩决策读 `agent.config.auto_compact_threshold`（engine_turn.rs:910）→ 活会话永远用旧阈值 | `qaqh-msgloop/src/ringing_v1/engine_session.rs:73` | 🔴 确定性 bug |
| R2 | 前端 `spawn_config_save` 成功后**不刷新 settings 缓存、不 bump rev**；轮询仅 rev 变化时刷草稿 → 缓存永久陈旧；重进页面首帧渲染陈旧快照（10000 = `CONTEXT_MIN` clamp 渲染、low = `normalize_effort("")` 回退、0.3 = 旧磁盘值） | `apps/winui/src/bridge/core_settings.rs:83`、`settings_view/view.rs:65` | 🔴 确定性缺陷 |
| R3 | 保存错误只进 `log_diag`，UI 无条件显示「已保存」（`set_save_error` 恒 None）→ 用户无法区分成功/失败 | 同上 + `view.rs on_save` | 🟠 |
| R4 | 保存动作**双发**：daemon 日志每次用户保存出现成对 `[config.save]`（不同 bcast id）；幂等写无害但来源不明 | client 层或前端绑定层，待查（D3） | 🟡 待查 |
| R5 | `autoCompactThreshold` 是唯一无值域守卫的数值字段——全零草稿整包保存会把阈值清成 0 | `service.rs:724` | 🟠 防御缺口 |

结构性疾病（本次升级要治的病根本体）：

- **S1 双真相源**：同一配置项在顶层扁平字段与 `[profiles.*]` 各存一份，load 先 profile 后扁平覆盖、save 反向同步，优先级链是此类 bug 温床；
- **S2 一个字段人肉同步 ~10 处**：`Config`→`PersistentConfig`→load 合并链→`save_with` 回填→`save_config` 分支→`load_config` json!→`reload_config` 手抄（R1 断点）→前端 `SettingsSnapshot`→`parse_config_load`→view.rs json!→UI 区块，无编译期一致性保证；
- **S3 wire 层 snake/camel 双键 shim**（`value2`）+ 手拼 JSON，Web 时代化石；
- **S4 分发靠广播 + 逐字段手抄**，无订阅模型。

---

## 2. 硬性设计约束（P1 起强制，违反即打回）

| # | 约束 | 理由 |
|---|---|---|
| K1 | wire DTO 放叶子 crate **`qaqh-config-api`**：零依赖 runtime/msgloop/daemon/winui | winui / ratatui / axum 三端只依赖它；不把守护进程拖进各端编译树 |
| K2 | 键风格定死 **camelCase**（`#[serde(rename_all = "camelCase")]`），删除 `value2` 双键 shim | JS/web 友好；Rust 侧一行声明；终结 S3 |
| K3 | 写语义 = **JSON Merge Patch**（RFC 7386 风格）：`ConfigPatch` 只含 `Option<T>` 字段、`skip_serializing_if = "None"`；后端逐字段应用+校验 | 多端并发编辑不互相覆盖；天然消除 R5 类「未加载字段毒化」 |
| K4 | fingerprint/字节序逻辑留在 ringing 传输层，DTO 层零耦合 | 传输关注点不进契约 |
| K5 | 磁盘格式向后兼容：旧 config.toml（任意历史形态）必须无损读入；新写收敛到规范形态（一次性 needs_rewrite） | 存量用户零迁移成本 |

---

## 3. 批次拆解

### P0 止血（半天，先行独立合入，不依赖重构）

| ID | 改动 | 文件 | 验收 |
|---|---|---|---|
| B1 | `reload_config` 补 `agent.config.auto_compact_threshold = cfg.auto_compact_threshold;`；删除重复的 permission_level 行（现连写两遍） | `engine_session.rs:73` | 新增单测：构造 agent.config 与新 cfg 阈值不同 → reload → 断言同步；workspace 测试绿 |
| B2 | 抽 `spawn_config_load_inner()` 共用；`spawn_config_save` Ok 分支调用之（刷新缓存 + bump `settings_rev`） | `core_settings.rs:83` | 保存后 ≤1 个轮询周期（500ms）内 UI 显示新值 |
| B3 | 保存 Err 冒泡：BridgeCore 增加 `save_error: Mutex<Option<String>>` + rev；view 轮询读并写入现成的 `set_save_error`；on_save 不再无条件置成功态 | `core_settings.rs`、`view.rs` | 断开 daemon 点保存 → 页面出现错误提示而非「已保存」 |
| B4 | 手工验证链：改阈值(0.95)→保存→①重进页面立即正确 ②运行中会话下一回合按新阈值决策（看 engine 日志 compact gate） | — | 全链通过 |
| B5 | 守卫升级 live-draft（同日真机反馈）：思考强度调整后压缩滑杆被 coerce 回声拉到 0.3、开关自开——渲染期捕获值跨渲染陈旧，改为对比「草稿现值钳到合法区间」；新增 `compact_restore` 槽位，开关关→开恢复最近有效阈值而非无条件 0.75；覆盖 context_section 全部控件 + api/subagent 的 max_tokens/timeout | `sections/basic.rs`、`mod.rs(SettingsCtx)`、`view.rs` | 前端 83 测试绿；复测：调 effort 不再影响压缩控件，开关关→开回到 0.95 |

### P1 schema 统一（契约层，~2–3 天）

| ID | 改动 | 文件 | 验收 |
|---|---|---|---|
| C1 | 新建 `crates/qaqh-config-api`：`ConfigDto`（读模型，全量）、`ConfigPatch`（写模型）；camelCase；serde 往返 + 历史 fixture 单测；ts-rs 生成 TS 类型放 feature `"ts"`（web 端启用） | 新 crate + workspace Cargo.toml | 往返测试绿；`cargo tree -e normal -p qaqh-config-api` 无内部依赖 |
| C2 | 后端接线：`load_config()` 返回 `ConfigDto` 序列化（删手拼 json!）；`save_config(params)` 入参改 `ConfigPatch` 反序列化 + 逐字段应用；threshold 加 `(0,1]` 校验；apiKey 空串/掩码守卫语义原样保留；删除 `value2` | `runtime/src/service.rs:377,1017` | ringing 集成测试绿；非法 patch（threshold=1.5）返回明确 Err |
| C3 | ✅ 已落地（2026-08-25）：`PersistentConfig` 六字段 `skip_serializing`；save_with 不再写顶层；load 检测 legacy → 值**无条件覆盖**固化进 `[profiles.<active>]`（语义裁定：历史读语义扁平胜出 ⇒ 扁平即最新意图）后剥离顶层键 | `qaqh-config/src/config.rs`、`qaqh-types/src/config.rs` | c3_migration_tests×3 绿（纯扁平/混合/纯新形态）+ 全套回归绿 |
| C4 | 前端接线：删 `parse_config_load`（改 serde 反序列化 `ConfigDto`，`SettingsSnapshot` 变 Dto 的 UI 投影）；on_save 从整包 json! 改为**字段级 dirty 的 `ConfigPatch` 累积器**（每个控件回调往对应 Option 写 Some）；删 view.rs 手拼载荷 | `shell_store.rs`、`settings_view/view.rs`、各 sections | 83 个前端测试更新后全绿；九分类逐一手动回归 |
| C5 | 回归 + 发布注记 | — | 后端 workspace 测试 + clippy 全绿；前端 cargo test 全绿；真机走查 §P0-B4 同款链条 |

### P2 分发现代化（~1–2 天）

| ID | 改动 | 文件 | 验收 |
|---|---|---|---|
| D1 | ✅ 已落地（2026-08-25）：`watch.rs`（tokio watch + LATEST 镜像——tokio `send` 零接收者会丢弃载荷的坑已用镜像堵死）；`Config::update` 成功路径 publish；`reload_config` 磁盘优先/镜像兜底。注：字段穷举已由 B1 `apply_config` 穷举字面量承担编译期守卫，整体 clone 赋值留待 AgentConfig 瘦身时一并处理 | `qaqh-config/src/watch.rs`、`config.rs`、`engine_session.rs` | watch 单测×2 + config34/msgloop52/runtime109 全绿 |
| D2 | ✅ 已落地（2026-08-26）：`ControlEvent::ConfigChanged{rev}`（domain 枚举新变体，前端经 qaqh-client re-export 自动可用）+ projection 折叠臂；`notify_config_changed` 在 worker 广播外经 hub 发布（seed=""= 全局惯例）；WinUI 收到即 force 重拉快照，500ms 轮询降为兜底 | `qaqh-domain/event.rs`、`runtime/service.rs+projection.rs`、winui core_client.rs | domain7/runtime109/winui83 全绿 clippy0 |
| D3 | ✅ 已排除（2026-08-26 真机实证）：5 次单击 ↔ daemon 5 条 `[config.save]`、间隔 6-8s，一一对应无双发；此前"成对"系旧会话连续点击。关闭 | — | 已满足 |
| D4 | axum 映射预研（纯文档，不实施）：`GET /api/config`→Dto、`PATCH /api/config`→Patch、`GET /api/config/events`(SSE←D1 watch)、token 鉴权沿用 daemon.json | `docs/config-revamp-plan.md` 附录 | web 端可据此直接开工 |

### P3 收尾挂账（可选，另行排期）

| # | 项 | 说明 | 来源 |
|---|---|---|---|
| P3-1 | `value2` 全局退役 | snake/camel 双键 shim 仍服务 git/workspace/fs 等数十处 action；config.save 已先行脱钩。逐 action 迁移到强类型后逐一删除 | C2 范围裁剪 |
| P3-2 | 版本化迁移器框架 | 收编散落迁移（明文密钥、provider_id、C3 扁平剥离、data-root marker）为有序 `MigrationStep` 链 + config schema_version 字段 | 原 P3 |
| P3-3 | validate 边界收敛 | apply_patch 内散落 filter 全部上移 `ConfigPatch::validate()` 一层（含 effort 白名单已做，剩余：字符串非空语义集中） | 原 P3 |
| P3-4 | AgentConfig 瘦身 + From<&Config> | agent.config 与全局 Config 大量重叠；瘦身后再评估 watch 快照整体赋值替代 apply_config 穷举拷贝 | D1 备注 |
| P3-5 | ratatui 接入样例 | 消费 ConfigDto/ConfigPatch + watch subscribe 的最小参考实现 | 原 P3 |
| P3-6 | 合规模式三选一决策 | content_guard 真实但休眠（词表硬编码）；extra_keywords/allowlist 死字段。A 补全 / B 退役 / C 不动——待产品拍板 | 2026-08-26 用户问答 |
| P3-7 | ts-rs feature 启用 | web 开工时开启 `"ts"` feature 生成 TS 类型（Cargo.toml 已预留注释位） | C1 备注 |

---

## 4. 兼容与风险

| 风险 | 对策 |
|---|---|
| 存量 config.toml 形态多样（纯扁平/纯 profile/混合/明文密钥残留） | C3 fixture 三形态全覆盖；needs_rewrite 一次性迁移；读路径永不破坏 |
| wire 键风格切换（snake→camel）破坏在途客户端 | winui 与 daemon 同仓库同发布（安装包含两端的单一构建），同 PR 升级无灰度窗口；ratatui/web 尚不存在，无存量 |
| 前端 83 测试大面积触碰 | C4 单独成 PR，先改类型再修测试，禁止夹带行为变更 |
| ts-rs 引入新构建依赖 | feature-gate（`"ts"`），默认关闭，不影响桌面构建 |
| PATCH 语义与现有「空串=保持」守卫的交互 | 语义统一进 Patch：`None`=不动、`Some("")`=显式置空（仅允许于可为空字段）、apiKey 特例保留掩码守卫；在 C2 注释中冻结 |

## 6. 附录：axum Web 端映射预研（T20，纯设计未实施）

> 目标读者：web 端开工时的第一任实现者。所有类型/机制均已在本轮落地，
> 本附录只是把它们钉在 HTTP 语义上。**不引入任何新契约**——违反即视为偏离本计划。

### 6.1 路由 → 既有能力映射

| HTTP | 处理器体（伪码） | 复用的既有件 |
|---|---|---|
| `GET /api/config` | `Config::load()` → `to_dto()` → `Json<ConfigDto>` | dto.rs 读映射 |
| `PATCH /api/config` | 反序列化 `Json<ConfigPatch>` → `validate()` → `Config::update(|cfg| apply_patch(cfg, &patch))`；`is_empty()` 时 204 直返 | dto.rs 写映射 + api 层 validate + 单写口 + watch publish（自动） |
| `GET /api/config/events` (SSE) | 订阅 `qaqh_config::watch::subscribe()`，变更时推 `event: config_changed
data: {"rev":…}` | D1 watch 层（与 T18 同源） |

错误语义：Patch 解析失败 → 400（body 携带 serde 错误消息）；`validate()` 失败 → **422**（值域问题属语义而非语法）；鉴权失败 → 401。前端 TS 类型经 ts-rs 从 ConfigDto/ConfigPatch 生成（feature-gate 已预留）。

### 6.2 鉴权与并发约定

- 鉴权沿用 daemon.json Bearer token（与 ringing_http.rs 同一令牌、同一 header 规范——token 不进 query string）；web 服务若独立端口暴露，须默认绑定 127.0.0.1。
- PATCH 天然并发安全：字段级 Option 只覆盖显式提交项，两客户端同时编辑不同字段互不覆盖（K3 设计初衷）；同字段后写胜，无版本向量需求（单用户场景足够，冲突面已被 PATCH 压到最小）。
- SSE 推送载荷只含 rev（通知性质），客户端收到后 GET /api/config 全量重拉——与 WinUI 的「推送通知 + 拉取权威」二段式完全一致，不发明第二套同步协议。

### 6.3 明确不做（防 scope 蔓延）

- 不做 config 的 PUT 整包替换（正是本轮消灭的毒化写法）；
- 不在 web 层二次校验值域（validate() 是唯一边界，P3 收敛点）；
- 不为 web 单独造配置端点命名空间以外的 RPC——一切经 DTO/Patch 契约。

## 5. 里程碑

- **M1** ✅：P0 止血合入（B1-B5，含两次真机回归修正）
- **M2** ✅：C1+C2 后端契约落地
- **M3** ✅：C3+C4 全栈切换 + 自动化回归（C5 九分类手动走查由用户执行中）
- **M4** ✅：P2 分发现代化（D1-D3）+ axum 附录定稿（§6）
- **遗留**：P3 挂账表（P3-1…P3-7）另行排期；T23 回声修复待真机复测确认

> 决策记录：NavigationView 树形历史方案已取消（2026-08-25）；本计划不含标签页入 TitleBar（方案 A/B 待拍板）、会话级端点（设计已定稿待实施）两项既有挂账。
