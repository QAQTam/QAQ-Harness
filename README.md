# QAQ-Harness

本地优先的 AI agent 工作台：**一个 Rust daemon** 承载多会话 agent 循环、canonical session、
工具运行时、沙箱与权限；**多种客户端**（桌面 / 终端 / 浏览器 / Windows 原生壳 / 手机）
经 **Ringing** 协议接入。

本仓**不放产品代码**。它是这个项目的门面与索引：哪个仓是什么、当前版本锁了哪些 commit、
包在哪下载。

> 为什么要有这一层：跨仓事实写在各个代码仓里必然漂移。**实例（2026-10-14 已修）**：
> `qaqh-js-sdk` 与 `qaqh-winui-app` 的 README 曾引用 `QAQ-Harness/docs/frontend-contract.md`，
> 而那份文档**从来没有存在过**（`qaqh-backend/crates/qaqh-client/src/discovery.rs:316`
> 的注释已承认这一点）；同一批 README 里的 `../QAQ-Harness` 指的是**改名前的** backend
> monorepo（现名 `qaqh-backend`），`qaqh-winui-app` 的 5 处 path 依赖也照旧名写死。
> 两处已于同日改到与事实一致（`qaqh-js-sdk@977806e`、`qaqh-winui-app@72e7586`）。

## 当前版本

**`v2.0.0-RC1`** — 锁定的 commit、产物与已知问题见 [`releases/2.0.0-RC1.md`](releases/2.0.0-RC1.md)。

## 组成发行版的两块

| 仓 | 角色 | 栈 |
|---|---|---|
| [`qaqh-backend`](https://github.com/QAQTam/qaqh-backend) | **权威后端**：daemon（多会话 agent 循环、canonical session、工具运行时、沙箱、权限、Ringing 服务面） | Rust 2024 多 crate workspace |
| [`qaqh-desktop-app`](https://github.com/QAQTam/qaqh-desktop-app) | **默认桌面客户端**：接入页 / 会话工作台；打包时内嵌由后端仓构建的 daemon 侧车 | Tauri 2 壳（`src-tauri`）+ SolidJS 2 + Vite + TS strict |

**发布规则：两仓同名 tag。** 壳包必须内嵌**同一个 tag** 构建的 daemon，所以两者不允许
错位。打包 CI 住在前端仓（`.github/workflows/release.yml`）：它按同名 tag 拉后端、
构建五个平台的包，全部到齐后才发布 Release。Intel macOS 走 arm64 runner 交叉编译
（`macos-13` 镜像正在退场）。

## 其它客户端（同一个 daemon 的多种前端）

| 仓 | 平台 | 说明 |
|---|---|---|
| [`qaqh-electron`](https://github.com/QAQTam/qaqh-electron) | 桌面（Electron） | 上一代桌面壳（top-tabbed workbench，Electron 44 + SolidJS 2） |
| [`qaqh-webUI`](https://github.com/QAQTam/qaqh-webUI) | 浏览器 | Ringing 协议前端，可独立部署 |
| [`qaqh-tui-app`](https://github.com/QAQTam/qaqh-tui-app) | 终端 | ratatui + tokio，V2 fullscreen 单流直连本地 daemon |
| [`qaqh-winui-app`](https://github.com/QAQTam/qaqh-winui-app) | Windows 原生 | WinUI 3 壳 + 安装器 / 更新器，**无 WebView**（XAML 由 windows-reactor 驱动） |
| [`qaqh-macios`](https://github.com/QAQTam/qaqh-macios) | macOS / iOS | 原生 SwiftUI 客户端 |
| [`qaqh-harmony`](https://github.com/QAQTam/qaqh-harmony) | 鸿蒙 | 局域网遥控与授权确认（ArkTS） |
| [`qaqh-android`](https://github.com/QAQTam/qaqh-android) | Android | 局域网遥控与授权确认（Compose） |
| [`qaqh-js-sdk`](https://github.com/QAQTam/qaqh-js-sdk) | 库 | 壳无关 TypeScript SDK（Electron / Tauri webview / 浏览器共用一份实现） |

**不是客户端的两个仓**（容易被误认）：[`qaqh-winui-reactor-vendor`](https://github.com/QAQTam/qaqh-winui-reactor-vendor)
与 [`qaq-winui-reactor`](https://github.com/QAQTam/qaq-winui-reactor) 是 `windows-rs` 的
vendor fork，属 Windows 原生壳的依赖。另有 [`qaq-shell`](https://github.com/QAQTam/qaq-shell)（WinPE 桌面 shell，
Rust + egui）与 [`qaq-pe`](https://github.com/QAQTam/qaq-pe)（恢复环境工具合集），是系统层项目。

## 文档的真源在哪

| 想知道 | 去哪 |
|---|---|
| 后端架构、ADR、计划、分析 | `qaqh-backend/docs/`（`ARCHITECTURE.md`、`adr/`、`plan-*`、`analysis-*`） |
| 桌面端视觉与排版规范 | `qaqh-desktop-app/docs/`（`ui-visual-spec.md`、`markdown-typography.md`） |
| 协议类型 | 后端 crate 用 ts-rs 生成到各客户端；生成物勿手改 |
| 打包与版本规约 | `qaqh-desktop-app/README.md`「打包与版本」+ `justfile` |

**本仓不复制这些内容**——复制出来的第二份必然与真源漂移。

## 本仓约定

1. 不放产品代码、不放生成物；只放索引、发布清单与说明。
2. 每个版本一个 `releases/<version>.md`：锁定哪些 commit、tag、产物、已知问题。
3. 跨仓事实一律**链接**到真源，不抄内容。

## 许可

| 仓 | 许可 | 文件 |
|---|---|---|
| `qaqh-backend` | **MIT OR Apache-2.0**（下游任选其一） | `LICENSE-MIT` + `LICENSE-APACHE` |
| `qaqh-desktop-app` | **MIT OR Apache-2.0** | `LICENSE-MIT` + `LICENSE-APACHE` + `NOTICE` |
| 其余自研仓（`qaqh-winui-app` · `qaqh-tui-app` · `qaqh-electron` · `qaqh-webUI` · `qaqh-js-sdk` · `qaqh-macios` · `qaqh-harmony` · `qaqh-android` · `qaq-shell` · `qaq-pe`） | **MIT OR Apache-2.0**（下游任选其一） | `LICENSE-MIT` + `LICENSE-APACHE` |
| 本仓（仅文档与索引） | **MIT OR Apache-2.0** | `LICENSE-MIT` + `LICENSE-APACHE` |
| 上游 vendor / fork（`qaqh-winui-reactor-vendor` · `qaq-winui-reactor` 等） | 随上游，本账号未改动 | 上游自带 |

**为什么双许可**：下游可**任选其一**；Apache-2.0 附带**显式专利授权**（§3），
对会被公司 / 客户拿去用的客户端比纯 MIT 更稳。

全部自研仓口径一致，版权声明年份 2026。

**许可是许可，商标是商标**：Apache-2.0 §6 明文排除商标、服务标记与商品名的许可，MIT 全文
根本未涉及商标——所以「加不加 Apache-2.0」既不带来商标授权，也不解决商标问题。客户端界面
里出现的「ChatGPT」「OpenAI」等名称，只用于**如实描述用户可登录的对象**；本项目与其权利人
**无隶属、无赞助、无背书**关系。官方图形资产默认不随仓分发（见桌面包内 `NOTICE` 与
`qaqh-desktop-app/docs/siwc-brand-posture.md`）。

本仓自身（仅文档与索引）为 MIT。
