# 在桌面放一个「Exile Ledger」快捷方式,指向 release 版的 exe。
#
# 单独一个脚本、而不是塞进 build-release.ps1:exe 换了地方(比如整个仓库
# 搬盘)之后,重跑这一个脚本就够了,不用再等一次编译。
#
# 用法:
#   powershell -ExecutionPolicy Bypass -File scripts\make-shortcut.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\make-shortcut.ps1 -ExePath D:\别处\exile-ledger.exe

[CmdletBinding()]
param(
    # 这两个都留空,默认值在下面算 —— 不能写在这里:Windows PowerShell 5.1
    # 求值 param() 默认值的时候 $PSScriptRoot 还是空的,写在这儿脚本一启动
    # 就报错。PowerShell 7 没这个毛病,所以只在 5.1 上炸,最难查的那一类。
    [string]$ExePath,
    [string]$ShortcutPath
)

$ErrorActionPreference = 'Stop'

# $PSScriptRoot 是脚本自己所在的目录,所以不管从哪个目录敲这条命令,算出来的
# 路径都一样。
if (-not $ExePath) {
    $ExePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\exile-ledger.exe'
}
if (-not $ShortcutPath) {
    $ShortcutPath = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Exile Ledger.lnk'
}

# 相对路径转成绝对路径:快捷方式里存相对路径的话,双击时以哪个目录为准是
# 说不准的事。
$ExePath = [System.IO.Path]::GetFullPath($ExePath)

if (-not (Test-Path -LiteralPath $ExePath)) {
    throw "找不到 exe:$ExePath`n先跑 scripts\build-release.ps1(或 cargo build --release -p pnd-app)。"
}

$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut($ShortcutPath)
$lnk.TargetPath = $ExePath
# 工作目录设成 exe 所在的目录。程序的数据其实都写在 %LOCALAPPDATA% 里,
# 跟工作目录无关,但万一以后有相对路径,这样才是可预期的。
$lnk.WorkingDirectory = Split-Path -Parent $ExePath
# 图标就用 exe 自己第 0 号那张(build.rs 编进资源段的那个)。
$lnk.IconLocation = "$ExePath,0"
# 鼠标停在快捷方式上时的那句提示,**只能写 ASCII**:WScript.Shell 存这个字段
# 走的是系统 ANSI 代码页,中文进去出来就是一串 `????`(实测过)。图标和
# 快捷方式的名字没有这个毛病,所以中文该在的地方还在。
$lnk.Description = 'Exile Ledger - Path of Exile price watcher and market ledger'
$lnk.Save()

Write-Host "快捷方式已写好:$ShortcutPath"
Write-Host "  指向:$ExePath"

# 程序改名之前那个快捷方式指着 pnd-app.exe,而那个文件已经不存在了 ——
# 留着它只会让人双击到一个"找不到目标"的对话框。新的写成功之后才删,
# 顺序反了的话中途出错就会两个都没有。
$legacyShortcut = Join-Path ([Environment]::GetFolderPath('Desktop')) 'POE Ninja Data.lnk'
if (Test-Path -LiteralPath $legacyShortcut) {
    Remove-Item -LiteralPath $legacyShortcut -Force
    Write-Host "  已删掉旧的:$legacyShortcut"
}
