# INTERFACES.md — prompt-vault 接口契约 v1

> **用途**：这是前端（`web/`）与 Rust 主服务（`src/`）之间的**唯一对齐基准**。
> 任何一侧改了接口，先改本文件再改代码。
> 依据：`开发文档.md` §7（API 契约）、§8（数据模型）、§9（双重检测）、§6（Qdrant Edge）。
> 生成于 2026-10-16。
> **2026-11 修订：删除进程内 candle，编码改为第三方 embedding API（§1 重写）。**

---

## 0. 通用约定

| 项 | 约定 |
| --- | --- |
| 监听 | `config.toml` 的 `[server] host/port`（默认 `127.0.0.1:8918`）。编码走 HTTPS 调用第三方服务 |
| 编码 | 请求/响应统一 `application/json; charset=utf-8` |
| 时间 | RFC3339 UTC 字符串，例 `"2026-10-16T12:00:00Z"` |
| 分页 | 请求 `?page=1&page_size=50`（`page_size` 上限 200）→ 响应 `{ "items": [...], "total": 123, "page": 1, "page_size": 50 }` |
| 错误 | HTTP 4xx/5xx + `{ "error": { "code": "SNAKE_CASE", "message": "人话", "detail": {...} } }` |
| 媒体 | `GET /media/{**path}` —— **挂在根路由**，不在 `/api` 下（开发文档 §7.1 明确踩过坑） |
| 前端 | `GET /` 返回打包进二进制的 `index.html`；未知非 `/api`、非 `/media` 路径回落 `index.html`（SPA 路由） |
| 静态资源 | `/assets/*`（前端 JS/CSS）：**磁盘优先**（`[web] dir`，默认 `web/dist`，开发期改完前端不用重编译），磁盘没有就回落到 `rust-embed` 打进二进制的产物；前端路由（无扩展名的路径）回落 `index.html` |
| 缩略图 | `/media/{**path}?w=256` 按需生成，缓存到 `data/thumbs/`；`/media` 支持 `Range`（`<video>` 拖进度条实测 206） |
| 路径 | **存储层（SQLite）不保存绝对路径**：一律存相对「可执行文件目录」的路径，绝对路径在读写边界现场换算。这样整个 `data/` 可以整包搬到别的机器或盘符 |

错误码表（`error.code`）：

| code | HTTP | 含义 |
| --- | --- | --- |
| `BAD_REQUEST` | 400 | 参数缺失或非法 |
| `NOT_FOUND` | 404 | 资产/作业/提示词不存在 |
| `ENCODER_UNREACHABLE` | 503 | 编码不可用：没有模型、模型加载失败，或该接口必须有向量能力（`/api/health.encoder.reachable=false`） |
| `FINGERPRINT_MISMATCH` | 409 | 写入被拒：向量指纹与索引指纹不一致（**拒绝，不是警告**，开发文档 §13 第 4 条） |
| `PAYLOAD_TOO_LARGE` | 413 | 图片超过 `[media] max_bytes` 上限 |
| `ENCODER_ERROR` | 502 | 编码过程失败（模型加载/推理报错） |
| `GOLDEN_FAILED` | 409 | 金标校验不通过，禁止开始导入/编码作业（当前后端恒 `unsupported`，不会走到这里） |
| `SPARSE_UNAVAILABLE` | 501 | 稀疏路不可用（索引未启用稀疏，或编码器不产出 `sparse`）——**显式失败，不假装成功** |
| `CONFLICT` | 409 | 作业状态冲突（如已完成的作业再次 resume） |
| `INTERNAL` | 500 | 内部错误 |

---

## 1. 编码后端（第三方 embedding API）

2026-11 起**不再在进程内加载模型**：编码通过 HTTPS 调用 OpenAI 兼容的 `/v1/embeddings`，
默认接硅基流动 `Qwen/Qwen3-VL-Embedding-8B`（4096 维）。

