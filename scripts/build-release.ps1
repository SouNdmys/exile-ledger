# 一条命令出一个能双击的程序:编 release 版,再把桌面快捷方式刷新一遍。
#
# 用法(在仓库根目录):
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1 -SkipShortcut
#
# 第一次编要几分钟(整个依赖树都得编一遍),之后只改自己的代码就快得多。

[CmdletBinding()]
param(
    # 只想编、不想动桌面的时候加这个开关。
    [switch]$SkipShortcut
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    Write-Host '正在编译 release 版(第一次会比较久)…'
    # release 版编到 target\release,和平时 cargo run 用的 target\debug 是两个
    # 文件,互不影响 —— 程序开着也能编。
    cargo build --release -p pnd-app
    if ($LASTEXITCODE -ne 0) {
        throw "编译失败(退出码 $LASTEXITCODE)。"
    }

    # crate 叫 pnd-app,exe 叫 exile-ledger.exe(见 pnd-app/Cargo.toml 的 [[bin]])。
    $exe = Join-Path $repo 'target\release\exile-ledger.exe'
    $size = [Math]::Round((Get-Item -LiteralPath $exe).Length / 1MB, 1)
    Write-Host "编好了:$exe($size MB)"

    if (-not $SkipShortcut) {
        & (Join-Path $PSScriptRoot 'make-shortcut.ps1') -ExePath $exe
    }
}
finally {
    Pop-Location
}
