# INTERFACES.md — prompt-vault 接口契约 v1

> **用途**：这是前端（`web/`）与 Rust 主服务（`src/`）之间的**唯一对齐基准**。
> 任何一侧改了接口，先改本文件再改代码。
> 依据：`开发文档.md` §7（API 契约）、§8（数据模型）、§9（双重检测）、§6（Qdrant Edge）。
> 生成于 2026-10-16。
> **2026-10-18 修订：编码改为进程内 candle（§1 重写），侧车契约、`encoder.mode`/`url` 字段已删除。**

---

## 0. 通用约定

| 项 | 约定 |
| --- | --- |
| 监听 | `127.0.0.1:8918`（默认；`--port` 覆盖）。编码在进程内完成（candle），没有独立侧车进程 |
| 编码 | 请求/响应统一 `application/json; charset=utf-8` |
| 时间 | RFC3339 UTC 字符串，例 `"2026-10-16T12:00:00Z"` |
| 分页 | 请求 `?page=1&page_size=50`（`page_size` 上限 200）→ 响应 `{ "items": [...], "total": 123, "page": 1, "page_size": 50 }` |
| 错误 | HTTP 4xx/5xx + `{ "error": { "code": "SNAKE_CASE", "message": "人话", "detail": {...} } }` |
| 媒体 | `GET /media/{**path}` —— **挂在根路由**，不在 `/api` 下（开发文档 §7.1 明确踩过坑） |
| 前端 | `GET /` 返回打包进二进制的 `index.html`；未知非 `/api`、非 `/media` 路径回落 `index.html`（SPA 路由） |
| 静态资源 | `/assets/*`（前端 JS/CSS）：**磁盘优先**（`--web-dir`，默认 `web/dist`，开发期改完前端不用重编译），磁盘没有就回落到 `rust-embed` 打进二进制的产物；前端路由（无扩展名的路径）回落 `index.html` |
| 缩略图 | `/media/{**path}?w=256` 按需生成，缓存到 `data/thumbs/`；`/media` 支持 `Range`（`<video>` 拖进度条实测 206） |

错误码表（`error.code`）：

| code | HTTP | 含义 |
| --- | --- | --- |
| `BAD_REQUEST` | 400 | 参数缺失或非法 |
| `NOT_FOUND` | 404 | 资产/作业/提示词不存在 |
| `ENCODER_UNREACHABLE` | 503 | 编码不可用：没有模型、模型加载失败，或该接口必须有向量能力（`/api/health.encoder.reachable=false`） |
| `FINGERPRINT_MISMATCH` | 409 | 写入被拒：向量指纹与索引指纹不一致（**拒绝，不是警告**，开发文档 §13 第 4 条） |
| `PAYLOAD_TOO_LARGE` | 413 | 图片超过 `--max-media-bytes` 上限 |
| `ENCODER_ERROR` | 502 | 编码过程失败（模型加载/推理报错） |
| `GOLDEN_FAILED` | 409 | 金标校验不通过，禁止开始导入/编码作业（当前后端恒 `unsupported`，不会走到这里） |
| `SPARSE_UNAVAILABLE` | 501 | 稀疏路不可用（索引未启用稀疏，或编码器不产出 `sparse`）——**显式失败，不假装成功** |
| `CONFLICT` | 409 | 作业状态冲突（如已完成的作业再次 resume） |
| `INTERNAL` | 500 | 内部错误 |

---

## 1. 编码后端（进程内 candle）

编码在**主服务进程内**完成：candle + fastembed 7.1.1 加载本地 `Qwen3-VL-Embedding-2B`。
（历史：原先通过 `embed-server/` 的 Python HTTP 侧车编码，该侧车与 `live` 模式已随代码移除。）

### 1.1 模型目录与加载

