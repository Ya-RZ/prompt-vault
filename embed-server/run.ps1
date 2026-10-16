#Requires -Version 5.1
# 备用入口。主入口是 run.bat（原生批处理，不依赖 PowerShell）。
# 两者功能等价、参数名不同（-CpuOnly 对应 --cpu），改一边必须同步改另一边。
<#
.SYNOPSIS
    启动编码侧车服务。

.DESCRIPTION
    单进程、单 worker。多 worker 会各自加载一份 4.2 GB 权重，8 GB 显存直接爆。

.EXAMPLE
    .\run.ps1
    .\run.ps1 -Port 8917 -ModelDir "D:\models\qwen3-vl-embedding-2b"
    .\run.ps1 -CpuOnly                          # 强制走 CPU（排除 GPU 因素时用）
    .\run.ps1 -MaxPixels 1310720                # 显存不够时往下调
#>
param(
    [string]$Host = "",
    [int]$Port = 0,
    [string]$ModelDir = "",
    [int]$MaxPixels = 0,
    [int]$MaxLength = 0,
    [switch]$CpuOnly
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$PY = Join-Path $PSScriptRoot ".venv\Scripts\python.exe"
if (-not (Test-Path $PY)) {
    throw "找不到 $PY。先跑 .\setup.ps1"
}

if ($ModelDir)  { $env:PV_MODEL_DIR  = $ModelDir }
if ($Host)      { $env:PV_HOST       = $Host }
if ($Port -gt 0)     { $env:PV_PORT = "$Port" }
if ($MaxPixels -gt 0) { $env:PV_MAX_PIXELS = "$MaxPixels" }
if ($MaxLength -gt 0) { $env:PV_MAX_LENGTH = "$MaxLength" }
if ($CpuOnly)   { $env:PV_DEVICE = "cpu" }

# 先让 app.py 解析模型目录（只做本地文件检查，不加载模型、不联网）。
# 解析失败会带出「所有试过的路径」，比等启动到一半才报错好定位。
$modelQuery = & $PY app.py --print-model-dir 2>&1
if ($LASTEXITCODE -ne 0) {
    $modelQuery | ForEach-Object { Write-Host $_ }
    throw "模型目录解析失败。上面的信息里列了所有试过的路径。"
}
$effectiveModel = ($modelQuery | Select-Object -First 1)

Write-Host "==> 启动 pv-embed-server" -ForegroundColor Cyan

$effectiveHost = if ($env:PV_HOST) { $env:PV_HOST } else { "127.0.0.1" }
$effectivePort = if ($env:PV_PORT) { $env:PV_PORT } else { "8917" }

Write-Host "    模型目录 : $effectiveModel"
Write-Host "    监听     : ${effectiveHost}:${effectivePort}"
Write-Host "    健康检查 : http://${effectiveHost}:${effectivePort}/health"
Write-Host "    指标     : http://${effectiveHost}:${effectivePort}/metrics"
Write-Host "    停止     : Ctrl+C"
Write-Host "    联网     : 已强制关闭，只用本地模型"
Write-Host ""

& $PY app.py
exit $LASTEXITCODE
