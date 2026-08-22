#requires -version 5.1
<#
build_release.ps1 -- 构建 release 版并打包发布目录。

用法：
    .\build_release.ps1            # 默认输出到 dist\
    .\build_release.ps1 -OutDir D:\release

产物结构（dist\）：
    Xiangxu\
    ├── xiangxu.exe              # 主程序（免 Rust 环境，双击即用）
    ├── llama-cuda\              # CUDA 版 llama.cpp + 运行时 DLL（自带，开箱即用）
    ├── download_cuda_runtime.ps1# 换机器/重装时：重新补装 CUDA 13.3 运行时 DLL
    └── README.md

注意：
  - 模型（约 4GB）不打包，用户在应用内「模型资源」一键下载。
  - 需要 NVIDIA 显卡（驱动须支持 CUDA 13.3；运行时 DLL 已随 llama-cuda 目录分发）。
#>
param(
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot

if (-not $OutDir) {
    $OutDir = Join-Path $Root 'dist'
}

Write-Host '=== 1/3 编译 release 版 ...' -ForegroundColor Cyan
Push-Location $Root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host '[X] cargo build 失败' -ForegroundColor Red
        exit 1
    }
} finally {
    Pop-Location
}

$Exe = Join-Path $Root 'target\release\xiangxu.exe'
if (-not (Test-Path $Exe)) {
    Write-Host "[X] 未找到 $Exe" -ForegroundColor Red
    exit 1
}

Write-Host '=== 2/3 组装发布目录 ...' -ForegroundColor Cyan
$Stage = Join-Path $OutDir 'Xiangxu'
if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item $Exe $Stage
Write-Host '  [OK] xiangxu.exe'

# CUDA 版 llama.cpp + 运行时 DLL（开箱即用）
if (Test-Path (Join-Path $Root 'llama-cuda')) {
    Copy-Item (Join-Path $Root 'llama-cuda') $Stage -Recurse
    Write-Host '  [OK] llama-cuda\ (含 CUDA 13.3 运行时 DLL)'
} else {
    Write-Host '  [SKIP] llama-cuda\ 不存在（需先准备 CUDA 版 llama.cpp 并运行 download_cuda_runtime.ps1）' -ForegroundColor Yellow
}

# CUDA 运行时补装脚本（用户换机器 / 重装系统时用）
Copy-Item (Join-Path $Root 'download_cuda_runtime.ps1') $Stage
Write-Host '  [OK] download_cuda_runtime.ps1'

# README
if (Test-Path (Join-Path $Root 'README.md')) {
    Copy-Item (Join-Path $Root 'README.md') $Stage
    Write-Host '  [OK] README.md'
}

Write-Host '=== 3/3 打包 zip ...' -ForegroundColor Cyan
$Zip = Join-Path $OutDir 'Xiangxu-release.zip'
if (Test-Path $Zip) { Remove-Item $Zip -Force }
# llama-cuda 目录约 660MB，压缩耗时较长属正常
Compress-Archive -Path $Stage -DestinationPath $Zip -CompressionLevel Optimal

$sizeMB = [math]::Round((Get-Item $Zip).Length / 1MB, 1)
Write-Host "`n完成！" -ForegroundColor Green
Write-Host "  发布目录: $Stage"
Write-Host "  压缩包:   $Zip  ($sizeMB MB)"
Write-Host "`n上传 $Zip 到 GitHub Releases 即可。用户解压后打开应用，"
Write-Host "在「模型资源」里一键下载模型就能使用（需 NVIDIA 显卡）。"