| 项 | 行为 |
| --- | --- |
| 模型目录解析 | `--model-dir` / `PV_MODEL_DIR`（显式指定必须有效，不合法则视为“没有模型”）→ `<exe目录>/models/qwen3-vl-embedding-2b` → `<cwd>/models/…` → `<cwd>/../models/…` |
| 权重加载 | **懒加载**：第一次编码才 mmap（4.2 GB 不复制），启动只解析目录 |
| 强制离线 | 把模型目录伪装成 HF 缓存（`$HF_HOME/hub/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/local/` + `refs/main`），并设 `HF_HUB_OFFLINE=1`；缓存里除 `preprocessor_config.json` 用**改写副本**（`max_pixels=3145728`）外，其余文件硬链接（跨设备退回复制） |
| HF 缓存位置 | `--hf-cache-dir`（默认 `<data-dir>/hf-cache`），不写进模型目录（模型目录可能是只读挂载） |
| 设备 | `--candle-device auto`（默认：二进制带 cuda 且驱动可用时用 GPU，否则 CPU）/ `cpu` / `cuda:N`；多卡时 auto 选取「算力优先、同算力比显存」 |
| 精度 | `--candle-dtype f32`（默认）/ `f16` / `bf16` |
| 指纹 | `Qwen/Qwen3-VL-Embedding-2B\|candle\|dtype=f32\|dim=2048\|pool=lasttoken\|norm=L2\|max_pixels=3145728\|max_length=8192\|backend=fastembed-7.1\|device=cpu` |

### 1.2 没有模型时的行为

编码不可用**不阻止启动**：

| 场景 | 行为 |
| --- | --- |
| 有模型 | `/api/health` → `READY`，`encoder.reachable=true` |
| 没有模型 | 启动照常（日志 warn）；`/api/health` → `DEGRADED`、`encoder.reachable=false`；**文本检索自动退化为纯词法**（`effective_mode="lexical"`，`warnings` 里说明）；`mode=dense` 与以图搜图返回 `ENCODER_UNREACHABLE` |
| `--mock` | 不加载模型，向量由 sha256 派生（指纹 `mock\|dim=2048\|v1\|device=cpu`），仅用于链路验证 |

#### 图片预处理（编码前）

| 检查 | 条件 | 行为 |
| --- | --- | --- |
| 宽高比 | 长边/短边 > `--max-aspect-ratio`（默认 `8.0`，`0` 表示不检查） | **直接 `400 BAD_REQUEST`**，`detail` 带 `width/height/aspect_ratio/max_aspect_ratio` |
| 像素数 | 宽×高 > `--max-pixels`（默认 `3,145,728`，约 1774²） | 等比缩放至安全区后再编码；提示写进检索响应的 `warnings`，或作业日志 |
| 尺寸在安全区 | 宽×高 ≤ `--max-pixels` | 原样交给 fastembed，不重复解码 |

只读文件头取尺寸：比例不合格的图（往往也是大文件）不需要整体解码就能拒绝。

### 1.3 硬规则（开发文档 §10 铁律 1、§9）

1. 当前编码指纹落库为**索引指纹**（`meta` 表 `index_fingerprint`），第一次写入时钉住。
2. 写入前比对指纹；不一致 → `FINGERPRINT_MISMATCH` **拒绝写入**。
3. 查询时若当前指纹与索引指纹不一致 → 拒绝检索并返回 `FINGERPRINT_MISMATCH`（同一空间才能比）。
4. 编码不可用**不报 503 拦检索**：文本检索退化为词法；只有必须有向量的接口（以图搜图、`mode=dense`）才返回 `ENCODER_UNREACHABLE`。

### 1.4 已知限制（不静默降级）

1. **不支持图文混合输入**：`{text, image}` 同时存在 → `400`，`detail.index` 指向该输入项（fastembed 的 `Qwen3VLEmbedding` 只实现 text-only / image-only；本项目本就是 image / text 两个向量名分开存）。
2. **不回传 `resized` / `vision_tokens`**：这两个诊断字段只由旧侧车提供，candle 版为 `null`。
3. **不产出 sparse**：hybrid 实际是 dense + lexical，`warnings` 里显式说明；`SPARSE_UNAVAILABLE` 只在索引未启用稀疏或显式 `mode=sparse` 时出现。
4. **金标校验为 `unsupported`**：candle 没有可复用的历史基线（与旧实现不同源），不拦作业。

## 2. 数据模型（SQLite，见 `migrations/0001_init.sql`）

