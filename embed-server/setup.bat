@echo off
chcp 65001 >nul 2>&1
setlocal EnableExtensions
cd /d "%~dp0"

rem ===========================================================================
rem  pv-embed-server 环境准备（原生批处理，不依赖 PowerShell）
rem
rem  用法：
rem    setup.bat                  建环境 + 装依赖 + 自检
rem    setup.bat --skip-selftest  只装依赖
rem    setup.bat --cpu            没有 NVIDIA 卡时装 CPU 版 torch
rem    setup.bat --python 3.12    指定 Python 版本
rem    setup.bat --help           显示帮助
rem ===========================================================================

set "TORCH_VER=2.8.0"
set "TVISION_VER=0.23.0"
set "CUDA_INDEX=https://download.pytorch.org/whl/cu128"
set "CPU_INDEX=https://download.pytorch.org/whl/cpu"
set "PYTHON_VER=3.12"
set "SKIP_SELFTEST="
set "CPU_ONLY="

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--skip-selftest" (
    set "SKIP_SELFTEST=1"
    shift
    goto parse
)
if /i "%~1"=="--cpu" (
    set "CPU_ONLY=1"
    shift
    goto parse
)
if /i "%~1"=="--python" (
    set "PYTHON_VER=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--help" goto usage
if /i "%~1"=="-h"     goto usage
echo [错误] 未知参数 %~1
goto usage
:parsed

echo.
echo ============================================================
echo  pv-embed-server 环境准备
echo ============================================================
echo  目录        : %~dp0
echo  Python      : %PYTHON_VER%
if defined CPU_ONLY (
    echo  torch       : %TORCH_VER% CPU 版
) else (
    echo  torch       : %TORCH_VER% + cu128
    echo  索引        : %CUDA_INDEX%
)
echo.

rem ---------------------------------------------------------------------------
rem [1/5] 确认 uv 可用
rem ---------------------------------------------------------------------------
echo [1/5] 检查 uv

where uv >nul 2>&1
if not errorlevel 1 goto have_uv

echo       未检测到 uv，尝试用 winget 安装
where winget >nul 2>&1
if errorlevel 1 goto try_pip

winget install --id=astral-sh.uv -e --accept-source-agreements --accept-package-agreements --silent
set "PATH=%USERPROFILE%\.local\bin;%PATH%"
where uv >nul 2>&1
if not errorlevel 1 goto have_uv

:try_pip
echo       winget 不可用，改用 pip 安装
python -m pip install --user --upgrade uv
if errorlevel 1 goto no_uv
set "PATH=%APPDATA%\Python\Scripts;%PATH%"
where uv >nul 2>&1
if not errorlevel 1 goto have_uv

:no_uv
echo.
echo [失败] 装不上 uv。请手动安装后重跑本脚本：
echo        https://docs.astral.sh/uv/getting-started/installation/
echo        装完新开一个 cmd 窗口，再执行 setup.bat
exit /b 1

:have_uv

rem ---------------------------------------------------------------------------
rem [2/5] 创建虚拟环境
rem ---------------------------------------------------------------------------
echo [2/5] 创建 .venv
uv venv .venv --python %PYTHON_VER%
if errorlevel 1 goto fail

set "PY=%~dp0.venv\Scripts\python.exe"
if not exist "%PY%" (
    echo [失败] .venv 没建起来，找不到 %PY%
    exit /b 1
)

rem ---------------------------------------------------------------------------
rem [3/5] 装 torch / torchvision
rem       必须单独先装：和其余依赖混在一条命令里，很容易从 PyPI 拉到 CPU 版，
rem       而装错了不会报错，只会在运行时发现 cuda 不可用。
rem ---------------------------------------------------------------------------
echo [3/5] 安装 torch 和 torchvision
if defined CPU_ONLY (
    uv pip install --python "%PY%" torch==%TORCH_VER% torchvision==%TVISION_VER% --index-url %CPU_INDEX%
) else (
    uv pip install --python "%PY%" torch==%TORCH_VER% torchvision==%TVISION_VER% --index-url %CUDA_INDEX%
)
if errorlevel 1 goto fail

rem ---------------------------------------------------------------------------
rem [4/5] 装其余依赖
rem       刻意不装 decord / opencv / datasets / matplotlib / ipykernel：
rem       那是视频与训练用的；decord 在 Windows 上只有空壳包。
rem ---------------------------------------------------------------------------
echo [4/5] 安装其余依赖
uv pip install --python "%PY%" "fastapi>=0.115" "uvicorn[standard]>=0.30" "prometheus-client>=0.20" "transformers>=4.57.3" "accelerate>=1.12.0" "qwen-vl-utils>=0.0.14" "pillow>=10" "numpy>=1.26" "scipy>=1.11"
if errorlevel 1 goto fail

rem ---------------------------------------------------------------------------
rem 版本核对
rem ---------------------------------------------------------------------------
echo.
echo 版本核对
"%PY%" -c "import sys,torch,torchvision,transformers;print('  python       ',sys.version.split()[0]);print('  torch        ',torch.__version__);print('  torchvision  ',torchvision.__version__);print('  transformers ',transformers.__version__)"
if errorlevel 1 goto fail

"%PY%" -c "import torch,sys;ok=torch.cuda.is_available();print('  cuda 可用    ',ok);print('  GPU          ',torch.cuda.get_device_name(0) if ok else '无');sys.exit(0 if ok else 1)"
if errorlevel 1 (
    if defined CPU_ONLY (
        echo  [提示] 当前是 CPU 模式，这是预期行为
    ) else (
        echo.
        echo  [警告] torch 装成了 CPU 版，或者驱动与 CUDA 版本不匹配。
        echo         索引用的是 %CUDA_INDEX%
        echo         先确认 nvidia-smi 能跑，再看 README 的「已知坑」第 1 条。
        echo.
    )
)

rem ---------------------------------------------------------------------------
rem [5/5] 自检
rem ---------------------------------------------------------------------------
if defined SKIP_SELFTEST goto done

echo [5/5] 自检，需加载 4.2 GB 模型，请稍等
echo.
"%PY%" app.py --selftest
if errorlevel 1 (
    echo.
    echo [失败] 自检没通过。先看上面的报错，再对照 README.md 的「已知坑」一节。
    exit /b 1
)

:done
echo.
echo ============================================================
echo  完成。启动服务： run.bat
echo ============================================================
exit /b 0

:fail
echo.
echo [失败] 上一条命令返回了错误，脚本中止。错误码 %ERRORLEVEL%
echo        把上面的完整输出贴回来定位。
exit /b %ERRORLEVEL%

:usage
echo.
echo 用法： setup.bat [选项]
echo.
echo   --cpu              装 CPU 版 torch，用于没有 NVIDIA 卡的机器
echo   --python VER       指定 Python 版本，默认 3.12
echo   --skip-selftest    只装依赖，不跑自检
echo   --help             显示本帮助
echo.
exit /b 0