### 1.1 配置

| 项 | 来源 |
| --- | --- |
| 服务地址 | `[embedding] base_url` |
| 模型 | `[embedding] model` |
| API Key | `[embedding] api_key` |
| 维度 | `[embedding] dimensions`（默认 4096） |
| 超时 | `[embedding] timeout_secs`（默认 60） |
| 指纹 | `http\|<base_url>\|<model>\|dim=4096\|norm=l2\|v1` |

### 1.2 请求形态

- 文本：`input: ["..."]`
- 图片：`input: [{"image": "data:image/png;base64,..."}]`
- 一次只发一种模态；同时有文本和图片时**图片优先**
- **视频不支持**：`data:video/*` 直接返回 `ENCODER_ERROR`

### 1.3 没有服务时的行为

| 场景 | 行为 |
| --- | --- |
| 配了 key 且探测成功 | `/api/health` → `READY`，`encoder.reachable=true` |
| 没配 key | 启动照常（warn）；`DEGRADED`、`reachable=false`；文本检索退化为纯词法；`mode=dense` / 以图搜图返回 `ENCODER_UNREACHABLE` |
| key 错 / 网络不通 | 启动探测 `failed`（**不阻止启动**）；health `DEGRADED`；导入/编码作业被金标闸门拒绝 |
| `[dev] mock = true` | 不联网，sha256 派生伪向量（指纹 `mock\|dim=4096\|v1\|device=cpu`），仅用于链路验证 |

### 1.4 硬规则（开发文档 §10 铁律）

1. 当前编码指纹落库为**索引指纹**（`meta` 表 `index_fingerprint`），第一次写入时钉住。
2. 写入前比对指纹；不一致 → `FINGERPRINT_MISMATCH` **拒绝写入**。
3. 查询时若当前指纹与索引指纹不一致 → 拒绝检索并返回 `FINGERPRINT_MISMATCH`。
4. 编码不可用**不报 503 拦检索**：文本检索退化为词法；只有必须有向量的接口才返回 `ENCODER_UNREACHABLE`。

### 1.5 金标探测

启动时调用一次 API（`probe`），验证连通性与维度：

| 结果 | 条件 |
| --- | --- |
| `passed` | 调用成功且返回维度 = 配置维度 |
| `failed` | 调用失败或维度不符（作业闸门拦截） |
| `unsupported` | `[dev] mock = true` |
| `unchecked` | 未配置服务，或还没发起过调用 |

云端没有本地基线，**不做余弦比对**；`/api/maintenance/golden/verify` 可手动重探。

### 1.6 已知限制（不静默降级）

1. **不产出 sparse**：hybrid 实际是 dense + lexical，`warnings` 里显式说明；`SPARSE_UNAVAILABLE` 只在索引未启用稀疏或显式 `mode=sparse` 时出现。
2. **`resized` / `vision_tokens` 为 `null`**：服务端预处理不可见。
3. **视频不支持**：视频资产需要先抽帧或上传封面，再作为图片导入。
4. **云端可能静默升级模型**：指纹只能钉住 `base_url + model + dim`，检索质量突变时先查服务商。

## 2. 数据模型（SQLite，见 `migrations/0001_init.sql`，单一 schema 文件）

```sql
asset(id INTEGER PK, kind TEXT['image'|'video'|'other'], rel_path TEXT UNIQUE, bytes INTEGER,
      width INTEGER, height INTEGER, duration_ms INTEGER,
      hash TEXT, hash_algo TEXT NOT NULL DEFAULT 'blake3',
      nsfw INTEGER DEFAULT 0,
      positive_prompt TEXT, negative_prompt TEXT, workflow TEXT, added_at TEXT)
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
向量本体在 Qdrant Server（默认 `http://127.0.0.1:6333`，集合名 `pv`），SQLite 只留指纹。

Qdrant payload 约定（`collection` 名 `pv`，可用 `[qdrant] collection` 覆盖）：