```sql
asset(id INTEGER PK, kind TEXT['image'|'video'], rel_path TEXT UNIQUE, bytes INTEGER,
      width INTEGER, height INTEGER, duration_ms INTEGER, sha256 TEXT,
      nsfw INTEGER DEFAULT 0, added_at TEXT)
prompt(id INTEGER PK, asset_id INTEGER → asset ON DELETE CASCADE, role TEXT,
       text TEXT, created_at TEXT)
prompt_vector(prompt_id INTEGER PK → prompt ON DELETE CASCADE, fingerprint TEXT NOT NULL,
              dim INTEGER NOT NULL, encoded_at TEXT NOT NULL)
asset_vector(asset_id INTEGER PK → asset ON DELETE CASCADE, fingerprint TEXT NOT NULL,
             dim INTEGER NOT NULL, resized TEXT, vision_tokens INTEGER, encoded_at TEXT NOT NULL)
tag(id INTEGER PK, name TEXT UNIQUE, created_at TEXT)
asset_tag(asset_id → asset ON DELETE CASCADE, tag_id → tag ON DELETE CASCADE, PRIMARY KEY(asset_id, tag_id))
import_job(id TEXT PK, kind TEXT, status TEXT, root TEXT, total INTEGER, done INTEGER,
           failed INTEGER, skipped INTEGER, cursor TEXT, fingerprint TEXT,
           params TEXT, message TEXT, created_at TEXT, updated_at TEXT, finished_at TEXT)
import_job_error(job_id → import_job ON DELETE CASCADE, rel_path TEXT, code TEXT, message TEXT, at TEXT)
meta(key TEXT PK, value TEXT)   -- index_fingerprint / schema_version / last_golden 等
```

**索引与向量同生命周期**（开发文档 §8）：向量的指纹存在 `asset_vector`/`prompt_vector` 里，
向量本体在 Qdrant Edge，SQLite 只留指纹。

Qdrant Edge payload 约定（`collection` 名 `pv`）：

| 点 | 说明 |
| --- | --- |
| dense 点 id | `asset.id` 直接用作 point id（图片/视频）；提示词点 id = `prompt.id + 1_000_000_000` |
| 向量名（稠密） | `image`（asset）/ `text`（prompt） |
| 向量名（稀疏，预留） | `sparse_image` / `sparse_text`，`Modifier::Idf`（内置 BM25，开发文档 §6.1） |
| payload | `{"kind":"asset"|"prompt","row_id":1,"fingerprint":"…","nsfw":false,"tags":["a","b"],"added_at_ms":1700000000000,"tag_ids":[1,2]}` |

---

## 3. 健康与统计

### `GET /api/health`

```json
{
  "status": "READY",
  "version": "0.1.0",
  "uptime_seconds": 42,
  "encoder": {
    "reachable": true,
    "mock": false,
    "model": "Qwen/Qwen3-VL-Embedding-2B",
    "device": "cpu",
    "dim": 2048,
    "model_dir": "/workspace/prompt-vault/models/qwen3-vl-embedding-2b",
    "max_pixels": 3145728,
    "max_length": 8192,
    "loaded": false,
    "fingerprint": "Qwen/Qwen3-VL-Embedding-2B|candle|dtype=f32|dim=2048|…|device=cpu",
    "last_error": null,
    "last_checked_at": "2026-10-18T12:00:00Z"
  },
  "golden": {
    "status": "unsupported",
    "passed": null,
    "checked_at": "2026-10-18T12:00:00Z",
    "detail": "candle 后端没有可复用的历史基线（与旧实现不同源）；换后端后需全量重编码",
    "cos_text": null, "cos_image": null, "threshold": null, "device_mismatch": false
  },
  "index": {
    "fingerprint": "Qwen/Qwen3-VL-Embedding-2B|candle|…|device=cpu",
    "points": 1234, "assets": 1200, "prompts": 34,
    "dense_size": 2048, "sparse_enabled": false, "quantization": "scalar-int8",
    "dir": "data/qdrant"
  },
  "db": { "path": "data/pv.sqlite", "assets": 1200, "pending_encode": 5 }
}
```

- `status`：`READY` | `DEGRADED`（编码不可用、金标失败、指纹不一致）。
  **没有模型不阻止启动**（编码不可用 + 检索退化），但要显式暴露。
- `encoder.reachable`：编码能力是否可用（有模型或 `--mock`）。
- `encoder.mock`：是否 `--mock` 模式（见 §3.1）。
- `encoder.loaded`：权重是否已真正加载（懒加载，首次编码后才 `true`）。
- `encoder` 没有 `mode` / `url` 字段：编码只有进程内一条路径。
- `golden.status`：`passed` | `failed` | `unsupported` | `unchecked`（当前后端恒 `unsupported`）。

