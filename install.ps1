# Cipher Windows 安装脚本 (PowerShell)
# 用法: .\install.ps1 [-Download] [-Tag <tag>]
#   -Download  从 GitHub Release 下载预构建二进制（无需 Rust 工具链）
#   -Tag       指定版本标签（默认 latest）

param(
    [switch]$Download,
    [string]$Tag = "latest"
)

$ErrorActionPreference = "Stop"
$BinName = "cipher"
$GhRepo = "NoxTyrannus/cipher"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\cipher"

Write-Host "== Cipher 安装脚本 =="

function Detect-Arch {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -eq "AMD64") { return "x86_64" }
    if ($arch -eq "ARM64") { return "arm64" }
    throw "不支持的架构: $arch"
}

function Get-AssetName {
    return "cipher-windows-$(Detect-Arch).zip"
}

function Add-ToPath {
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($currentPath -notlike "*$InstallDir*") {
        $newPath = "$currentPath;$InstallDir"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host "已将 $InstallDir 加入用户 PATH (需重开终端生效)"
    }
}

function Install-FromBinary {
    param([string]$BinPath)

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $BinPath -Destination (Join-Path $InstallDir "$BinName.exe") -Force
    Add-ToPath

    Write-Host "已安装到 $InstallDir\$BinName.exe"
    Write-Host ""
    Write-Host "== 安装完成 =="
    Write-Host "首次使用请运行: $BinName setup"
    Write-Host "然后启动: $BinName"
}

function Download-And-Install {
    if (-not (Get-Command curl -ErrorAction SilentlyContinue)) {
        throw "需要 curl 来下载 Release 文件。"
    }

    $asset = Get-AssetName
    Write-Host "== 检测到平台: $asset =="

    if ($Tag -eq "latest") {
        $url = "https://github.com/$GhRepo/releases/latest/download/$asset"
    } else {
        $url = "https://github.com/$GhRepo/releases/download/$Tag/$asset"
    }

    $tmpdir = Join-Path $env:TEMP "cipher-install"
    New-Item -ItemType Directory -Force -Path $tmpdir | Out-Null
    $zipPath = Join-Path $tmpdir $asset

    Write-Host "== 下载 $url =="
    curl.exe -fSL $url -o $zipPath
    if ($LASTEXITCODE -ne 0) {
        throw "下载失败 (curl 退出码 $LASTEXITCODE): $url"
    }

    # 校验和验证：Release 附带 SHA256SUMS 则强制校验，缺失则警告跳过
    if ($Tag -eq "latest") {
        $sumsUrl = "https://github.com/$GhRepo/releases/latest/download/SHA256SUMS"
    } else {
        $sumsUrl = "https://github.com/$GhRepo/releases/download/$Tag/SHA256SUMS"
    }
    $sumsPath = Join-Path $tmpdir "SHA256SUMS"
    curl.exe -fsSL $sumsUrl -o $sumsPath
    if ($LASTEXITCODE -eq 0 -and (Test-Path $sumsPath)) {
        $expected = (Get-Content $sumsPath | ForEach-Object {
            $parts = $_ -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].Trim().TrimStart('*') -eq $asset) { $parts[0].ToLower() }
        } | Select-Object -First 1)
        if ($expected) {
            $actual = (Get-FileHash -Algorithm SHA256 $zipPath).Hash.ToLower()
            if ($actual -ne $expected) {
                throw "SHA256 校验失败: 期望 $expected, 实际 $actual"
            }
            Write-Host "== SHA256 校验通过 =="
        } else {
            Write-Host "警告: SHA256SUMS 中未找到 $asset 条目，跳过校验"
        }
    } else {
        Write-Host "警告: Release 未提供 SHA256SUMS，跳过校验和验证"
    }

    Write-Host "== 解压 =="
    $extractDir = Join-Path $tmpdir "extract"
    if (Test-Path $extractDir) { Remove-Item -Recurse -Force $extractDir }
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

    $binPath = Join-Path $extractDir "$BinName.exe"
    if (-not (Test-Path $binPath)) {
        $binPath = (Get-ChildItem -Path $extractDir -Filter "$BinName.exe" -Recurse | Select-Object -First 1).FullName
    }
    if (-not $binPath -or -not (Test-Path $binPath)) {
        throw "下载的压缩包中未找到 $BinName.exe"
    }

    Install-FromBinary -BinPath $binPath
    Remove-Item -Recurse -Force $tmpdir -ErrorAction SilentlyContinue
}

function Build-And-Install {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "错误: 未找到 Rust 工具链。请先安装: https://rustup.rs"
        Write-Host "或使用 .\install.ps1 -Download 免编译安装"
        exit 1
    }
    Write-Host "== 构建 release 版本 (可能需要几分钟) =="
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "构建失败 (cargo 退出码 $LASTEXITCODE)"
    }
    $binPath = Join-Path (Get-Location) "target\release\$BinName.exe"
    if (-not (Test-Path $binPath)) {
        throw "构建失败: 未找到 $binPath"
    }
    Install-FromBinary -BinPath $binPath
}

if ($Download) {
    Download-And-Install
} else {
    Build-And-Install
}