| 点 | 说明 |
| --- | --- |
| dense 点 id | `asset.id` 直接用作 point id（图片/视频）；提示词点 id = `prompt.id + 1_000_000_000` |
| 向量名（稠密） | `image`（asset）/ `text`（prompt） |
| 向量名（稀疏，预留） | `sparse_image` / `sparse_text`，`Modifier::Idf`（内置 BM25，开发文档 §6.1） |
| payload | `{"kind":"asset"|"prompt","row_id":1,"asset_id":1,"fingerprint":"…","nsfw":false,"tags":["a","b"],"added_at_ms":1700000000000,"source":""}` |

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
    "model": "Qwen/Qwen3-VL-Embedding-8B",
    "device": "cloud",
    "dim": 4096,
    "model_dir": null,
    "max_pixels": null,
    "max_length": null,
    "loaded": true,
    "fingerprint": "http|https://api.siliconflow.cn/v1|Qwen/Qwen3-VL-Embedding-8B|dim=4096|norm=l2|v1",
    "last_error": null,
    "last_checked_at": "2026-11-05T12:00:00Z"
  },
  "golden": {
    "status": "passed",
    "passed": true,
    "checked_at": "2026-11-05T12:00:00Z",
    "detail": "第三方 embedding 调用可用（云端无本地基线，只验证连通性与维度）",
    "cos_text": null, "cos_image": null, "threshold": 0.999, "device_mismatch": false
  },
  "index": {
    "fingerprint": "http|https://api.siliconflow.cn/v1|Qwen/Qwen3-VL-Embedding-8B|dim=4096|norm=l2|v1",
    "points": 1234, "assets": 1200, "prompts": 34,
    "dense_size": 4096, "sparse_enabled": true, "quantization": "scalar-int8",
    "dir": "http://127.0.0.1:6334#pv"
  },
  "db": { "path": "data/pv.sqlite", "assets": 1200, "pending_encode": 5 }
}
```

- `status`：`READY` | `DEGRADED`（编码不可用、金标失败、指纹不一致）。
  **没有模型不阻止启动**（编码不可用 + 检索退化），但要显式暴露。
- `encoder.reachable`：编码能力是否可用（配置了服务或 `[dev] mock = true`）。
- `encoder.mock`：是否 `[dev] mock = true`（见 §3.1）。
- `encoder.loaded`：权重是否已真正加载（懒加载，首次编码后才 `true`）。
- `encoder` 没有 `mode` / `url` 字段：编码只有进程内一条路径。
- `golden.status`：`passed` | `failed` | `unsupported` | `unchecked`（当前后端恒 `unsupported`）。

### 3.1 mock 开关（开发期）

`[dev] mock = true` 时，主服务**不调用第三方服务**，用内置确定性伪向量
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
{ "key": "tag", "source": "sql", "items": [ { "value": "cat", "count": 120 } ] }
```

### `GET /api/stats/size`

```json
{ "media_bytes": 123456789, "vector_bytes": 1234567, "db_bytes": 456789, "wal_bytes": 32768, "total_bytes": 124569144 }
```

---

## 4. 资产

### `POST /api/assets/upload`

`multipart/form-data`，字段名 `file`。落盘 → 按 `<media>/<hash 前两位>/<hash 前 16 位><后缀>` 归档 → 建资产（未编码）→ 返回 `201` + `AssetDto`。

- 大小上限由 `[media] max_bytes` 控制（默认 256 MB），超限 → `413 PAYLOAD_TOO_LARGE`
- 同一 blake3 hash 已存在 → 直接返回已有资产（不重复落盘）
- kind 判定：扩展名 + 能否读出图片/视频尺寸；判不出的归 `other`
- **PNG 会解析 ComfyUI 元数据**（`tEXt`）：写入 `positive_prompt` / `negative_prompt` / `workflow`，正面提示词另建 `prompt(role='positive')`

