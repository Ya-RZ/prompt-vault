# pv-embed-server — prompt-vault 的编码侧车

把 **官方 `Qwen3VLEmbedder`**（QwenLM/Qwen3-VL-Embedding 仓库的参考实现）包成一个本地 HTTP 服务，
让 `prompt-vault` 的 Rust 主进程通过 localhost 调它。

**为什么不做进主进程**：官方实现是 Python + transformers。只要向量由它产出，就和官方参考实现
逐位一致（余弦 1.0），不存在"自己用 candle / llama.cpp 重写，然后差 0.99"的问题。
代价是多一个进程和多一份 Python 环境——换来的是可验证的对齐。

---

## 0. 边界（这个服务不做什么）

- 不改 `prompt-vault` 的任何 Rust 代码。主进程那边怎么接，是下一步的事。
- 不做鉴权、限流、多 worker。默认只监听 `127.0.0.1`，不要直接暴露到公网。
- 不做视频（只做文本 / 图片 / 图文混合），所以不装 `decord`、`opencv`。
- 不做微调、不做 NSFW 特判——嵌入模型没有审核层，任何图片都能编码。

---

## 1. 前置条件

| 需要 | 说明 |
| --- | --- |
| Windows 10/11 | 入口是 `.bat`，不依赖 PowerShell（官方仓库那个 `setup_environment.sh` 是 bash，Windows 上跑不了，所以才有了这份） |
| Python 3.12 | `setup.ps1` 会自己用 uv 拉一个 3.12，系统里不用预装 |
| [uv](https://docs.astral.sh/uv/) | 没装的话 `setup.ps1` 会自动装 |
| NVIDIA 驱动 | 你的机器是 610.47 / CUDA UMD 13.3，向下兼容，跑 cu128 没问题 |
| 模型文件 | 约 4.2 GB，见下一节 |
| 显存 | 2B 权重 bf16 约 4.2 GB。8 GB 卡能跑，但视觉 token 一多就紧 |

---

## 2. 模型文件从哪来（本程序不会下载）

**程序强制离线。** `app.py` 在任何 `import transformers` 之前就设好：

```
HF_HUB_OFFLINE=1
TRANSFORMERS_OFFLINE=1
HF_HUB_DISABLE_TELEMETRY=1
HF_HUB_DISABLE_IMPLICIT_TOKEN=1
```

效果是：任何意外的联网尝试会**直接抛错**（`OfflineModeIsEnabled`），而不是静默下载几个 GB。
如果启动时报这类错，说明确实有代码想联网 —— 那正是要抓的。

所以模型必须自己准备好。先在**能联网的机器**上取一份，再拷到目标机器：

```bat
rem 这一步是你在能联网的机器上手动做的，不是本程序做的
pip install -U "huggingface_hub[cli]"
hf download Qwen/Qwen3-VL-Embedding-2B --local-dir D:\models\qwen3-vl-embedding-2b
```

### 放哪里 —— 按以下顺序自动查找

| 优先级 | 位置 | 说明 |
| :---: | --- | --- |
| 1 | `PV_MODEL_DIR` 环境变量 / `--model-dir PATH` | **显式指定就必须有效**：不合法直接报错退出，不会静默回落到下一个候选 |
| 2 | `<程序目录>\models` | 模型文件直接铺在 `models\` 下（该目录里有 `config.json`） |
| 3 | `<程序目录>\models\*` | `models\` 下任意子目录，按名字排序取第一个可用的 |
| 4 | `<程序目录>\..\kit\models\qwen3-vl-embedding-2b` | 兼容本项目原有布局 |

`<程序目录>`：直接跑 `python app.py` 时是 `embed-server\`；跑打包后的 exe 时是 exe 所在目录。

**推荐布局**（打包分发时也一样）：

```
embed-server\
├─ pv-embed-server.exe
└─ models\
   └─ qwen3-vl-embedding-2b\
      ├─ config.json
      ├─ preprocessor_config.json
      ├─ tokenizer.json
      ├─ 1_Pooling\config.json
      ├─ scripts\qwen3_vl_embedding.py
      └─ model.safetensors
```

想知道最终用的是哪个目录：

```bat
run.bat                                        rem 启动时会打印「模型目录 : ...」
.venv\Scripts\python.exe app.py --print-model-dir    rem 只打印不加载模型
```

`--print-model-dir` 非常适合排查路径问题：它只做本地文件检查，不加载模型、不联网。

### 目录里必须有什么

启动时逐个检查，缺一个就报错退出，**不会自动补齐**：

```
config.json
tokenizer.json
preprocessor_config.json
1_Pooling\config.json
scripts\qwen3_vl_embedding.py     ← 官方参考实现，本服务直接加载它
model.safetensors                 # 4,255,140,312 字节
```

找不到时会把**所有试过的路径**、以及各自缺什么，一次全部列出来。

> 如果你已经有 `prompt-vault/kit/models/qwen3-vl-embedding-2b`，那一份就是完整的官方权重，
> 不用重新取，按上表第 4 条能直接找到。

---

## 3. 三步启动

```bat
rem ① 装环境（会自动跑一次自检）
setup.bat

rem ②（可选）自检单独再跑一遍，看清楚设备/维度/指纹
.venv\Scripts\python.exe app.py --selftest

rem ③ 起服务
run.bat
```

服务默认监听 `http://127.0.0.1:8917`。

```bat
rem 探活
curl http://127.0.0.1:8917/health
```

`/health` 里必须看到 `"device": "cuda:0"`。如果显示 `cpu`，说明 torch 装成了 CPU 版，
回去看下面的「已知坑」第 1 条。

**先在没显卡的笔记本上验证代码**：`setup.bat --cpu` 然后 `run.bat --cpu`。

### 目录里的两套脚本

| 文件 | 说明 |
| --- | --- |
| **`setup.bat` / `run.bat`** | **主入口。** 原生批处理，不依赖 PowerShell，双击或直接在 cmd 里跑都行。首行 `chcp 65001` 是为了让中文正常显示。 |
| `setup.ps1` / `run.ps1` | 备用。功能等价，参数名不同（`-CpuOnly` 对应 `--cpu`）。**两边逻辑是重复的，改一边要改另一边。** |

`.bat` 参数：

```bat
setup.bat [--cpu] [--python 3.12] [--skip-selftest] [--help]

run.bat   [--model-dir PATH] [--host ADDR] [--port N]
          [--max-pixels N] [--max-length N] [--cpu] [--help]
```

如果你看到中文变成乱码，说明控制台代码页没切成 UTF-8。手动执行一次 `chcp 65001` 再跑即可；
或者把两个 `.bat` 用记事本另存为 ANSI —— 但不建议，那会在非中文 Windows 上反而乱码。

---

## 4. 配置项

全部走环境变量，`run.ps1` 的参数会覆盖它们。

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PV_MODEL_DIR` | 无（按 §2 的顺序自动查找） | 模型目录。**设了就必须有效**，否则启动直接报错；等价于 `--model-dir PATH` |
| `PV_HOST` | `127.0.0.1` | 监听地址。**不要改成 0.0.0.0 除非你清楚后果**（服务没有任何鉴权） |
| `PV_PORT` | `8917` | 端口 |
| `PV_DEVICE` | `auto` | `auto` / `cuda` / `cpu` |
| `PV_TORCH_DTYPE` | `auto` | `auto` 交给 transformers 按 checkpoint 决定（官方是 bfloat16）。可强制 `bfloat16` / `float16` / `float32` |
| `PV_MAX_PIXELS` | `1843200` | 官方参考脚本的 `MAX_PIXELS`。**改了就会偏离官方口径**，指纹也跟着变 |
| `PV_MIN_PIXELS` | `4096` | 官方 `MIN_PIXELS` |
| `PV_MAX_LENGTH` | `8192` | 官方 `MAX_LENGTH`。图像 token 数 = 像素数 ÷ 1024，超了会被截断 |
| `PV_ALLOW_TRUNCATION` | `0` | `0` = 超预算返回 413；`1` = 放过（会静默截断，向量变垃圾，仅排查用） |
| `PV_LOG_LEVEL` | `INFO` | 日志级别。日志是 JSON lines，直接进 stdout |

---

## 5. API

### `GET /health`

```json
{
  "status": "READY",
  "model": "Qwen/Qwen3-VL-Embedding-2B",
  "device": "cuda:0",
  "dim": 2048,
  "pooling": "lasttoken",
  "max_pixels": 1843200,
  "max_length": 8192,
  "template_overhead": 22,
  "allow_truncation": false,
  "fingerprint": "Qwen3-VL-Embedding-2B|bfloat16|dim=2048|pool=lasttoken|norm=L2|max_pixels=1843200|max_length=8192|script=xxxxxxxxxxxx|weights=4255140312",
  "load_seconds": 12.3
}
```

### `POST /v1/embeddings`

`input` 可以是单个对象或数组；每一项给 `text`、`image`、或两者都给（图文融合成一个向量）。

```powershell
curl -X POST http://127.0.0.1:8917/v1/embeddings `
  -H "Content-Type: application/json" `
  -d '{\"input\": [{\"text\": \"一只在窗台上睡觉的橘猫\"}]}'

curl -X POST http://127.0.0.1:8917/v1/embeddings `
  -H "Content-Type: application/json" `
  -d '{\"input\": [{\"image\": \"D:\\pics\\a.jpg\"}]}'

curl -X POST http://127.0.0.1:8917/v1/embeddings `
  -H "Content-Type: application/json" `
  -d '{\"input\": [{\"text\": \"暖色调，极简风格\", \"image\": \"D:\\pics\\a.jpg\"}]}'
```

`image` 接受三种形态：**本地路径** / **http(s) URL** / **`data:image/...;base64,...`**。

返回（`data[i]` 与 `input[i]` 同序）：

```json
{
  "object": "list",
  "model": "Qwen/Qwen3-VL-Embedding-2B",
  "dim": 2048,
  "fingerprint": "Qwen3-VL-Embedding-2B|bfloat16|...",
  "data": [
    {
      "object": "embedding",
      "index": 0,
      "kind": "image",
      "embedding": [0.013, -0.041, ...],
      "resized": "1632x1088",
      "vision_tokens": 1734
    }
  ],
  "usage": { "prompt_tokens": 0, "total_tokens": 0 },
  "elapsed_seconds": 0.42
}
```

`resized` 和 `vision_tokens` 是额外的诊断字段——**这两个数字是排查质量问题的第一现场**，
出问题时先看它们对不对。

### `GET /metrics`

Prometheus 文本格式。指标：`pv_embed_requests_total{status}`、`pv_embed_items_total{kind}`、
`pv_embed_errors_total{reason}`、`pv_embed_latency_seconds`、`pv_embed_vision_tokens`、
`pv_embed_inflight`、`pv_embed_info{fingerprint,device,max_pixels,dim}`。

### 错误码

| 码 | 含义 |
| --- | --- |
| 400 | 请求体不是 JSON / 缺 `input` / 某项缺 `text` 和 `image` / 图片路径不存在 / 宽高比 > 200 |
| 413 | **图像会被截断**：算出来的 `seq_len > max_length`。响应体里有 `plan`，告诉你会归到多大、占多少 token |
| 500 | 编码内部错误。详情在服务端日志里，响应体不回显 |

413 是刻意设计的：**超预算时宁可报错，也不要给你一个看起来正常、实际是垃圾的向量。**

---

## 6. 指纹与可复现性

### 可复现性（本机实测，不是推测）

| 场景 | 结果 |
| --- | --- |
| 同一输入、同一批次形状，重复调用 | **逐位相同**（模型是确定性的） |
| 图片：单独编码 vs 混在 `[文本, 图片]` 批次里编码 | **逐位相同**（图片向量对批次构成免疫） |
| 文本：单独编码 vs 混批编码 | 余弦 **0.999868**（右填充导致的 bf16 kernel 差异） |
| 向量 L2 模长 | **不会精确等于 1**（实测 1.000298 / 1.000717） |

最后一条不是 bug：官方是在 **bf16** 张量上做 `F.normalize` 的，输出精度就是 bf16。本服务刻意
不"修正"它——重新在 float32 里归一化会偏离官方，那才是 bug。

前两条里文本的批次漂移，实践含义是：**如果你需要严格可复现的文本向量，保持批次形状稳定**；
否则接受它——它比"跨实现差异"（不同后端之间约 0.99）小两个数量级。

### 指纹

`fingerprint` 把决定"向量落在哪个空间"的每个因素都编进去了：模型名、dtype、维度、池化方式、
归一化、`max_pixels`、`max_length`、官方脚本的 sha256 前 12 位、权重字节数。

**客户端的铁律**：写入索引前比对指纹，不一致就拒绝。原因很具体：

- 同一个模型、**同一个维度**，换了 dtype / max_pixels / 推理后端，向量会落在不同位置，
  而且**不会报错**——余弦照样算得出来，只是排序悄悄变差。
- 官方参考脚本和仓库 `preprocessor_config.json` 的 `max_pixels` 就不一样（1843200 vs 1310720）。
  两者都能跑，但**不是一个空间**，混进同一个索引会静默污染。

---

## 7. 已知坑（都是实测过的，不是推测）

1. **torch 装成 CPU 版**。PyPI 上的 Windows wheel 是 CPU 构建；CUDA 版只在
   `https://download.pytorch.org/whl/cu128` 上。`setup.ps1` 已经分成两步装来避免这个问题，
   装完还会打印 `cuda 可用` 让你确认。torch 2.8.0 的 Windows wheel 只有 cu126 / cu128 / cu129
   三档，**没有 cu130**。
2. **`torchvision` 不能省**。`qwen_vl_utils.vision_process` 在模块顶层就 `import torchvision`，
   少了它连 import 都过不去。
3. **`decord` 不要装**。它只在视频路径的函数内部 import；PyPI 上 Windows 只有
   `decord-0.6.0-py3-none-win_amd64.whl` 一个空壳包。官方 `pyproject.toml` 里有它，
   但那是为了视频，本服务用不到。
4. **不要开 `flash_attention_2`**。官方注释里写它是可选加速，Windows 上基本装不了。
5. **官方 `scripts/setup_environment.sh` 是 bash 脚本**，Windows 上跑不了。别去 clone 官方仓库
   然后试图跑它，直接用这份 `setup.bat`。
6. **图片超预算会静默截断**。这是最隐蔽的坑：处理器 `truncation=True, max_length=8192`
   会把超出的 token 直接砍掉，**图像 token 被吃掉，不报错，向量维度正确、归一化正确，但内容是垃圾**，
   而且不同图片被砍的位置不一样，错误还不一致。本服务在编码前用官方 `smart_resize` 做预检，
   超了就返回 413。
7. **8 GB 显存的实际天花板不是 `max_pixels`**。1734 个视觉 token（官方默认对 5400×3603 的结果）
   已经接近上限；再往上 attention 是 O(n²)，很容易 OOM。显存不够时**往下调 `PV_MAX_PIXELS`**，
   不要往上调。
8. **视频/大图很慢**。CPU 16 核上：文本约 1.3 s/条，512² 图约 19 s/张，
   1734 token 的大图约 143 s/张。GPU 会快一到两个数量级，但没有批处理优化。
9. **不要开多 worker**。`uvicorn --workers N` 或 `gunicorn` 会让每个进程各加载一份 4.2 GB
   权重，8 GB 显存直接爆。本服务的并发靠一个进程内的锁串行化，这是刻意的。

---

## 8. 换 CUDA 版本

改两处：`pyproject.toml` 的 `[[tool.uv.index]] url`，和 `setup.ps1` 顶部的 `$CUDA_TAG`。

可选项（torch 2.8.0 的 Windows wheel 实测存在）：`cu126`、`cu128`、`cu129`。
`cu129` 对应的 torchvision 只有 `0.23.0` 一个版本，选择面窄；`cu128` 最稳。

## 9. 换 `max_pixels`

改 `PV_MAX_PIXELS`。但要知道代价：

- **改小**：偏离官方口径，指纹变化，需要用新指纹**全量重编码**。
- **改大**：视觉塔的原生分辨率只有 768×768（576 个 token），官方默认的 1843200 已经是它的 3.1 倍。
  再往上买的是位置编码插值，收益递减；而且要小心撞上第 7 条的显存墙和第 6 条的截断线
  （`MAX_LENGTH=8192` 下，图像像素的安全上限是 8,366,080）。

---

## 10. 打包成独立可执行文件

```bat
build.bat
build.bat --model-dir D:\models\qwen3-vl-embedding-2b
build.bat --onefile                            rem 不推荐，见下面第 1 条
build.bat --skip-smoke                         rem 只打包
```

产物：`dist\pv-embed-server\pv-embed-server.exe`（`--onedir`，推荐）。

### 实测数据

| 项 | 实测 |
| --- | --- |
| 产物大小 | **5.2 GB**（Linux 构建，torch 2.14.1+cu130；Windows 上 torch 2.8.0+cu128 同量级） |
| 冷启动到可服务 | **4 秒** |
| 打包耗时 | 约 20 分钟（16 核） |
| 冻结后 `--selftest` | **PASS** —— 文本与图片都是 2048 维，重复编码逐位相同 |
| 冻结后 HTTP | `/health`、`/v1/embeddings` 文本与图片、`/metrics` 全部正常 |

### 为什么必须用 `--collect-all`

`app.py` 用 `importlib.util.spec_from_file_location(...).exec_module()` 在**运行时**加载
`<模型目录>\scripts\qwen3_vl_embedding.py`。那个文件里的 import 是
`transformers.models.qwen3_vl.modeling_qwen3_vl` 这种深层路径 ——
**PyInstaller 的静态分析看不到动态加载的文件**，不显式 collect 就会在冻结后
`ModuleNotFoundError`。所以 `build.bat` 里有：

- `--collect-all transformers` / `qwen_vl_utils` / `torch` / `torchvision` / `accelerate` / `safetensors`
- `--collect-submodules uvicorn` —— uvicorn 的 loop / protocol 实现是按字符串延迟导入的，同理

这个做法很钝（会多收不少用不到的东西），但它是"能跑"和"跑不起来"的区别。
缺模块时先看 `build\pv-embed-server\warn-pv-embed-server.txt`，里面列了所有没找到的可选模块。

### 冻结后的模型目录

打包后 `__file__` 指向 bundle 内部的 `_internal`，靠它推算默认路径会算错。
`app.py` 里做了冻结检测：

- **正常运行时**：默认 `..\kit\models\qwen3-vl-embedding-2b`，行为完全不变
- **冻结后**：默认 `<exe 所在目录>\kit\models\qwen3-vl-embedding-2b`

所以分发时要么把模型放到 exe 同级的 `kit\models\` 下，要么启动前设 `PV_MODEL_DIR`。

### 打包的坑

1. **产物是目录，不是单文件**（除非 `--onefile`）。`--onefile` 每次启动要把几 GB 解压到临时目录，
   冷启动从 4 秒变成几十秒，而且更容易被杀软拦。
2. **构建需要 15 GB 以上临时空间**（dist 约 5 GB + build 约 10 GB）。
3. **未签名的几 GB exe 会触发 SmartScreen**，这是必然的，不是脚本写错了。
4. **杀软的实时防护可能锁住 torch 的 DLL** 导致打包失败，临时关掉再试。
5. **模型永远在二进制外面，而且 `build.bat` 会硬校验这一点。** 打完包后脚本会扫描
   `dist`，只要发现有 `*.safetensors` / `*.gguf` / `pytorch_model*.bin` 就**判定构建失败**并列出
   命中路径 —— 不靠"应该不会"，而是每次都验。这个打包只解决"目标机器不用装 Python"，
   不解决模型分发：模型要另外放到 exe 同级的 `models\` 下，或者用 `PV_MODEL_DIR` 指过去。
6. **打包不会改变数值行为**：冻结前后向量逐位相同（本机已验证）。
7. **冻结后依然强制离线**：exe 里同样会设 `HF_HUB_OFFLINE=1`，目标机器不需要联网。

## 11. 怎么确认它真的对齐官方

`app.py` 直接 `exec_module` 加载 `<MODEL_DIR>/scripts/qwen3_vl_embedding.py`，
用的是同一个 `Qwen3VLEmbedder` 类、同一个 `MAX_LENGTH` / `MIN_PIXELS` / `MAX_PIXELS` 常量、
同一条 last-token 池化 + L2 归一化路径。**没有中间层，所以不存在"近似"**。

想自己验：把同一张图分别喂给官方脚本和本服务，比对向量。**在同一个批次形状下**余弦应该是
`1.000000`（同一台机器、同一个 dtype 下逐位相同）。如果明显小于 1，先看两边的 `fingerprint`
和 `/health` 里的 `max_pixels` 是否一致。