### 3.1 `--mock` 开关（开发期）

`--mock`（或 `PV_MOCK=true`）时，主服务**不加载模型**，用内置确定性伪向量
（sha256 派生、L2 归一化、2048 维、`fingerprint = "mock|dim=2048|v1|device=cpu"`），
`encoder.mock=true`。用于在没有模型的环境验证「导入 → 编码 → 入库 → 检索 → 前端」全链路。
前端页头据此显示 mock 标记。

### `GET /api/stats`

```json
{
  "assets": { "total": 1200, "images": 1180, "videos": 20, "nsfw": 3, "bytes": 123456789 },
  "prompts": { "total": 1500, "encoded": 1400, "pending": 100 },
  "vectors": { "points": 1234, "dense_size": 2048, "sparse_enabled": false, "disk_bytes": 1234567, "quantization": "scalar-int8" },
  "encode_progress": { "total": 1500, "encoded": 1400, "percent": 93.3 },
  "by_tag":     [ { "key": "cat", "count": 120 } ],
  "by_month":   [ { "key": "2026-10", "count": 300 } ],
  "by_source":  [ { "key": "wallhaven", "count": 900 } ],
  "by_kind":    [ { "key": "image", "count": 1180 } ],
  "jobs": { "running": 1, "succeeded": 3, "failed": 0, "cancelled": 0 }
}
```

`by_source` 取 `rel_path` 的第一段目录名（无目录则 `"(root)"`）。
分布优先用 Qdrant `facet()`（开发文档 §7.2）；`by_month`/`by_source` 若 facet 不适用则 SQL 聚合。

### `GET /api/stats/facets?key=tag|kind|nsfw|month|source`

```json
{ "key": "tag", "source": "qdrant-facet", "items": [ { "value": "cat", "count": 120 } ] }
```

### `GET /api/stats/size`

```json
{ "media_bytes": 123456789, "vector_bytes": 1234567, "db_bytes": 456789, "wal_bytes": 32768, "total_bytes": 124569144 }
```

---

## 4. 资产

### `GET /api/assets`

查询参数：`page`(1) `page_size`(50) `q`(模糊匹配 rel_path 或 prompt 文本) `tag`(可重复)
`kind`(image|video) `nsfw`(true|false|all，默认 all) `encoded`(true|false|all，默认 all)
`sort`(`added_at|-added_at|id|-id|bytes|-bytes`，默认 `-added_at`)

```json
{
  "items": [ {
    "id": 1, "kind": "image", "rel_path": "wallhaven/2026/cat.jpg", "bytes": 123456,
    "width": 1024, "height": 1024, "duration_ms": null, "sha256": "…",
    "nsfw": false, "added_at": "2026-10-16T12:00:00Z",
    "tags": ["cat", "sunset"],
    "prompts": [ { "id": 1, "role": "caption", "text": "…", "created_at": "…" } ],
    "vector": { "encoded": true, "fingerprint": "…", "dim": 2048, "resized": "1632x1088", "vision_tokens": 1734, "encoded_at": "…" },
    "media_url": "/media/wallhaven/2026/cat.jpg",
    "thumb_url": "/media/wallhaven/2026/cat.jpg?w=256"
  } ],
  "total": 1200, "page": 1, "page_size": 50
}
```

`thumb_url` 的 `w` 参数由 `/media/*` 侧按需缩放（缓存到 `data/thumbs/`）。

### `GET /api/assets/{id}`

单个资产对象，结构同上（多带 `prompts` 全部角色）。

### `POST /api/assets/{id}/prompts`

```json
{ "text": "一只在窗台上睡觉的橘猫", "role": "caption", "encode": true }
```

→ `201` + 新建的 `prompt` 对象。`encode=true` 时同步编码该提示词文本
（失败返回 `ENCODER_*`，但**提示词已落库**，可用 `POST /api/jobs` 补编码）。

### `POST /api/assets/{id}/meta`

```json
{ "nsfw": true, "tags": ["cat", "warm"], "rel_path": "…", "width": 1024, "height": 1024, "duration_ms": null }
```

字段全部可选，只改给出的。`tags` 是全量替换语义。→ `200` + 资产对象。

