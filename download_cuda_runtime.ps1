#requires -version 5.1
<#
download_cuda_runtime.ps1 —— 为 CUDA 版 llama-server.exe 补装 CUDA 13.3 运行时 DLL。

背景：llama.cpp 的 CUDA 版服务端（llama-cuda\llama-server.exe）运行时需要 NVIDIA 的
运行时库 cudart64_13.dll / cublas64_13.dll / cublasLt64_13.dll。
这些库**不随显卡驱动发布**（驱动只带 nvcuda.dll），需要从 CUDA 分发包单独获取。

注意：NVIDIA 分发包的目录名不是 cudart/cublas/cublasLt（那些路径会 404），
真实包名是：
    cuda_cudart  -> 内含 cudart64_13.dll
    libcublas    -> 内含 cublas64_13.dll + cublasLt64_13.dll（还有 nvblas64_13.dll）
本脚本从官方分发包索引自动挑最新 13.3 版本下载，把 DLL 解压到 llama-server.exe 同目录
（Windows 优先从 exe 所在目录加载 DLL，无需安装完整 CUDA Toolkit、无需改 PATH）。

用法（任选其一）：
    双击本文件
    或在终端执行：  .\download_cuda_runtime.ps1
    自定义 CUDA 目录： .\download_cuda_runtime.ps1 -CudaDir 'D:\tools\llama-cuda'

下载量约 400~700MB，请保持网络通畅。脚本可重复执行（会覆盖同名 DLL）。
#>
param(
    [string]$CudaDir = '',
    [string]$BaseUrl = 'https://developer.download.nvidia.com/compute/cuda/redist'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# 兼容 TLS1.2（老版 Windows PowerShell 默认可能只开 TLS1.0/1.1，会被 CDN 拒连）
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# ---------- 1. 定位 CUDA 版 llama-server 目录 ----------
if (-not $CudaDir) {
    $root = $PSScriptRoot
    foreach ($c in @('llama-cuda', 'llama-b10453-bin-win-cuda-13.3-x64')) {
        if (Test-Path (Join-Path $root (Join-Path $c 'llama-server.exe'))) {
            $CudaDir = (Resolve-Path (Join-Path $root $c)).Path
            break
        }
    }
}
if (-not $CudaDir -or -not (Test-Path (Join-Path $CudaDir 'llama-server.exe'))) {
    Write-Host '[X] 未找到 CUDA 版 llama-server.exe。' -ForegroundColor Red
    Write-Host '    可用参数指定目录，例如： .\download_cuda_runtime.ps1 -CudaDir D:\tools\llama-cuda' -ForegroundColor Yellow
    exit 1
}
Write-Host "[OK] CUDA 版目录: $CudaDir" -ForegroundColor Green

# ---------- 2. 依次下载 cuda_cudart / libcublas 的最新 13.3 包 ----------
# 注意包名：cudart 在 cuda_cudart，cublas+cublasLt 在 libcublas（cudart/cublas/cublasLt 会 404）。
$packages = @('cuda_cudart', 'libcublas')
$tmp = Join-Path $env:TEMP 'xiangxu-cuda-runtime'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$anyError = $false
foreach ($pkg in $packages) {
    $index = "$BaseUrl/$pkg/windows-x86_64/"
    Write-Host "`n[$pkg] 查询最新 13.3 版本: $index" -ForegroundColor Cyan
    try {
        $html = (Invoke-WebRequest -Uri $index -UseBasicParsing -TimeoutSec 30).Content
    } catch {
        Write-Host "  [X] 索引页获取失败: $($_.Exception.Message)" -ForegroundColor Red
        $anyError = $true
        continue
    }

    # 匹配形如 cuda_cudart-windows-x86_64-13.3.29-archive.zip（注意带 -archive 后缀）
    $pat = [regex]::Escape($pkg) + '-windows-x86_64-13\.3[0-9.]*(-archive)?\.zip'
    $zips = [regex]::Matches($html, $pat) | ForEach-Object { $_.Value } | Sort-Object -Unique

    # 取版本号最高的那个（13.3.29 > 13.3.0.5；按版本号比较）
    $best = $null
    foreach ($z in $zips) {
        $verStr = (($z -replace '^.+?windows-x86_64-', '') -replace '-archive\.zip$', '')
        if ($verStr -match '^13\.3\.\d+(\.\d+)?$') {
            $v = [version]$verStr
            if (-not $best -or $v -gt $best.Ver) { $best = [PSCustomObject]@{ Zip = $z; Ver = $v } }
        }
    }
    if (-not $best) {
        Write-Host "  [X] 索引页里没有 13.3.x 的包，请手动打开 $index 确认版本。" -ForegroundColor Red
        $anyError = $true
        continue
    }

    $url = "$index$($best.Zip)"
    $zipPath = Join-Path $tmp $best.Zip
    Write-Host "  下载 $url" -ForegroundColor Cyan
    try {
        Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing -TimeoutSec 600
    } catch {
        Write-Host "  [X] 下载失败: $($_.Exception.Message)" -ForegroundColor Red
        $anyError = $true
        continue
    }

    $extractDir = Join-Path $tmp ($pkg + '-x')
    if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

    $dlls = Get-ChildItem $extractDir -Recurse -Filter '*.dll' -ErrorAction SilentlyContinue
    if (-not $dlls) {
        Write-Host "  [X] 压缩包里没有找到 DLL。" -ForegroundColor Red
        $anyError = $true
        continue
    }
    foreach ($d in $dlls) {
        Copy-Item $d.FullName (Join-Path $CudaDir $d.Name) -Force
        Write-Host "  已拷贝 $($d.Name)  ($([math]::Round($d.Length/1MB,1)) MB)"
    }
}

# ---------- 3. 验证三个库族是否齐全 ----------
Write-Host "`n===== 验证 =====" -ForegroundColor Cyan
$families = @('cudart64*.dll', 'cublas64*.dll', 'cublasLt64*.dll')
$allOk = $true
foreach ($f in $families) {
    $hit = Get-ChildItem $CudaDir -Filter $f -ErrorAction SilentlyContinue
    if ($hit) {
        foreach ($h in $hit) { Write-Host "  [OK] $($h.Name)  ($([math]::Round($h.Length/1MB,1)) MB)" -ForegroundColor Green }
    } else {
        Write-Host "  [X] 缺少 $f" -ForegroundColor Red
        $allOk = $false
    }
}

if (-not $anyError -and $allOk) {
    Write-Host "`n运行时就绪！" -ForegroundColor Green
    Write-Host "再做个快速自检（加载 CUDA 后端，验证 DLL 能被找到）..." -ForegroundColor Cyan
    try {
        $ver = & (Join-Path $CudaDir 'llama-server.exe') --version 2>&1 | Select-Object -First 6
        $ver | ForEach-Object { Write-Host "  $_" }
    } catch {
        Write-Host "  自检命令执行失败（不影响 DLL 已就位）: $($_.Exception.Message)" -ForegroundColor Yellow
    }
    Write-Host "`n下一步：打开应用 -> 高级设置/API，确认「服务端exe」已自动指向 CUDA 版；" -ForegroundColor Yellow
    Write-Host "    识图/翻译的「GPU层数(-ngl)」应为 99。然后重启应用即可（应用会自动检测）。"
} else {
    Write-Host "`n有步骤未完成，请根据上面的 [X] 信息处理。临时下载文件在: $tmp" -ForegroundColor Red
    exit 1
}
