#Requires -Version 5.1
<#
.SYNOPSIS
    打包 Windows 应用 —— **双击即用**的 `lovart-client.exe`。

.DESCRIPTION
    Windows 上本工具的交付形态就是**一个自包含的 exe**（对标 macOS 的 `lovart.app`）：
    双击 = 图形界面，带参数 = 无头 CLI，**同一个二进制**。

    ── 为什么要有这个脚本（而不是手敲 cargo build）────────────────────
    手敲会漂：忘了生成 `.ico` ⇒ exe 顶着默认图标；没进 MSVC 环境 ⇒ 链接器
    找不到直接报错。打包是**可复现动作**，就该有唯一的脚本入口
    （与 `scripts/make-app.sh` 对称）。

    ── 没有用 tauri-cli 的原因 ──────────────────────────────────────
    与 macOS 侧一致：本工具只需要**一个 exe**，前端资源在编译期已由 `tauri-build`
    嵌进二进制（`frontendDist: ../ui`），运行时不需要任何外部文件。
    `cargo tauri build` 会额外拉一棵 CLI 依赖树，且默认要产安装包 —— 用不上。

.PARAMETER SkipIcon
    跳过 `.ico` 重新生成（已有 `src-tauri/icons/icon.ico` 时可用）。

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/make-exe.ps1
#>
[CmdletBinding()]
param(
    [switch]$SkipIcon
)

$ErrorActionPreference = 'Stop'

$Root     = Split-Path -Parent $PSScriptRoot          # 仓库根（scripts/ 的上一级）
$SrcTauri = Join-Path $Root 'src-tauri'
$IconPng  = Join-Path $Root 'assets\icon.png'
$IconIco  = Join-Path $SrcTauri 'icons\icon.ico'
$Bin      = Join-Path $SrcTauri 'target\release\lovart-client.exe'
$Out      = Join-Path $Root 'lovart-client.exe'