### `DELETE /api/assets/{id}`

删 SQLite 行（级联 prompt/tag）+ 删 Qdrant 点（稠密与稀疏**两路**，开发文档 §13 第 7 条）
+ 删媒体文件（`?keep_file=true` 可保留）。→ `{ "deleted": true, "id": 1, "vector_deleted": true, "file_deleted": false }`

### `GET /api/assets/{id}/similar?k=20&mode=dense`

→ 与 `/api/search/image` 相同结构。

### `GET /api/prompts/{id}/similar?k=20`

```json
{ "items": [ { "prompt": { "id": 7, "asset_id": 3, "role": "caption", "text": "…", "created_at": "…" },
               "asset": { "id": 3, "rel_path": "…", "media_url": "/media/…" }, "score": 0.91 } ], "total": 20 }
```

---

## 5. 搜索

### `GET /api/search/text`

参数：`q`(必需) `k`(20) `mode`(`hybrid`|`dense`|`lexical`，默认 `hybrid`；稀疏不可用时 `hybrid` 自动退化为 `dense` 并在 `warnings` 说明)
`tag`(可重复) `nsfw`(false 默认 **过滤掉 nsfw**) `kind` `min_score` `with_vector=false`

```json
{
  "query": "橘猫 窗台",
  "mode": "hybrid",
  "effective_mode": "dense",
  "took_ms": 12,
  "warnings": ["candle 后端不产出 BM25 稀疏向量：hybrid 实际是 dense + lexical，索引里的 sparse 向量名不参与召回"],
  "items": [ {
    "score": 0.87,
    "sources": ["dense"],
    "rank": { "dense": 1, "sparse": null, "lexical": 3, "rrf": 0.0164 },
    "asset": { "id": 1, "rel_path": "…", "media_url": "/media/…", "thumb_url": "…", "width": 1024, "height": 1024, "nsfw": false, "tags": ["cat"] },
    "matched": { "prompt_id": 3, "text": "…", "role": "caption" }
  } ],
  "total": 20
}
```

**两条检索路径**（开发文档 §1）：
- **词法路（零模型成本）**：SQLite 对 `prompt.text` / `rel_path` / `tag.name` 模糊命中 → 元数据 join 到 asset。
- **稠密+稀疏路**：文本编码 → Qdrant 查询 `text`（提示词点）与 `image`（资产点）两个向量名，合并取优。

### `POST /api/search/image`

`multipart/form-data`：`file`（必填）或 `image_base64`；
或 `application/json`：`{ "asset_id": 1 }`（按已有资产找相似）/ `{ "image_base64": "…" }`。

查询参数 `k`(20) `tag` `nsfw` `with_vector=false`

响应与 `/api/search/text` 同构（`query` 为 `"<file:cat.jpg>"` 或 `"<asset_id:1>"`），
并多带 `query_image: { "resized": "1632x1088", "vision_tokens": 1734 }`。

**以图搜图与文本检索结果必须可比**：同一 `k`、同一排序键（默认 `-score`）。

### `GET /api/search/lexical`（调试，纯词法）

参数 `q`(必需) `k`(50)。返回 `items[{asset_id, rel_path, hit_field: "path"|"prompt"|"tag", hit_text, score}]`。

### `GET /api/search/similar/{vector_kind}`（诊断口，实测可用）

`vector_kind` ∈ `dense|sparse`，带 `q`（必需）与 `k`。
稀疏不可用时返回 `501` + `error.code="SPARSE_UNAVAILABLE"`；可用时直接返回稀疏路的
`point_id / score / payload`，便于人工核对 BM25 召回。

### `GET /api/stats/facets?key=...` 的 `source` 语义（实测）

| key | 数据来源 | 说明 |
| --- | --- | --- |
| `tag` | `qdrant-facet`（失败回落 `sql`） | **只统计资产点**（`kind=asset`）；不加这个过滤会把同一张图按 asset + prompt 算两遍（实测标签计数会翻倍） |
| `nsfw` | `qdrant-facet`（失败回落 `sql`） | 同上，只统计资产点 |
| `kind` | `sql` | payload 里的 `kind` 是**点类型**（asset/prompt），页面要的是**媒体类型**（image/video），后者在 SQLite |
| `month` / `source` | `sql` | `source` 取 `rel_path` 第一段目录 |

