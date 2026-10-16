#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""prompt-vault 编码侧车 —— 官方 Qwen3VLEmbedder + FastAPI。

设计约束（写死的，别随手改）：
  1. 模型只通过官方仓库脚本 <模型目录>/scripts/qwen3_vl_embedding.py 加载。
     不自己实现池化、不自己拼 chat template —— 这样向量才等于官方参考实现的向量。
  2. 单进程、单模型实例、串行编码：模型不是线程安全的，且显存里只装得下一份。
  3. 每个响应对带 fingerprint：客户端据此拒绝把不同来源的向量混进同一个索引。

启动：
    python app.py                        # 起服务
    python app.py --selftest             # 只跑一次自检（加载模型 + 编码文本 + 编码图）后退出

配置全部走环境变量，见 README.md。
"""

from __future__ import annotations

import base64
import hashlib
import io
import json
import logging
import os
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

# FastAPI / Prometheus 必须在模块层导入：本文件启用了 `from __future__ import annotations`，
# 注解在运行时是字符串，FastAPI 靠 `func.__globals__` 解析类型。函数内导入的 `Request`
# 不在模块全局里，会被当成查询参数，所有请求返回 422。
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, Response
from prometheus_client import CONTENT_TYPE_LATEST, Counter, Gauge, Histogram, generate_latest

# --------------------------------------------------------------------------
# 1. 配置（必须在 import torch 之前处理，因为 PV_DEVICE=cpu 靠改 CUDA_VISIBLE_DEVICES 生效）
# --------------------------------------------------------------------------


def _env(name: str, default: str) -> str:
    v = os.environ.get(name, "").strip()
    return v if v else default


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        return int(raw)
    except ValueError:
        raise SystemExit(f"[配置错误] {name} 必须是整数，收到 {raw!r}") from None


HERE = Path(__file__).resolve().parent

# --------------------------------------------------------------------------
# 强制离线 —— 必须在任何 import transformers / huggingface_hub 之前生效
#
# 本服务只加载本地模型，**绝不从 Hugging Face 下载任何东西**。
# 设成 offline 之后，任何意外的网络访问会直接抛 OfflineModeIsEnabled，
# 而不是静默下载几个 GB。启动时报这类错，说明确实有代码想联网，那正是要抓的。
# --------------------------------------------------------------------------
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
os.environ["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
os.environ["HF_HUB_DISABLE_PROGRESS_BARS"] = "1"

# “程序目录”：PyInstaller 冻结后 __file__ 指向 bundle 内部的 _internal，用 exe 所在目录；
# 未冻结时就是 app.py 所在目录。
PROGRAM_DIR = Path(sys.executable).resolve().parent if getattr(sys, "frozen", False) else HERE

HOST = _env("PV_HOST", "127.0.0.1")
PORT = _env_int("PV_PORT", 8917)
DEVICE = _env("PV_DEVICE", "auto").lower()          # auto | cuda | cpu
LOG_LEVEL = _env("PV_LOG_LEVEL", "INFO").upper()
TORCH_DTYPE = _env("PV_TORCH_DTYPE", "auto").lower()  # auto | bfloat16 | float16 | float32
# 0 = 超过序列预算直接 413 报错；1 = 放过（会静默截断，向量变垃圾，只在排查时用）
ALLOW_TRUNCATION = _env_int("PV_ALLOW_TRUNCATION", 0) == 1

if DEVICE == "cpu":
    # 让 torch.cuda.is_available() 返回 False，官方脚本就会自动落到 CPU
    os.environ["CUDA_VISIBLE_DEVICES"] = ""
elif DEVICE == "cuda":
    os.environ.pop("CUDA_VISIBLE_DEVICES", None)


# --------------------------------------------------------------------------
# 2. 结构化日志（JSON lines，便于 sidecar 日志被采集）
# --------------------------------------------------------------------------

logger = logging.getLogger("pv-embed")


class _JsonFormatter(logging.Formatter):
    def format(self, record: logging.LogRecord) -> str:
        payload: dict[str, Any] = {
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(record.created))
            + f".{int(record.msecs):03d}",
            "level": record.levelname,
            "msg": record.getMessage(),
        }
        extra = getattr(record, "extra_fields", None)
        if isinstance(extra, dict):
            payload.update(extra)
        if record.exc_info:
            payload["exc"] = self.formatException(record.exc_info)
        return json.dumps(payload, ensure_ascii=False)


_handler = logging.StreamHandler(sys.stdout)
_handler.setFormatter(_JsonFormatter())
logger.addHandler(_handler)
logger.setLevel(getattr(logging, LOG_LEVEL, logging.INFO))
logger.propagate = False


def log(msg: str, **fields: Any) -> None:
    logger.info(msg, extra={"extra_fields": fields})


# --------------------------------------------------------------------------
# 3. 定位本地模型目录（绝不下载） + 加载官方参考实现
# --------------------------------------------------------------------------

# 模型目录必须齐备的文件。少任何一个都不算可用，且不会有任何"自动补齐"。
REQUIRED_FILES: tuple[str, ...] = (
    "config.json",
    "tokenizer.json",
    "preprocessor_config.json",
    "1_Pooling/config.json",
    "scripts/qwen3_vl_embedding.py",
    "model.safetensors",
)


def _missing_in(cand: Path) -> list[str]:
    if not cand.is_dir():
        return ["<目录不存在>"]
    return [rel for rel in REQUIRED_FILES if not (cand / rel).is_file()]


def model_dir_candidates() -> list[Path]:
    """按优先级列出候选模型目录，保序去重。

    1. PV_MODEL_DIR / --model-dir      显式指定，优先级最高
    2. <程序目录>/models                直接把模型铺在 models 下
    3. <程序目录>/models/*               models 下任意子目录
    4. <程序目录>/../kit/models/...      兼容本项目原有布局
    """
    out: list[Path] = []

    def add(p: Path) -> None:
        try:
            rp = p.expanduser().resolve()
        except OSError:
            return
        if rp not in out:
            out.append(rp)

    env = os.environ.get("PV_MODEL_DIR", "").strip()
    if env:
        add(Path(env))

    for base in (PROGRAM_DIR, HERE):
        models = base / "models"
        add(models)
        add(models / "qwen3-vl-embedding-2b")
        if models.is_dir():
            try:
                for child in sorted(models.iterdir()):
                    if child.is_dir():
                        add(child)
            except OSError:
                pass
    for base in (PROGRAM_DIR, HERE):
        add(base.parent / "kit" / "models" / "qwen3-vl-embedding-2b")

    return out


def resolve_model_dir() -> Path:
    """返回第一个文件齐备的本地模型目录；都没有就报错退出。

    只会做本地文件系统检查 —— 不联网、不下载、不解压、不补齐。
    """
    cands = model_dir_candidates()
    for cand in cands:
        if not _missing_in(cand):
            return cand

    lines = ["[启动失败] 找不到可用的本地模型目录。按优先级试过这些：", ""]
    for cand in cands[:12]:
        miss = _missing_in(cand)
        if miss == ["<目录不存在>"]:
            lines.append(f"  x {cand}    （目录不存在）")
        else:
            shown = "、".join(miss[:3])
            more = f" 等 {len(miss)} 个" if len(miss) > 3 else ""
            lines.append(f"  x {cand}    缺：{shown}{more}")
    lines += [
        "",
        "本服务【不会】联网下载模型。请自行准备模型目录，三种放法任选其一：",
        f"  1. 放到  {PROGRAM_DIR / 'models'}  下（该目录直接就是模型，含 config.json）",
        f"  2. 放到  {PROGRAM_DIR / 'models' / 'qwen3-vl-embedding-2b'}",
        "  3. 设环境变量 PV_MODEL_DIR=<模型目录>，或用 --model-dir 参数",
        "",
        "模型目录必须包含：" + "、".join(REQUIRED_FILES),
    ]
    raise SystemExit("\n".join(lines))


def check_model_dir(model_dir: Path) -> Path:
    """校验一个明确指定的模型目录，返回其官方脚本路径。"""
    missing = _missing_in(model_dir)
    if missing:
        raise SystemExit(
            "[启动失败] 模型目录不可用：" + str(model_dir)
            + "\n  缺少：" + "、".join(missing)
            + "\n  必须包含：" + "、".join(REQUIRED_FILES)
        )
    return model_dir / "scripts" / "qwen3_vl_embedding.py"


def load_official_module(script: Path):
    import importlib.util

    spec = importlib.util.spec_from_file_location("qwen3_vl_embedding_official", script)
    if spec is None or spec.loader is None:
        raise SystemExit(f"[启动失败] 无法加载官方脚本：{script}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def pick_torch_dtype():
    import torch

    return {
        "auto": None,  # 交给 transformers 按 checkpoint 决定（官方是 bfloat16）
        "bfloat16": torch.bfloat16,
        "float16": torch.float16,
        "float32": torch.float32,
    }[TORCH_DTYPE]


# --------------------------------------------------------------------------
# 4. 服务主体
# --------------------------------------------------------------------------


@dataclass
class Modality:
    kind: str  # text | image | text+image


class EmbedService:
    def __init__(self, model_dir: Path | None = None) -> None:
        # 显式指定就校验它（不对直接报错，不回落）；没指定就按优先级自动找
        self.model_dir = Path(model_dir).resolve() if model_dir else resolve_model_dir()
        script = check_model_dir(self.model_dir)
        official = load_official_module(script)

        # 官方默认值就是参考脚本自己的常量，这里显式取出来，既可见又可覆盖
        self.max_length = _env_int("PV_MAX_LENGTH", official.MAX_LENGTH)
        self.min_pixels = _env_int("PV_MIN_PIXELS", official.MIN_PIXELS)
        self.max_pixels = _env_int("PV_MAX_PIXELS", official.MAX_PIXELS)

        self.lock = threading.Lock()
        self.script_sha = hashlib.sha256(script.read_bytes()).hexdigest()
        self.weights_bytes = (self.model_dir / "model.safetensors").stat().st_size

        kwargs: dict[str, Any] = {}
        dtype = pick_torch_dtype()
        if dtype is not None:
            kwargs["torch_dtype"] = dtype

        t0 = time.time()
        log("loading official Qwen3VLEmbedder", model_dir=str(self.model_dir))
        self.emb = official.Qwen3VLEmbedder(
            self.model_dir,
            max_length=self.max_length,
            min_pixels=self.min_pixels,
            max_pixels=self.max_pixels,
            **kwargs,
        )
        self.load_seconds = time.time() - t0

        import torch

        self.torch = torch
        self.device = str(next(self.emb.model.parameters()).device)

        # 从 1_Pooling/config.json 读官方声明的维度，作为契约
        pooling_cfg = json.loads((self.model_dir / "1_Pooling" / "config.json").read_text())
        self.dim = int(pooling_cfg["embedding_dimension"])
        self.pooling = pooling_cfg["pooling_mode"]

        # 视觉预处理参数（用于启动时的预算自检，不参与编码）
        prep = json.loads((self.model_dir / "preprocessor_config.json").read_text())
        self.patch_size = int(prep["patch_size"])
        self.merge_size = int(prep["merge_size"])
        self.factor = self.patch_size * self.merge_size

        # chat template 的固定开销：用一个 image 占位符渲染一次，数出非图像 token 数
        self.template_overhead = self._measure_template_overhead()

        kind = "bfloat16" if dtype is None else str(dtype).replace("torch.", "")
        self.fingerprint = "|".join(
            [
                "Qwen3-VL-Embedding-2B",
                kind,
                f"dim={self.dim}",
                f"pool={self.pooling}",
                "norm=L2",
                f"max_pixels={self.max_pixels}",
                f"max_length={self.max_length}",
                f"script={self.script_sha[:12]}",
                f"weights={self.weights_bytes}",
            ]
        )

        log(
            "ready",
            device=self.device,
            dim=self.dim,
            pooling=self.pooling,
            max_pixels=self.max_pixels,
            max_length=self.max_length,
            template_overhead=self.template_overhead,
            load_seconds=round(self.load_seconds, 1),
            fingerprint=self.fingerprint,
        )

    # -- 预算自检 -----------------------------------------------------------

    def _measure_template_overhead(self) -> int:
        """渲染一次带单个 image 占位符的 chat template，数出固定开销 token 数。"""
        conv = [
            {"role": "system", "content": [{"type": "text", "text": "Represent the user's input."}]},
            {"role": "user", "content": [{"type": "image", "image": "PLACEHOLDER"}]},
        ]
        text = self.emb.processor.apply_chat_template(conv, add_generation_prompt=True, tokenize=False)
        ids = self.emb.processor.tokenizer(text)["input_ids"]
        # 模板里恰好一个 <|image_pad|>，其余都是固定开销
        return len(ids) - 1

    def plan_image(self, pil_image) -> dict[str, int]:
        """按官方 smart_resize 算出这张图会被归到多大、要占多少视觉 token。"""
        from qwen_vl_utils.vision_process import smart_resize

        w, h = pil_image.size
        if h <= 0 or w <= 0:
            raise ValueError("图片尺寸非法")
        if max(h, w) / min(h, w) > 200:
            raise ValueError(f"宽高比超过 200（{w}x{h}），官方预处理会直接拒绝")
        h_bar, w_bar = smart_resize(h, w, self.factor, self.min_pixels, self.max_pixels)
        tokens = (h_bar // self.patch_size) * (w_bar // self.patch_size) // (self.merge_size**2)
        return {"resized_w": w_bar, "resized_h": h_bar, "vision_tokens": tokens}

    # -- 编码 ---------------------------------------------------------------

    def embed(self, items: list[dict[str, Any]]) -> tuple[list[list[float]], list[dict[str, Any]]]:
        plans: list[dict[str, Any]] = []
        payload: list[dict[str, Any]] = []

        for it in items:
            text = it.get("text")
            image = it.get("image")
            if text is None and image is None:
                raise ValueError("每一项至少要给 text 或 image")
            if text is not None and not isinstance(text, str):
                raise ValueError("text 必须是字符串")

            kind = "text+image" if (text and image) else ("image" if image else "text")
            resolved_image, plan = self._resolve_image(image)
            if plan is not None:
                budget = self.template_overhead + (self._count_text_tokens(text) if text else 0)
                seq_len = plan["vision_tokens"] + budget
                plan["seq_len"] = seq_len
                plan["max_length"] = self.max_length
                if seq_len > self.max_length and not ALLOW_TRUNCATION:
                    raise SequenceBudgetError(plan)
            plans.append({"kind": kind, "plan": plan})

            entry: dict[str, Any] = {}
            if text:
                entry["text"] = text
            if resolved_image is not None:
                entry["image"] = resolved_image
            if it.get("instruction"):
                entry["instruction"] = it["instruction"]
            payload.append(entry)

        with self.lock, self.torch.no_grad():
            out = self.emb.process(payload)
            arr = out.to(self.torch.float32).cpu().numpy()

        return [row.tolist() for row in arr], plans

    def _count_text_tokens(self, text: str | None) -> int:
        if not text:
            return 0
        return len(self.emb.processor.tokenizer(text)["input_ids"])

    def _resolve_image(self, image: Any):
        """把入参规整成官方能吃的形态：本地路径 / http(s) URL / PIL.Image。

        返回 (给官方脚本的值, 预算 plan 或 None)。
        """
        if image is None:
            return None, None

        from PIL import Image

        if isinstance(image, str):
            s = image.strip()
            if s.startswith("data:image/"):
                _, _, b64 = s.partition(",")
                try:
                    raw = base64.b64decode(b64, validate=True)
                except Exception as exc:  # noqa: BLE001
                    raise ValueError(f"data URI 里的 base64 解不开：{exc}") from None
                pil = Image.open(io.BytesIO(raw))
                pil.load()
                return pil, self.plan_image(pil)
            if s.startswith(("http://", "https://")):
                # URL 需要联网下载，这里不做预算自检（下载两次不划算）
                return s, None
            p = Path(s)
            if not p.is_file():
                raise ValueError(f"图片文件不存在：{p}")
            with Image.open(p) as pil:
                plan = self.plan_image(pil)
            return str(p), plan

        if isinstance(image, Image.Image):
            return image, self.plan_image(image)

        raise ValueError("image 只接受 本地路径 / http(s) URL / data URI / PIL.Image")


class SequenceBudgetError(Exception):
    def __init__(self, plan: dict[str, Any]) -> None:
        self.plan = plan
        super().__init__(
            f"这张图会被归到 {plan['resized_w']}x{plan['resized_h']}，占 {plan['vision_tokens']} "
            f"个视觉 token，加模板开销后 seq_len={plan['seq_len']} > max_length={plan['max_length']}。"
            f"继续跑会被静默截断（图像 token 被砍掉，向量变垃圾）。"
            f"请下调 PV_MAX_PIXELS，或上调 PV_MAX_LENGTH。"
        )


# --------------------------------------------------------------------------
# 5. HTTP 层
# --------------------------------------------------------------------------


def build_app(service: EmbedService):
    REQ = Counter("pv_embed_requests_total", "请求数", ["status"])
    ITEMS = Counter("pv_embed_items_total", "编码条目数", ["kind"])
    ERRORS = Counter("pv_embed_errors_total", "错误数", ["reason"])
    INFLIGHT = Gauge("pv_embed_inflight", "正在编码的请求数")
    LATENCY = Histogram(
        "pv_embed_latency_seconds",
        "单次请求耗时",
        buckets=(0.05, 0.1, 0.25, 0.5, 1, 2, 5, 10, 20, 40, 80, 160, 320, 600),
    )
    TOKENS = Histogram(
        "pv_embed_vision_tokens",
        "每张图的视觉 token 数",
        buckets=(4, 64, 256, 576, 1024, 1280, 1800, 2048, 4096, 8192),
    )
    INFO = Gauge("pv_embed_info", "模型身份", ["fingerprint", "device", "max_pixels", "dim"])
    INFO.labels(
        fingerprint=service.fingerprint,
        device=service.device,
        max_pixels=str(service.max_pixels),
        dim=str(service.dim),
    ).set(1)

    app = FastAPI(title="prompt-vault embed sidecar", version="0.1.0")

    @app.get("/health")
    def health():
        return {
            "status": "READY",
            "model_dir": str(service.model_dir),
            "model": "Qwen/Qwen3-VL-Embedding-2B",
            "device": service.device,
            "dim": service.dim,
            "pooling": service.pooling,
            "max_pixels": service.max_pixels,
            "max_length": service.max_length,
            "template_overhead": service.template_overhead,
            "allow_truncation": ALLOW_TRUNCATION,
            "fingerprint": service.fingerprint,
            "load_seconds": round(service.load_seconds, 1),
        }

    @app.get("/metrics")
    def metrics():
        return Response(generate_latest(), media_type=CONTENT_TYPE_LATEST)

    @app.post("/v1/embeddings")
    async def embeddings(request: Request):
        t0 = time.time()
        rid = f"{int(t0 * 1000) % 10**9:09d}"
        INFLIGHT.inc()
        raw_items: list[dict[str, Any]] = []
        try:
            body = await request.json()
        except Exception:  # noqa: BLE001
            INFLIGHT.dec()
            REQ.labels(status="400").inc()
            ERRORS.labels(reason="bad_json").inc()
            return JSONResponse(status_code=400, content={"error": "请求体不是合法 JSON"})

        if not isinstance(body, dict):
            INFLIGHT.dec()
            REQ.labels(status="400").inc()
            ERRORS.labels(reason="bad_json").inc()
            return JSONResponse(status_code=400, content={"error": "请求体必须是 JSON 对象"})

        inp = body.get("input")
        if inp is None:
            INFLIGHT.dec()
            REQ.labels(status="400").inc()
            ERRORS.labels(reason="no_input").inc()
            return JSONResponse(status_code=400, content={"error": "缺少 input 字段"})
        if isinstance(inp, dict):
            inp = [inp]
        if not isinstance(inp, list) or not inp:
            INFLIGHT.dec()
            REQ.labels(status="400").inc()
            ERRORS.labels(reason="bad_input").inc()
            return JSONResponse(
                status_code=400, content={"error": "input 必须是对象或非空数组"}
            )

        for it in inp:
            if not isinstance(it, dict):
                INFLIGHT.dec()
                REQ.labels(status="400").inc()
                ERRORS.labels(reason="bad_input").inc()
                return JSONResponse(
                    status_code=400, content={"error": "input 的每一项必须是对象"}
                )
            raw_items.append(it)

        try:
            vecs, plans = service.embed(raw_items)
        except SequenceBudgetError as exc:
            REQ.labels(status="413").inc()
            ERRORS.labels(reason="sequence_budget").inc()
            log("sequence budget exceeded", request_id=rid, **exc.plan)
            return JSONResponse(
                status_code=413,
                content={"error": str(exc), "plan": exc.plan},
            )
        except ValueError as exc:
            REQ.labels(status="400").inc()
            ERRORS.labels(reason="bad_item").inc()
            return JSONResponse(status_code=400, content={"error": str(exc)})
        except Exception as exc:  # noqa: BLE001
            REQ.labels(status="500").inc()
            ERRORS.labels(reason="internal").inc()
            logger.exception("编码失败 request_id=%s", rid)
            return JSONResponse(
                status_code=500,
                content={"error": f"编码失败：{type(exc).__name__}（详情见服务端日志）"},
            )
        finally:
            # 唯一的 dec 点：上面每个 except 分支都靠它收口，别在分支里再 dec 一次（会把 gauge 减成负数）
            INFLIGHT.dec()

        data = []
        for idx, (vec, meta) in enumerate(zip(vecs, plans)):
            kind = meta["kind"]
            ITEMS.labels(kind=kind).inc()
            plan = meta["plan"]
            if plan:
                TOKENS.observe(plan["vision_tokens"])
            data.append(
                {
                    "object": "embedding",
                    "index": idx,
                    "kind": kind,
                    "embedding": vec,
                    "resized": (
                        f"{plan['resized_w']}x{plan['resized_h']}" if plan else None
                    ),
                    "vision_tokens": plan["vision_tokens"] if plan else None,
                }
            )

        elapsed = time.time() - t0
        LATENCY.observe(elapsed)
        REQ.labels(status="200").inc()
        log(
            "embedded",
            request_id=rid,
            items=len(data),
            kinds=[m["kind"] for m in plans],
            elapsed=round(elapsed, 3),
        )
        return {
            "object": "list",
            "model": "Qwen/Qwen3-VL-Embedding-2B",
            "dim": service.dim,
            "fingerprint": service.fingerprint,
            "data": data,
            "usage": {"prompt_tokens": 0, "total_tokens": 0},
            "elapsed_seconds": round(elapsed, 3),
        }

    return app


# --------------------------------------------------------------------------
# 6. 自检 / 入口
# --------------------------------------------------------------------------


def parse_args(argv: list[str] | None = None):
    import argparse

    p = argparse.ArgumentParser(
        prog="pv-embed-server",
        description=(
            "prompt-vault 编码侧车：本地加载官方 Qwen3VLEmbedder 并对外提供 HTTP。"
            "本程序不会联网下载模型，模型必须自己准备好放在本地。"
        ),
    )
    p.add_argument("--selftest", action="store_true", help="加载模型编码一轮后退出")
    p.add_argument(
        "--model-dir",
        default="",
        metavar="PATH",
        help="模型目录，优先级最高；等价于设环境变量 PV_MODEL_DIR",
    )
    p.add_argument(
        "--print-model-dir",
        action="store_true",
        help="只打印解析到的模型目录后退出，不加载模型（排查路径问题用）",
    )
    p.add_argument("--host", default="", help="监听地址，覆盖 PV_HOST")
    p.add_argument("--port", type=int, default=0, help="端口，覆盖 PV_PORT")
    return p.parse_args(argv)


def selftest() -> int:
    """加载模型，编码文本与图片各两次，检查维度 / 归一化 / 确定性。不需要外部文件。"""
    from PIL import Image, ImageDraw

    service = EmbedService()

    img = Image.new("RGB", (512, 512), (32, 32, 48))
    draw = ImageDraw.Draw(img)
    draw.ellipse((100, 100, 412, 412), fill=(220, 60, 60))
    draw.text((200, 240), "SELFTEST", fill=(255, 255, 255))

    def run(items):
        return service.embed(items)

    report = []
    ok = True
    for label, items in (("text", [{"text": "一只在窗台上睡觉的橘猫"}]), ("image", [{"image": img}])):
        vecs_a, plans_a = run(items)
        vecs_b, _ = run(items)  # 同一批次形状重跑，应当逐位一致
        vec = vecs_a[0]
        norm = sum(x * x for x in vec) ** 0.5
        same = vec == vecs_b[0]
        good = len(vec) == service.dim and 0.99 < norm < 1.01 and same
        ok = ok and good
        report.append(
            {
                "kind": label,
                "dim": len(vec),
                # 官方在 bf16 上做 L2 归一化，模长不会精确等于 1，这是官方行为不是 bug
                "norm": round(norm, 6),
                "plan": plans_a[0]["plan"],
                "repeat_identical": same,
                "ok": good,
                "head": [round(x, 5) for x in vec[:5]],
            }
        )

    print(
        json.dumps(
            {
                "model_dir": str(service.model_dir),
                "device": service.device,
                "dim": service.dim,
                "pooling": service.pooling,
                "max_pixels": service.max_pixels,
                "max_length": service.max_length,
                "template_overhead": service.template_overhead,
                "fingerprint": service.fingerprint,
                "load_seconds": round(service.load_seconds, 1),
                "cases": report,
                "SELFTEST": "PASS" if ok else "FAIL",
            },
            ensure_ascii=False,
            indent=2,
        )
    )
    return 0 if ok else 1


def main() -> int:
    args = parse_args()

    if args.model_dir:
        # 显式指定就必须有效：不合法直接退出，不静默回落到别的目录
        os.environ["PV_MODEL_DIR"] = args.model_dir
        check_model_dir(Path(args.model_dir).expanduser().resolve())

    if args.print_model_dir:
        print(resolve_model_dir())
        return 0

    if args.selftest:
        return selftest()

    service = EmbedService()
    app = build_app(service)

    host = args.host or HOST
    port = args.port or PORT

    import uvicorn

    log("serving", host=host, port=port, workers=1, model_dir=str(service.model_dir))
    uvicorn.run(app, host=host, port=port, workers=1, log_config=None, access_log=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
