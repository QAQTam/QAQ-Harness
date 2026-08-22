# Issue 草稿（贴到 qaqh-winui-app 仓库）

> 标题：**契约对齐：后端三刀重构落地 + webUI 托管就绪，请按清单核对/提 PR**
>
> 背景：QAQ-Harness 后端完成架构砍伐（-5000 行）并正式化了浏览器 webUI
> 托管。契约规则见 [frontend-contract.md](https://github.com/QAQTam/QAQ-Harness/blob/main/docs/frontend-contract.md)。
> 本 issue 列出需要 winui-app 侧核对或修改的事项；每项完成后勾选，
> 全部完成后关闭本 issue。

## A. 需要开 PR 修改的

### A1. discovery `endpoint` 字段迁移
daemon.json 的 `endpoint` 当前值仍是遗留格式 `ws://host:port/control/v1`
（WS 数据协议已于 M3 拆除，仅剩生命周期端点）。计划改为
`http://host:port`。请：
- [ ] 排查所有解析 `endpoint` 的代码路径，确认只取 host:port 部分
      （若是，无需改动即可兼容新值）；
- [ ] 若有依赖 `ws://` 前缀或 `/control/v1` 后缀的逻辑，改造为前缀无关。

### A2. renderer 浏览器适配（webUI 前置）
daemon 已在 `GET /debug/` 提供 renderer 静态托管 + token 桥注入
（`window.__QAQH_DEBUG__ = {token, nonce}`）。请提 PR：
- [ ] 构建产物使用相对 base（vite `base: './'`），资源引用无绝对路径；
- [ ] 连接配置抽象为 provider：桌面壳走 IPC/preload；检测到
      `window.__QAQH_DEBUG__` 时走同源 + 桥 token；
- [ ] SSE 客户端确认用 fetch + ReadableStream（EventSource 无法携带
      `Authorization` / `X-QAQH-Client-Session-Id` 自定义头，且 daemon
      禁止 query token——这是硬约束，见契约文档第 2 节）；
- [ ] （待确认）前端路由若为 history 模式，刷新非根路径会 404：
      请告知路由模式；hash 模式则无需处理。

### A3. `profile.*` action 启用核对
daemon actions 白名单此前缺失 `profile.` 前缀，导致 SDK 的
`ActionRequest::ProfileApply / ProfileSaveCurrent / ProfileDelete`
实际不可用（400）；后端已修复。请：
- [ ] 确认设置页命名 profiles 功能改用上述三个类型化变体
      （而非绕道 config.save）。

## B. 仅需核对的

### B1. 三刀重构后的行为面
以下改动**不改变 HTTP 表面**，但请回归验证：
- [ ] 移除 `qaqh-lsp / qaqh-update / qaqh-gate-testui`（updater/installer
      归属本仓库不变）；
- [ ] `QaqhService::handle` 清除 12 个不可达 legacy 方法（Ringing 命令是
      唯一会话命令入口，不受影响）;
- [ ] daemon HTTP 共享管线重构（响应字节级语义保持一致）；
- [ ] webUI 托管 loopback-only：LAN server 模式下 `/debug/*` 一律 403
      （原生壳不受影响，仍走 Ringing 端点）。

### B2. 工具链
- [ ] Windows 侧 rustfmt/clippy 版本与本仓库 CI 口径对齐（当前 Linux 端
      stable 1.98 已全绿；建议双侧固定同一 toolchain）。

## C. 决策请求

- [ ] Tauri 壳是否立项？后端建议：renderer 与壳解耦已完成，Tauri 可直接
      复用 `qaqh-client` 做 daemon 生命周期管理；Electron 不建议新起。