---

## 5.5 运维口（`/api/maintenance/*`）

| 接口 | 用途 |
| --- | --- |
| `GET /api/maintenance/vector/info` | 索引状态：目录、维度、稀疏开关、**实际生效的量化**、点数、占用、索引指纹、向量名清单 |
| `POST /api/maintenance/vector/optimize` | 手动 `optimize()`（删除是标记删除，没有后台 optimizer） |
| `POST /api/maintenance/vector/export` | 全量导出 JSONL。body `{"path":"…"}` 可指定路径，默认落在 `data/exports/vectors-<时间戳>.jsonl` |
| `POST /api/maintenance/vector/import` | 从 JSONL 重建（搬家/灾备）。body `{"path":"…"}` |
| `POST /api/maintenance/golden/verify` | 跑一次金标校验并落库，返回 `{golden, blocks_jobs}` |
| `POST /api/maintenance/encoder/refresh` | 返回当前编码器状态（进程内 candle，无远端可探） |

---

## 6. 作业（批量导入 / 编码，断点续传）

### `POST /api/jobs/import`

```json
{ "root": "media", "recursive": true, "extensions": ["jpg","jpeg","png","webp","gif","mp4"],
  "default_prompt_role": "caption", "dedupe_by": "sha256" }
```

→ `202` + job 对象（`id` 为 uuid）。**导入前先做金标校验**（§9.4）：不通过 → `GOLDEN_FAILED`。

### `POST /api/jobs/encode`

```json
{ "limit": null, "only_missing": true, "batch_size": 8, "asset_ids": null, "prompt_ids": null }
```

### `GET /api/jobs` / `GET /api/jobs/{id}`

```json
{
  "id": "9f1c…", "kind": "import|encode", "status": "running|paused|succeeded|failed|cancelled",
  "root": "media", "total": 50000, "done": 1200, "failed": 3, "skipped": 40,
  "cursor": "media/wallhaven/2026/xxx.jpg",
  "fingerprint": "Qwen3-VL-Embedding-2B|…",
  "message": "…", "created_at": "…", "updated_at": "…", "finished_at": null,
  "progress": { "percent": 2.4, "eta_seconds": 432000, "rate_per_minute": 6.7 },
  "errors": [ { "rel_path": "…", "code": "PAYLOAD_TOO_LARGE", "message": "…", "at": "…" } ]
}
```

### `POST /api/jobs/{id}/pause` / `resume` / `cancel`

- `pause` → `status=paused`，当前批次做完后停，**`cursor` 落库**（这就是断点）。
- `resume` → 从 `cursor` 之后继续，**不重复编码已完成项**（开发文档 §13 第 6 条：
  以 `asset_vector.fingerprint == 当前指纹` 判定「已完成」，指纹不同则**重新编码**）。
- 重启进程后，`status=running` 的作业在 `/api/jobs` 里显示 `paused` 并带
  `message="进程重启，已暂停，可 resume"`。

### `GET /api/jobs/{id}/errors`

分页返回该作业的错误明细（`import_job_error`）。

---

## 7. 前端页面 ↔ 接口映射（Ant Design v6，四页）

| 页面 | 路由 | 用到的接口 |
| --- | --- | --- |
| **资产库** | `/assets` | `GET /api/assets`、`GET /api/assets/{id}`、`POST /api/assets/{id}/meta`、`POST /api/assets/{id}/prompts`、`DELETE /api/assets/{id}`、`GET /api/assets/{id}/similar` |
| **搜索** | `/search` | `GET /api/search/text`、`POST /api/search/image`、`GET /api/search/lexical`、`GET /api/prompts/{id}/similar` |
| **作业** | `/jobs` | `GET /api/jobs`、`GET /api/jobs/{id}`、`POST /api/jobs/import`、`POST /api/jobs/encode`、`pause/resume/cancel`、`GET /api/jobs/{id}/errors` |
| **统计** | `/stats` | `GET /api/stats`、`GET /api/stats/facets`、`GET /api/stats/size` |
| 全局页头 | 所有页 | `GET /api/health`（状态徽标、指纹、`encoder.mock=true` 时显示 mock 标记） |

前端硬性要求：