### `GET /api/assets`

查询参数：`page`(1) `page_size`(50) `q`(模糊匹配 rel_path 或 prompt 文本) `tag`(可重复)
`kind`(image|video) `nsfw`(true|false|all，默认 all) `encoded`(true|false|all，默认 all)
`sort`(`added_at|-added_at|id|-id|bytes|-bytes`，默认 `-added_at`)

```json
{
  "items": [ {
    "id": 1, "kind": "image", "rel_path": "wallhaven/2026/cat.jpg", "bytes": 123456,
    "width": 1024, "height": 1024, "duration_ms": null,
    "hash": "…", "hash_algo": "blake3",
    "nsfw": false, "added_at": "2026-11-06T12:00:00Z",
    "positive_prompt": "一只在窗台上的橘猫，写实照片",
    "negative_prompt": "模糊，低质量",
    "workflow": "{\"nodes\": […]}",
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

### `POST /api/assets/{id}/encode`

单图手动编码，不经过作业：

```json
{ "mode": "image" }
```

- `image`（默认）：读媒体文件 → 编码 → 写 `asset_vector`；响应 `{ asset_id, mode, encoded, fingerprint, dim }`
- `positive`：取该资产的 `prompt(role='positive')`（没有则用 `positive_prompt` 现场建一条）→ 编码 → 写 `prompt_vector`；
  响应额外带 `prompt_id`
- 非法 `mode` → `400 BAD_REQUEST`；资产没有正面提示词 → `400`；编码不可用 → `ENCODER_*`

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

## 4.5 标签

### `GET /api/tags?page=1&page_size=100&q=`

```json
{ "items": [ { "id": 1, "name": "cat", "asset_count": 12 } ], "total": 3 }
```

按 `asset_count` 降序、`name` 升序；`q` 对标签名做 LIKE（通配符已转义）。

### `DELETE /api/tags/{id}`

→ `200 {"deleted": true}`；不存在 → `404`；仍被资产引用 → `409 CONFLICT`，`detail.asset_count` 给出引用数。

### `POST /api/tags/cleanup`

→ `{"deleted": 7}`，删除 `asset_tag` 里没有任何引用的标签行。

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
  "warnings": ["第三方 embedding 不产出 BM25 稀疏向量：hybrid 实际是 dense + lexical，索引里的 sparse 向量名不参与召回"],
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
| `tag` | **`sql`（2026-10 起固定）** | 改为实时 SQL 统计：Qdrant facet 只读已 optimize 的段，详情页新加的标签落在 delta 段里查不到（原来「加了标签但过滤器不显示」的根因）；口径与 `GET /api/tags` 的 `asset_count` 一致 |
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
| `POST /api/maintenance/encoder/refresh` | 返回当前编码器状态（第三方服务地址、模型、维度、最近一次调用） |
| `POST /api/maintenance/comfy/reparse` | 回填已有 PNG 的 ComfyUI 元数据（幂等；返回 scanned/updated） |
| `POST /api/maintenance/purge` | **强制清空**：body 必须带 `{"confirm":"DELETE"}`；DROP 业务表并重建、重建 Qdrant 集合、清空 media/thumbs；有 running/pending 作业时 `409` |

---

## 6. 作业（批量导入 / 编码，断点续传）

### `POST /api/jobs/import`

```json
{ "root": "inbox", "recursive": true, "extensions": [],
  "default_prompt_role": "caption", "dedupe_by": "blake3" }
