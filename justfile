# QAQ-Harness Monorepo — 后端统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (15 crates)
#   dsh-minimal-mode/ 极简模式工具（bash_v2 + str_replace_editor）
#
# 说明：Windows 桌面层（WinUI3 壳 / installer / updater）已拆分为独立仓库
# F:\qaqh-winui-app；本仓库只保留跨平台后端核心与公共 SDK。
# 前端打包用 ../qaqh-winui-app/justfile。

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ────────────────────────────────────────────

# 编译 daemon（后端核心，release）
build-daemon:
    cargo build --release -p qaqh-daemon -p qaqh-workspace

# ── 开发 ────────────────────────────────────────────

# 启动 daemon（dev profile）
dev:
    cargo run -p qaqh-daemon -- run

# ── webUI（浏览器直连）─────────────────────────────

# 打开 webUI：读 daemon.json 解析地址，浏览器打开 /debug/（token 由桥脚本
# 自动注入，无需手填）。前置：daemon 已运行（just dev）且 renderer 产物
# 可被定位（QAQH_DEBUG_RENDERER_DIR 或 out/renderer）。
[unix]
web:
    #!/usr/bin/env bash
    set -euo pipefail
    f="${QAQH_DATA_DIR:-$HOME/.config/qaqh}/daemon.json"
    if [ ! -f "$f" ]; then echo "daemon.json 不存在：先启动 daemon（just dev）" >&2; exit 1; fi
    url=$(python3 - "$f" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
ep = d["endpoint"].replace("ws://", "http://").split("/control/v1")[0]
print(ep + "/debug/")
PY
)
    echo "webUI: $url"
    xdg-open "$url" >/dev/null 2>&1 || open "$url" >/dev/null 2>&1 || echo "（手动打开上方地址）"

[windows]
web:
    #!/usr/bin/env pwsh
    $file = if ($env:QAQH_DATA_DIR) { Join-Path $env:QAQH_DATA_DIR "daemon.json" } else { Join-Path $env:USERPROFILE ".deepx\daemon.json" }
    if (-not (Test-Path $file)) { Write-Error "daemon.json 不存在：先启动 daemon（just dev）"; exit 1 }
    $d = Get-Content $file -Raw | ConvertFrom-Json
    $url = ($d.endpoint -replace '^ws://', 'http://') -replace '/control/v1$', ''
    Write-Output "webUI: $url/debug/"
    Start-Process "$url/debug/"

# ── 检查 & 测试 ─────────────────────────────────────

# Rust workspace 检查
check-rust:
    cargo check --workspace

# 全部静态检查
check: check-rust

# 全部测试
test:
    cargo test --workspace

# Rust 测试
test-rust:
    cargo test --workspace

# Rust 格式化检查
fmt:
    cargo fmt --all --check

# Rust Clippy
clippy:
    cargo clippy --workspace --all-targets

# ── 工具 ────────────────────────────────────────────

# 产物状态
[windows]
status:
    @Write-Output "=== Rust binaries ==="
    @if (Test-Path 'target/release/qaqh-daemon.exe') { '  ✓ qaqh-daemon.exe' } else { '  ✗ qaqh-daemon.exe' }
    @if (Test-Path 'target/release/qaqh-workspace.exe') { '  ✓ qaqh-workspace.exe' } else { '  ✗ qaqh-workspace.exe' }

# 清理
clean:
    cargo clean

# 从 version.txt 同步版本号到所有后端配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1