1. **Ant Design v6**（`antd@6.6.6`），React 19 + TypeScript + Vite。
2. **`package.json` 必须锁 `antd@6.6.6`**，构建产物输出到 `web/dist/`（Rust 侧 `rust-embed` 打包）。
3. 图片一律用 `media_url` / `thumb_url` 直出（**不要**自己拼 `/media` 前缀，前缀由后端给）。
4. `nsfw=true` 的资产默认**模糊占位**，点击才显示。
5. 搜索页两种模式切换：文本输入框 + 拖拽上传图（`AntD Upload`），结果网格支持
   `k`、`mode`、`nsfw` 开关；每条结果显示命中提示词与分数。
6. 作业页显示进度条、`done/total/failed`、速率、ETA、`pause/resume/cancel` 按钮、错误明细抽屉。
7. 统计页用 `@ant-design/charts` 或 AntD 自带组件画分布图（标签/月份/来源）+ 体积卡片。
8. 开发期用 Vite 代理 `/api` 和 `/media` 到 `http://127.0.0.1:8918`（`vite.config.ts`）。

---

## 8. 验收对应（开发文档 §13，本项目范围内）

| §13 | 本项目（Rust 主服务 + 前端）如何证明 |
| --- | --- |
| 1 | `/api/health` 返回 `READY`，带 `encoder.fingerprint` 与 `model_dir` |
| 2 | `golden` 字段展示 `unsupported` 并说明「当前后端没有历史基线」 |
| 3 | 同一张图连续编码两次，余弦 1.000000（模型推理确定性）；缺模型时退化为词法而不报错 |
| 4 | 指纹不一致时 `POST /api/assets/{id}/prompts` 等写入返回 `409 FINGERPRINT_MISMATCH` |
| 5 | 超过 `--max-media-bytes` 的图返回 413 `PAYLOAD_TOO_LARGE` |
| 6 | `pause` 后 `resume` 不重复编码（`cursor` + `asset_vector.fingerprint` 双重判定） |
| 7 | `DELETE /api/assets/{id}` 后稠密/稀疏两路都不再返回该点 |
| 8 | 重启后 `/api/health` 正常、数据可查、指纹不变 |
| 9 | Qdrant Edge PoC 七步（`cargo test --test qdrant_poc -- --nocapture`） |
| 11/12 | **不在本轮范围**（用户明确：不准备覆盖语料、不运行模型） |

---

## 9. 实测到的实现约束（写给下一个改代码的人）

1. **`EdgeShard` 只在 `Drop` 时 flush**。进程被 `kill`/`SIGTERM` 直接终止会**丢向量**（实测：编码 2 个点后强杀，重启后 `points=0`，而 SQLite 里的指纹还在 → 两边不一致）。
   所以：写入路径显式 `flush()`，且 `main.rs` 拦下 `SIGINT/SIGTERM`，先 flush 再退出。
2. **`SparseVector::new` 要求 `indices` 唯一**，重复直接报 `Validation error: must be unique`。
3. **`serde` 构造量化配置**：crate 根没有导出 `ScalarQuantization` 等类型，用
   `{"scalar":{"type":"int8","quantile":0.99}}` 反序列化能得到正确配置（实测启动日志确认）；
   构造失败会**降级为不量化**并在 `/api/maintenance/vector/info` 里如实显示 `quantization: "none"`。
4. **磁盘占用要按实际占块算**：Qdrant 的向量文件是稀疏预分配，`metadata.len()` 会把 4 个点的库算成 **265 MB**，
   实际 `du` 只有 **444 KB**（已改用 `st_blocks`）。
5. **`kind` 有两个含义，别混**：Qdrant payload 里的 `kind` 是点类型（`asset`/`prompt`），
   资产表里的 `kind` 是媒体类型（`image`/`video`）。把后者当过滤条件传给 Qdrant 会把结果全滤空（踩过）。
6. **`qdrant_edge` 会打 `Feature flags not initialized!` 警告**：其 `common::flags` 是私有模块，
   库使用者无法调用 `init_feature_flags`，只能走默认值。属已知限制，不影响功能。
7. **本机 lightpanda 的 fetch/XHR 不会结算**（微任务正常、网络请求永久 pending），
   所以"浏览器直连后端渲染"无法用它验证；验证方式：`--inject-script` 注入**真实 curl 抓到的后端响应**。
