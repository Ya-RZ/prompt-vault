#Requires -Version 5.1
# 备用入口。主入口是 setup.bat（原生批处理，不依赖 PowerShell）。
# 两者功能等价、参数名不同（-CpuOnly 对应 --cpu），改一边必须同步改另一边。
<#
.SYNOPSIS
    建好 pv-embed-server 的 Python 环境并装齐依赖。

.DESCRIPTION
    默认装 CUDA 12.8 版的 torch 2.8.0（对齐官方 Qwen3-VL-Embedding 的 torch==2.8.* 约束）。
    装完自动跑一次 --selftest（会加载 4.2 GB 模型，约 30 秒）。

.EXAMPLE
    .\setup.ps1                 # 建环境 + 装依赖 + 自检
    .\setup.ps1 -SkipSelftest   # 只装依赖
    .\setup.ps1 -CpuOnly        # 没有 NVIDIA 卡时装 CPU 版 torch（先在笔记本上验证代码）
#>
param(
    [string]$Python = "3.12",
    [switch]$SkipSelftest,
    [switch]$CpuOnly
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$TORCH_VER   = "2.8.0"
$TVISION_VER = "0.23.0"
$CUDA_TAG    = "cu128"
$CUDA_INDEX  = "https://download.pytorch.org/whl/$CUDA_TAG"
$CPU_INDEX   = "https://download.pytorch.org/whl/cpu"

Write-Host "==> pv-embed-server 环境准备" -ForegroundColor Cyan
Write-Host "    目录   : $PSScriptRoot"
Write-Host "    Python : $Python"
if ($CpuOnly) {
    Write-Host "    torch  : $TORCH_VER (CPU)"
} else {
    Write-Host "    torch  : $TORCH_VER+$CUDA_TAG"
    Write-Host "    index  : $CUDA_INDEX"
}

# ---- 0. 前置检查 ----------------------------------------------------------
if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
    Write-Host "==> 未检测到 uv，正在安装" -ForegroundColor Yellow
    powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"
    $env:Path = "$env:USERPROFILE\.local\bin;$env:Path"
    if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
        throw "uv 安装后仍找不到，请手动安装：https://docs.astral.sh/uv/getting-started/installation/"
    }
}

if (-not $CpuOnly) {
    if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {
        Write-Host "==> GPU:"
        & nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader |
            ForEach-Object { Write-Host "    $_" }
    } else {
        Write-Host "!! 没找到 nvidia-smi。若这台机器确实没有 NVIDIA 卡，请改用 -CpuOnly" -ForegroundColor Red
    }
}

# ---- 1. 虚拟环境 ----------------------------------------------------------
Write-Host "==> 创建 .venv" -ForegroundColor Cyan
uv venv .venv --python $Python
$PY = Join-Path $PSScriptRoot ".venv\Scripts\python.exe"
if (-not (Test-Path $PY)) { throw ".venv 没建起来，找不到 $PY" }

# ---- 2. 依赖 --------------------------------------------------------------
# torch/torchvision 必须单独先装：混在一条命令里容易被解析成 PyPI 上的 CPU 版。
Write-Host "==> 安装 torch / torchvision" -ForegroundColor Cyan
if ($CpuOnly) {
    uv pip install --python $PY "torch==$TORCH_VER" "torchvision==$TVISION_VER" --index-url $CPU_INDEX
} else {
    uv pip install --python $PY "torch==$TORCH_VER" "torchvision==$TVISION_VER" --index-url $CUDA_INDEX
}
if ($LASTEXITCODE -ne 0) { throw "torch 安装失败" }

Write-Host "==> 安装其余依赖" -ForegroundColor Cyan
uv pip install --python $PY `
    "fastapi>=0.115" "uvicorn[standard]>=0.30" "prometheus-client>=0.20" `
    "transformers>=4.57.3" "accelerate>=1.12.0" "qwen-vl-utils>=0.0.14" `
    "pillow>=10" "numpy>=1.26" "scipy>=1.11"
if ($LASTEXITCODE -ne 0) { throw "依赖安装失败" }

# ---- 3. 版本核对（防止装成 CPU 版 torch 却不知道）--------------------------
Write-Host "==> 版本核对" -ForegroundColor Cyan
if ($CpuOnly) { $env:PV_SETUP_EXPECT_CUDA = "0" } else { $env:PV_SETUP_EXPECT_CUDA = "1" }
$env:PV_SETUP_CUDA_INDEX = $CUDA_INDEX

# 单引号 here-string：PowerShell 不做变量替换，避免把 $ 交给 Python 之前被吃掉
& $PY -c @'
import os, sys, torch, torchvision, transformers
print(f"    python        {sys.version.split()[0]}")
print(f"    torch         {torch.__version__}")
print(f"    torchvision   {torchvision.__version__}")
print(f"    transformers  {transformers.__version__}")
ok = torch.cuda.is_available()
print(f"    cuda 可用     {ok}")
if ok:
    p = torch.cuda.get_device_properties(0)
    print(f"    GPU           {p.name}")
    print(f"    显存          {p.total_memory / 1024**3:.1f} GB")
if os.environ.get("PV_SETUP_EXPECT_CUDA") == "1" and not ok:
    print("    !! torch 装成了 CPU 版，或驱动 / CUDA 版本不匹配。")
    print("       检查索引: " + os.environ.get("PV_SETUP_CUDA_INDEX", ""))
'@

# ---- 4. 自检 --------------------------------------------------------------
if ($SkipSelftest) {
    Write-Host "==> 已跳过自检。启动： .\run.ps1" -ForegroundColor Green
    exit 0
}

Write-Host "==> 自检（要加载 4.2 GB 模型，请稍等）" -ForegroundColor Cyan
& $PY app.py --selftest
if ($LASTEXITCODE -ne 0) {
    throw "自检失败。先看上面的报错，再对照 README.md 的「已知坑」一节。"
}

Write-Host ""
Write-Host "==> 完成。启动服务： .\run.ps1" -ForegroundColor Green
