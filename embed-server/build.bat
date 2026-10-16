@echo off
chcp 65001 >nul 2>&1
setlocal EnableExtensions
cd /d "%~dp0"

rem ===========================================================================
rem  把 pv-embed-server 的 Python 侧车打包成可分发的 Windows 可执行文件
rem
rem  用法：
rem    build.bat                             打包，默认 onedir 目录形态
rem    build.bat --model-dir D:\...\model    指定冒烟测试用的模型目录
rem    build.bat --skip-smoke                跳过冒烟测试
rem    build.bat --onefile                   打成单文件，不推荐，见下面的说明
rem    build.bat --help                      显示帮助
rem
rem  产物：
rem    dist\pv-embed-server\pv-embed-server.exe    （--onedir，推荐）
rem    dist\pv-embed-server.exe                    （--onefile）
rem
rem  重要说明，先看完再跑：
rem  1. 打包过程要 10~30 分钟，产物 3~5 GB。大头是 torch 和它的 CUDA 运行库。
rem  2. 模型那 4.2 GB 永远在二进制外面，必须单独放。这个打包只解决"目标机器不用装 Python"。
rem  3. 冻结后默认模型目录变成 exe 同级的 kit\models\qwen3-vl-embedding-2b；
rem     也可以直接用 PV_MODEL_DIR 环境变量指过去。
rem  4. 未签名的几 GB exe 会触发 SmartScreen / 杀软告警，这是预期行为。
rem ===========================================================================

set "MODE_DIR=--onedir"
set "ARG_MODEL_DIR="
set "SKIP_SMOKE="
set "OUT_EXE_PREVIEW=dist\pv-embed-server\pv-embed-server.exe"

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--onefile" (
    set "MODE_DIR=--onefile"
    set "OUT_EXE_PREVIEW=dist\pv-embed-server.exe"
    shift
    goto parse
)
if /i "%~1"=="--model-dir" (
    set "ARG_MODEL_DIR=%~2"
    shift
    shift
    goto parse
)
if /i "%~1"=="--skip-smoke" (
    set "SKIP_SMOKE=1"
    shift
    goto parse
)
if /i "%~1"=="--help" goto usage
if /i "%~1"=="-h"     goto usage
echo [错误] 未知参数 %~1
goto usage
:parsed

set "PY=%~dp0.venv\Scripts\python.exe"

echo.
echo ============================================================
echo  打包 pv-embed-server
echo ============================================================
echo  形态      : %MODE_DIR%
echo  产物预计  : %OUT_EXE_PREVIEW%
echo  预计耗时  : 10 ~ 30 分钟，产物 3 ~ 5 GB
echo.

rem ---------------------------------------------------------------------------
rem [1/5] 检查环境
rem ---------------------------------------------------------------------------
echo [1/5] 检查 .venv
if not exist "%PY%" (
    echo [失败] 找不到 %PY%
    echo        先运行 setup.bat 建环境。
    exit /b 1
)

"%PY%" -c "import torch,transformers,fastapi" >nul 2>&1
if errorlevel 1 (
    echo [失败] .venv 里缺核心依赖。先运行 setup.bat 把依赖装齐。
    exit /b 1
)

rem ---------------------------------------------------------------------------
rem [2/5] 装 pyinstaller
rem ---------------------------------------------------------------------------
echo [2/5] 检查 pyinstaller
"%PY%" -c "import PyInstaller" >nul 2>&1
if not errorlevel 1 goto have_pyi

echo       未安装，正在装
uv pip install --python "%PY%" pyinstaller
if errorlevel 1 goto fail

:have_pyi
"%PY%" -c "import PyInstaller;print('      pyinstaller',PyInstaller.__version__)"
if errorlevel 1 goto fail

rem ---------------------------------------------------------------------------
rem [3/5] 清理旧产物
rem ---------------------------------------------------------------------------
echo [3/5] 清理旧产物
if exist build rmdir /s /q build
if exist dist rmdir /s /q dist
mkdir build
mkdir dist

rem ---------------------------------------------------------------------------
rem [4/5] 打包
rem
rem  --collect-all 是必须的，不是保险起见：
rem  app.py 用 importlib 在运行时加载 <模型目录>\scripts\qwen3_vl_embedding.py，
rem  那个文件里的 import 是 transformers.models.qwen3_vl.* 这种深层路径。
rem  PyInstaller 的静态分析看不见动态加载的文件，不 collect 就会在运行时
rem  报 ModuleNotFoundError。--collect-submodules uvicorn 同理：
rem  uvicorn 的 loop / protocol 实现是按字符串延迟导入的。
rem ---------------------------------------------------------------------------
echo [4/5] 打包中，这一步最慢，不要关窗口
echo.

"%PY%" -m PyInstaller --noconfirm --clean --console %MODE_DIR% --name pv-embed-server --distpath dist --workpath build --specpath build --collect-all torch --collect-all torchvision --collect-all transformers --collect-all qwen_vl_utils --collect-all accelerate --collect-all safetensors --collect-submodules uvicorn --copy-metadata huggingface-hub --exclude-module tkinter --exclude-module IPython --exclude-module pytest --exclude-module notebook app.py
if errorlevel 1 goto fail

