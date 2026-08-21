# sync-version.ps1 — qaqh-winui-app：从 version.txt 同步版本号
# 仅覆盖本仓库配置：Cargo.toml（[workspace.package]）与 package.json。
# 后端版本锁（qaqh-backend.lock.json）由 QAQ-Harness 后端仓库维护，不在此同步。

param(
    [string]$VersionFile = "version.txt",
    [string]$CargoToml   = "Cargo.toml",
    [string]$RootPkgJson = "package.json"
)

$ErrorActionPreference = "Stop"

$v = (Get-Content $VersionFile -Raw).Trim()
if (-not $v) { throw "version.txt is empty" }

# Cargo.toml: 替换 [workspace.package] 下的 version
# 注意：反向引用必须写成 ${1}/${2}（带花括号）。若写成 "$1"+$v 且版本号以
# 数字开头，会拼出 "$11..."，被 .NET 解析为不存在的第 11 个捕获组并按字面量
# 输出，导致整段匹配（含 [workspace.package] 段头）被吞掉。
$cargo = Get-Content $CargoToml -Raw
$pattern = '(?ms)(\[workspace\.package\][^\[]*?version\s*=\s*")[^"]*(")'
if (-not [regex]::IsMatch($cargo, $pattern)) {
    throw "Cargo.toml 中未找到 [workspace.package] 的 version 条目"
}
$cargo = [regex]::Replace(
    $cargo,
    $pattern,
    ('${1}' + $v + '${2}')
)
if (($cargo -notmatch '\[workspace\.package\]') -or
    ($cargo -notmatch ('version\s*=\s*' + [regex]::Escape('"' + $v + '"')))) {
    throw "Cargo.toml 版本写入后自检失败（期望 $v），已中止"
}
Set-Content $CargoToml -Value $cargo -NoNewline

# 根 package.json
$pkg = Get-Content $RootPkgJson -Raw | ConvertFrom-Json
$pkg.version = $v
($pkg | ConvertTo-Json -Depth 8) + [Environment]::NewLine |
    Set-Content $RootPkgJson -Encoding UTF8

Write-Host "Done — $v synced to Cargo.toml and package.json (backend lock lives in ../QAQ-Harness)"