# ── 工具定位 ────────────────────────────────────────────────────────
function Find-Cargo {
    $candidates = @()
    if ($env:CARGO_HOME)  { $candidates += (Join-Path $env:CARGO_HOME 'bin\cargo.exe') }
    if ($env:USERPROFILE) { $candidates += (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe') }
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    $cmd = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "找不到 cargo。请先安装 Rust：https://rustup.rs"
}

function Find-VcVars64 {
    # MSVC 链接器（link.exe）与 INCLUDE/LIB 由 vcvars64.bat 提供，编译前必须载入。
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $vsPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null
    if (-not $vsPath) {
        $vsPath = & $vswhere -latest -products * -property installationPath 2>$null
    }
    if (-not $vsPath) { return $null }
    $vcvars = Join-Path ($vsPath | Select-Object -First 1).Trim() 'VC\Auxiliary\Build\vcvars64.bat'
    if (Test-Path $vcvars) { return $vcvars }
    return $null
}

function Import-VcEnv {
    # 借一个临时 .cmd 把 vcvars64.bat 导出的环境变量灌进当前 PowerShell 进程。
    # （直接把带空格的路径塞进 `cmd /c "..."` 会被引号规则咬到，用文件最稳。）
    param([string]$VcVars)
    $tmp = Join-Path $env:TEMP ('vcenv-' + [Guid]::NewGuid().ToString('N') + '.cmd')
    try {
        Set-Content -Path $tmp -Encoding ASCII -Value @(
            '@echo off',
            ('call "{0}" >nul 2>&1' -f $VcVars),
            'set'
        )
        $dump = & cmd.exe /c $tmp
        foreach ($line in $dump) {
            if ($line -match '^([^=]+)=(.*)$') {
                Set-Item -Path ('Env:' + $matches[1]) -Value $matches[2] -ErrorAction SilentlyContinue
            }
        }
    } finally {
        Remove-Item -Path $tmp -Force -ErrorAction SilentlyContinue
    }
}

# ── .ico 生成 ───────────────────────────────────────────────────────
# 从 `assets/icon.png`（1024）切出多档尺寸，装进标准 ICO 容器。
# 容器里的每一档都是 PNG 数据（Vista+ 支持），`tauri-winres` 原样嵌入资源。
function New-Ico {
    param([string]$Png, [string]$Ico)
    Add-Type -AssemblyName System.Drawing

    $sizes = @(16, 24, 32, 48, 64, 128, 256)
    $blobs = @()
    $src = [System.Drawing.Image]::FromFile($Png)
    try {
        foreach ($s in $sizes) {
            $bmp = New-Object System.Drawing.Bitmap($s, $s)
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $g.PixelOffsetMode   = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $g.SmoothingMode     = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
            $g.DrawImage($src, 0, 0, $s, $s)
            $g.Dispose()
            $ms = New-Object System.IO.MemoryStream
            $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
            $bmp.Dispose()
            $blobs += , $ms.ToArray()
            $ms.Dispose()
        }
    } finally {
        $src.Dispose()
    }

    $dir = Split-Path -Parent $Ico
    if (-not (Test-Path $dir)) { $null = New-Item -ItemType Directory -Path $dir -Force }

    $count  = $blobs.Count
    $offset = 6 + 16 * $count
    $fs = [System.IO.File]::Create($Ico)
    $bw = New-Object System.IO.BinaryWriter($fs)
    try {
        $bw.Write([UInt16]0)        # reserved
        $bw.Write([UInt16]1)        # type：1 = icon
        $bw.Write([UInt16]$count)
        for ($i = 0; $i -lt $count; $i++) {
            $s   = $sizes[$i]
            $len = $blobs[$i].Length
            $dim = if ($s -ge 256) { 0 } else { $s }   # 256 用 0 表示
            $bw.Write([Byte]$dim)       # width
            $bw.Write([Byte]$dim)       # height
            $bw.Write([Byte]0)          # color count（0 = 真彩）
            $bw.Write([Byte]0)          # reserved
            $bw.Write([UInt16]1)        # color planes
            $bw.Write([UInt16]32)       # bits per pixel
            $bw.Write([UInt32]$len)     # bytes in resource
            $bw.Write([UInt32]$offset)  # image offset
            $offset += $len
        }
        foreach ($b in $blobs) { $bw.Write($b) }
        $bw.Flush()
    } finally {
        $bw.Dispose()
        $fs.Dispose()
    }
}

# ── 1. 图标 ─────────────────────────────────────────────────────────
if ($SkipIcon) {
    Write-Host '==> 生成 icon.ico（已跳过）'
    if (-not (Test-Path $IconIco)) { throw "-SkipIcon 但 $IconIco 不存在" }
} else {
    Write-Host '==> 生成 icon.ico'
    New-Ico -Png $IconPng -Ico $IconIco
    Write-Host "    $IconIco"
}

# ── 2. MSVC 环境 ────────────────────────────────────────────────────
$cargo  = Find-Cargo
$vcvars = Find-VcVars64
if ($vcvars) {
    Write-Host '==> 载入 MSVC 环境'
    Import-VcEnv -VcVars $vcvars
} else {
    Write-Warning '找不到 vcvars64.bat —— 若链接失败，请安装 VS Build Tools 的「C++ 生成工具」组件'
}

# ── 3. 编译 ─────────────────────────────────────────────────────────
# 前端资源（`ui/`）是在**这一步**被嵌进去的 ⇒ 改过 `ui/` 必须重新 build，
# 只拷旧二进制不会更新界面。
Write-Host '==> cargo build --release'
Push-Location $SrcTauri
try {
    & $cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }
} finally {
    Pop-Location
}

if (-not (Test-Path $Bin)) { throw "构建产物不存在：$Bin" }

# ── 4. 落到仓库根 ───────────────────────────────────────────────────
Write-Host "==> 组装 $Out"
Copy-Item -Path $Bin -Destination $Out -Force

Write-Host ''
Write-Host "✅ 完成：$Out"
Write-Host '   双击 = 图形界面；命令行 = .\lovart-client.exe --help'
Write-Host ''
Write-Host '⚠️ 工作区（.env / inbox / var / output 的根）由程序自己判定，顺序见'
Write-Host '   src-tauri/src/gui.rs 的 workspace_dir()：当前目录是工作区 ⇒ 就用它。'
Write-Host '   把 exe 放在仓库根（与 .env / inbox / var / output 同层）即可直接跑。'
