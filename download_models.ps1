#requires -version 5.1
<#
download_models.ps1 -- 下载象胥所需的 AI 模型（GGUF 格式）到 assets/models/ 目录。

模型清单（总计约 4GB）：
  1. Qwen2.5-1.5B-Instruct Q4_K_M   (~1.0 GB)  -- 翻译模型
  2. Qwen2.5-VL-3B-Instruct Q4_K_M  (~1.8 GB)  -- 视觉识图模型
  3. Qwen2.5-VL-3B mmproj f16       (~1.3 GB)  -- 视觉投影文件

所有模型均来自 Hugging Face 官方仓库，Apache-2.0 许可。

用法：
    双击本文件
    或在终端执行：  .\download_models.ps1
    自定义输出目录： .\download_models.ps1 -ModelsDir 'D:\models'
#>
param(
    [string]$ModelsDir = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# ---------- 1. 确定输出目录 ----------
$Root = $PSScriptRoot
if (-not $ModelsDir) {
    $ModelsDir = Join-Path $Root 'assets\models'
}

$TextModelDir = $ModelsDir
$VLModelDir   = Join-Path $ModelsDir 'qwen2.5-vl-3b'

New-Item -ItemType Directory -Force -Path $TextModelDir | Out-Null
New-Item -ItemType Directory -Force -Path $VLModelDir   | Out-Null

Write-Host '========================================' -ForegroundColor Cyan
Write-Host '  象胥模型下载脚本' -ForegroundColor Cyan
Write-Host '========================================' -ForegroundColor Cyan
Write-Host "输出目录: $ModelsDir"
Write-Host ''

# ---------- 2. 定义模型文件 ----------
$models = @(
    [PSCustomObject]@{
        Name    = '翻译模型 Qwen2.5-1.5B-Instruct Q4_K_M'
        Repo    = 'Qwen/Qwen2.5-1.5B-Instruct-GGUF'
        File    = 'qwen2.5-1.5b-instruct-q4_k_m.gguf'
        OutDir  = $TextModelDir
        OutName = 'qwen2.5-1.5b-instruct-q4_k_m.gguf'
    },
    [PSCustomObject]@{
        Name    = '视觉模型 Qwen2.5-VL-3B-Instruct Q4_K_M'
        Repo    = 'ggml-org/Qwen2.5-VL-3B-Instruct-GGUF'
        File    = 'Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf'
        OutDir  = $VLModelDir
        OutName = 'Qwen2.5-VL-3B-Instruct.Q4_K_H.gguf'
    },
    [PSCustomObject]@{
        Name    = '视觉投影 mmproj-f16'
        Repo    = 'ggml-org/Qwen2.5-VL-3B-Instruct-GGUF'
        File    = 'mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf'
        OutDir  = $VLModelDir
        OutName = 'Qwen2.5-VL-3B-Instruct.mmproj.gguf'
    }
)

# ---------- 3. 逐个下载 ----------
$anyError = $false
foreach ($m in $models) {
    $outPath = Join-Path $m.OutDir $m.OutName

    if (Test-Path $outPath) {
        $sizeMB = [math]::Round((Get-Item $outPath).Length / 1MB, 1)
        Write-Host "[SKIP] $($m.Name) -- 已存在 ($sizeMB MB)" -ForegroundColor Yellow
        continue
    }

    $url = "https://huggingface.co/$($m.Repo)/resolve/main/$($m.File)"
    Write-Host "[DOWN] $($m.Name)" -ForegroundColor Green
    Write-Host "  URL: $url"
    Write-Host "  保存到: $outPath"

    try {
        Invoke-WebRequest -Uri $url -OutFile $outPath -UseBasicParsing -TimeoutSec 1800
        $sizeMB = [math]::Round((Get-Item $outPath).Length / 1MB, 1)
        Write-Host "  [OK] 下载完成 ($sizeMB MB)`n" -ForegroundColor Green
    } catch {
        Write-Host "  [FAIL] $($_.Exception.Message)`n" -ForegroundColor Red
        $anyError = $true
    }
}

# ---------- 4. 验证 ----------
Write-Host '========================================' -ForegroundColor Cyan
Write-Host '  验证' -ForegroundColor Cyan
Write-Host '========================================' -ForegroundColor Cyan
$allOk = $true
foreach ($m in $models) {
    $outPath = Join-Path $m.OutDir $m.OutName
    if (Test-Path $outPath) {
        $sizeMB = [math]::Round((Get-Item $outPath).Length / 1MB, 1)
        Write-Host "  [OK] $($m.OutName)  ($sizeMB MB)" -ForegroundColor Green
    } else {
        Write-Host "  [X]  缺少 $($m.OutName)" -ForegroundColor Red
        $allOk = $false
    }
}

if ($allOk -and -not $anyError) {
    Write-Host "`n模型就绪！可以运行 cargo run 启动象胥了。" -ForegroundColor Green
} else {
    Write-Host "`n部分模型下载失败，请检查网络后重新运行本脚本（已下载的会跳过）。" -ForegroundColor Red
    exit 1
}
