@echo off
chcp 65001 >nul 2>&1
setlocal EnableExtensions
cd /d "%~dp0"

rem ===========================================================================
rem  启动 pv-embed-server（原生批处理，不依赖 PowerShell）
rem
rem  单进程、单 worker。多 worker 会各自加载一份 4.2 GB 权重，8 GB 显存直接爆。
rem
rem  用法：
rem    run.bat
rem    run.bat --port 8917 --model-dir D:\models\qwen3-vl-embedding-2b
rem    run.bat --cpu                    强制走 CPU
rem    run.bat --max-pixels 1310720     显存不够时往下调
rem    run.bat --help                   显示帮助
rem ===========================================================================

set "PY=%~dp0.venv\Scripts\python.exe"
if not exist "%PY%" (
    echo [错误] 找不到 %PY%
    echo        先运行 setup.bat 建环境。
    exit /b 1
)

set "ARG_MODEL_DIR="
set "ARG_HOST="
set "ARG_PORT="
set "ARG_MAX_PIXELS="
set "ARG_MAX_LENGTH="
set "ARG_CPU="

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--model-dir" (
    set "ARG_MODEL_DIR=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--host" (
    set "ARG_HOST=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--port" (
    set "ARG_PORT=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--max-pixels" (
    set "ARG_MAX_PIXELS=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--max-length" (
    set "ARG_MAX_LENGTH=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--cpu" (
    set "ARG_CPU=1"
    shift
    goto parse
)
if /i "%~1"=="--help" goto usage
if /i "%~1"=="-h"     goto usage
echo [错误] 未知参数 %~1
goto usage
:parsed

rem 命令行参数覆盖环境变量
if defined ARG_MODEL_DIR  set "PV_MODEL_DIR=%ARG_MODEL_DIR%"
if defined ARG_HOST       set "PV_HOST=%ARG_HOST%"
if defined ARG_PORT       set "PV_PORT=%ARG_PORT%"
if defined ARG_MAX_PIXELS set "PV_MAX_PIXELS=%ARG_MAX_PIXELS%"
if defined ARG_MAX_LENGTH set "PV_MAX_LENGTH=%ARG_MAX_LENGTH%"
if defined ARG_CPU        set "PV_DEVICE=cpu"

rem 生效值，仅用于显示
set "EFF_HOST=127.0.0.1"
if defined PV_HOST set "EFF_HOST=%PV_HOST%"
set "EFF_PORT=8917"
if defined PV_PORT set "EFF_PORT=%PV_PORT%"

rem 先让 app.py 解析模型目录（只做本地文件检查，不加载模型、不联网）。
rem 解析失败会带出「所有试过的路径」，比等启动到一半才报错好定位得多。
set "EFF_MODEL="
set "TMP_MD=%TEMP%\pv_model_dir.%RANDOM%.txt"
"%PY%" app.py --print-model-dir > "%TMP_MD%" 2>&1
if errorlevel 1 (
    type "%TMP_MD%"
    del "%TMP_MD%" >nul 2>&1
    echo.
    echo [失败] 模型目录解析失败。上面的信息里列了所有试过的路径。
    exit /b 1
)
set /p EFF_MODEL=<"%TMP_MD%"
del "%TMP_MD%" >nul 2>&1

echo.
echo ============================================================
echo  启动 pv-embed-server
echo ============================================================
echo  模型目录  : %EFF_MODEL%
echo  监听      : %EFF_HOST%:%EFF_PORT%
echo  健康检查  : http://%EFF_HOST%:%EFF_PORT%/health
echo  指标      : http://%EFF_HOST%:%EFF_PORT%/metrics
echo  停止      : Ctrl+C
echo  联网      : 已强制关闭，只用本地模型
if defined ARG_CPU echo  设备      : CPU 强制
if defined PV_MAX_PIXELS echo  max_pixels: %PV_MAX_PIXELS%
echo.

"%PY%" app.py
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" (
    echo.
    echo [退出] 服务异常退出，错误码 %RC%
)
exit /b %RC%

:usage
echo.
echo 用法： run.bat [选项]
echo.
echo   --model-dir PATH   指定模型目录。不指定时按顺序自动找（程序目录 = 本批处理所在目录）：
echo                        [程序目录]\models 本身
echo                        [程序目录]\models 下的任意子目录
echo                        [程序目录]\..\kit\models\qwen3-vl-embedding-2b
echo   --host ADDR        监听地址，默认 127.0.0.1
echo   --port N           端口，默认 8917
echo   --max-pixels N     图像像素上限，默认 1843200；改了就会偏离官方口径
echo   --max-length N     序列长度上限，默认 8192
echo   --cpu              强制走 CPU
echo   --help             显示本帮助
echo.
echo  也可以直接用环境变量： PV_MODEL_DIR / PV_HOST / PV_PORT / PV_MAX_PIXELS /
echo  PV_MAX_LENGTH / PV_DEVICE / PV_TORCH_DTYPE / PV_ALLOW_TRUNCATION / PV_LOG_LEVEL
echo.
exit /b 0