if /i "%MODE_DIR%"=="--onedir" (
    set "OUT_EXE=%~dp0dist\pv-embed-server\pv-embed-server.exe"
) else (
    set "OUT_EXE=%~dp0dist\pv-embed-server.exe"
)

if not exist "%OUT_EXE%" (
    echo [失败] 打包命令返回成功，但找不到产物 %OUT_EXE%
    exit /b 1
)

echo.
echo 打包完成。产物目录内容：
dir /b dist
echo.

rem ---------------------------------------------------------------------------
rem 硬校验：产物里绝对不能有模型权重
rem   本项目要求"禁止打包模型"。PyInstaller 正常不会收源码树外的文件，
rem   但加了 --collect-all 之后不敢只靠"应该不会"，所以直接扫一遍，发现就失败。
rem ---------------------------------------------------------------------------
echo 校验产物内无模型权重
set "FOUND_MODEL="
dir /s /b "dist\*.safetensors" >nul 2>&1
if not errorlevel 1 set "FOUND_MODEL=1"
dir /s /b "dist\*.gguf" >nul 2>&1
if not errorlevel 1 set "FOUND_MODEL=1"
dir /s /b "dist\pytorch_model*.bin" >nul 2>&1
if not errorlevel 1 set "FOUND_MODEL=1"
if defined FOUND_MODEL (
    echo [失败] 产物里发现了模型权重文件 —— 模型禁止打包。
    dir /s /b "dist\*.safetensors"
    dir /s /b "dist\*.gguf"
    dir /s /b "dist\pytorch_model*.bin"
    echo        检查 app.py 有没有把模型目录牵进来，或 --collect-all 列表里有没有多东西。
    exit /b 1
)
echo       OK：产物内无 *.safetensors / *.gguf / pytorch_model*.bin
echo.

rem ---------------------------------------------------------------------------
rem [5/5] 冒烟测试
rem ---------------------------------------------------------------------------
if defined SKIP_SMOKE goto done

echo [5/5] 冒烟测试，要加载 4.2 GB 模型
echo.

if defined ARG_MODEL_DIR set "PV_MODEL_DIR=%ARG_MODEL_DIR%"
if not defined PV_MODEL_DIR (
    if exist "%~dp0models\qwen3-vl-embedding-2b\config.json" (
        set "PV_MODEL_DIR=%~dp0models\qwen3-vl-embedding-2b"
    )
)
if not defined PV_MODEL_DIR (
    if exist "%~dp0models\config.json" (
        set "PV_MODEL_DIR=%~dp0models"
    )
)
if not defined PV_MODEL_DIR (
    if exist "%~dp0..\kit\models\qwen3-vl-embedding-2b\config.json" (
        set "PV_MODEL_DIR=%~dp0..\kit\models\qwen3-vl-embedding-2b"
    )
)
if not defined PV_MODEL_DIR (
    echo [跳过] 没指定模型目录，也没在 models\ 或项目老位置找到。
    echo        用 build.bat --model-dir PATH 重跑一次，或者手动验证：
    echo        set PV_MODEL_DIR=你的模型目录
    echo        dist\pv-embed-server\pv-embed-server.exe --selftest
    goto done
)

echo       PV_MODEL_DIR=%PV_MODEL_DIR%
"%OUT_EXE%" --selftest
if errorlevel 1 (
    echo.
    echo [失败] 打包产物自检没通过。看上面的报错：
    echo        ModuleNotFoundError 之类的缺模块，加到 build.bat 的 --collect-all 列表里。
    echo        DLL 加载失败之类的，通常是 CUDA 运行库没被收进去。
    exit /b 1
)

:done
echo.
echo ============================================================
echo  完成
echo ============================================================
echo  可执行文件 : %OUT_EXE_PREVIEW%
echo.
echo  模型【不在】产物里，分发时带上两样：
echo    1. dist 下那一整个目录
echo    2. 模型目录，放到 exe 同级的 models\qwen3-vl-embedding-2b
echo       或者启动前设 PV_MODEL_DIR
echo.
echo  目标机器不需要装 Python，也不需要联网。
echo ============================================================
exit /b 0

:fail
echo.
echo [失败] 打包过程出错，错误码 %ERRORLEVEL%
echo        完整输出往上翻，最常见的两类原因：
echo          1. 磁盘空间不够，构建过程需要 15 GB 以上临时空间
echo          2. 杀软锁住了 torch 的 DLL，临时关掉实时防护再试
exit /b %ERRORLEVEL%

:usage
echo.
echo 用法： build.bat [选项]
echo.
echo   --model-dir PATH   指定冒烟测试用的模型目录
echo   --skip-smoke       只打包，不跑冒烟测试
echo   --onefile          打成单文件。不推荐：每次启动要把几 GB 解压到临时目录，
echo                      冷启动很慢，而且更容易被杀软拦
echo   --help             显示本帮助
echo.
exit /b 0