```

→ `202` + job 对象（`id` 为 uuid）。**导入前先做金标校验**（§9.4）：不通过 → `GOLDEN_FAILED`。

导入语义（2026-10 起）：

| 项 | 规则 |
| --- | --- |
| `root` | 可省略；省略时用 `config.toml` 的 `[comfyui] output_dir`（没配置才回落 media）。可传绝对或相对路径（相对路径以**可执行文件所在目录**为基准）；目录不存在直接 `400`。**落库时换算成相对路径**，跨盘无法换算则存空并在 `warnings` 提示；响应里的 `root` 是现场拼回的绝对路径 |
| `extensions` | 小写、不带点；**空数组 = 不过滤**，任意类型都可导入（落 `other` 桶） |
| 归档 | 复制到 `<media>/<hash 前两位>/<hash 前 16 位><后缀>`（`import.copy = false` 时改为移动；类型只写进 `asset.kind`） |
| ComfyUI 元数据 | 仅 PNG：解析 `tEXt` 的 `prompt`/`workflow`，写入 `positive_prompt`/`negative_prompt`/`workflow`；positive 另写一条 `prompt(role='positive')` |
| 哈希 | blake3；文件 > 100 MB 时**只取前 100 MB** 计算 |
| 去重 | 同一 hash 已存在 → 跳过，源文件留在原位；同名不同内容 → 加 `<hash8>-` 前缀，不覆盖 |
| 自跳过 | 源文件已在 media 目录（或其子目录）内 → 跳过，避免自己搬自己 |
| 隐藏文件 | `.` 开头、`Thumbs.db`、`desktop.ini` 不导入 |

### `POST /api/jobs/encode`

```json
{ "limit": null, "only_missing": true, "batch_size": 8, "asset_ids": null, "prompt_ids": null }
```

### `DELETE /api/jobs/{id}`

→ `200 {"deleted": true}`；`200` 之外：不存在 → `404`，状态为 `running`/`pending`/`paused` → `409 CONFLICT`（提示先取消）。
删除会级联清掉该作业的 `import_job_error` 行。

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
| 5 | 超过 `[media] max_bytes` 的图返回 413 `PAYLOAD_TOO_LARGE` |
| 6 | `pause` 后 `resume` 不重复编码（`cursor` + `asset_vector.fingerprint` 双重判定） |
| 7 | `DELETE /api/assets/{id}` 后稠密/稀疏两路都不再返回该点 |
| 8 | 重启后 `/api/health` 正常、数据可查、指纹不变 |
| 9 | 按 `[qdrant] url` 连上 Qdrant，集合不存在时自动创建（`image`/`text` + 可选 `sparse_*`），写入后检索可命中 |
| 11/12 | **不在本轮范围**（用户明确：不准备覆盖语料、不运行模型） |

---

## 9. 实测到的实现约束（写给下一个改代码的人）

1. **连不上 Qdrant 就启动失败**（重试 3 次，间隔 1s/2s）。这是刻意的：本地工具宁可启动报错，也不要"起来了但搜不到东西"。
2. **集合配置（维度 / 稀疏 / 量化）建好后不可改**。改配置必须删集合重建；`/api/maintenance/vector/info` 会如实回报当前集合的配置。
3. **`disk_bytes` 在 Server 模式下恒为 0**：磁盘占用由服务端管理，客户端拿不到。`/api/health` 的 `index.dir` 改为 `url#collection`。
4. **`SparseVector::new` 要求 `indices` 唯一**，重复直接报 `Validation error: must be unique`。
5. **`kind` 有两个含义，别混**：Qdrant payload 里的 `kind` 是点类型（`asset`/`prompt`），
   资产表里的 `kind` 是媒体类型（`image`/`video`）。把后者当过滤条件传给 Qdrant 会把结果全滤空（踩过）。
6. **所有向量操作都是 async**：`upsert_*` / `search_*` / `delete_*` / `facet` / `count` / `export_jsonl` / `import_jsonl`
   都要 `.await`；`flush()` / `optimize()` 保留但为空实现（服务端后台自理）。
7. **本机 lightpanda 的 fetch/XHR 不会结算**（微任务正常、网络请求永久 pending），
   所以"浏览器直连后端渲染"无法用它验证；验证方式：`--inject-script` 注入**真实 curl 抓到的后端响应**。
