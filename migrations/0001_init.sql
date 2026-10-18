-- prompt-vault 初始 schema（开发文档 §8 + INTERFACES.md §2）
-- 约定：向量本体不在 SQLite，只在 Qdrant Edge；这里存指纹，与向量同生命周期。

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS asset (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL CHECK (kind IN ('image', 'video')),
    rel_path    TEXT    NOT NULL UNIQUE,
    bytes       INTEGER NOT NULL DEFAULT 0,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    sha256      TEXT,
    nsfw        INTEGER NOT NULL DEFAULT 0,
    added_at    TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_asset_added_at ON asset (added_at DESC);
CREATE INDEX IF NOT EXISTS idx_asset_kind     ON asset (kind);
CREATE INDEX IF NOT EXISTS idx_asset_nsfw     ON asset (nsfw);
-- 注意：这里**不要**再建 sha256 索引。0001 每次启动都会执行（靠 IF NOT EXISTS 幂等），
-- 而 0002 会把 asset 重建成 hash 结构；那时再对不存在的 sha256 列建索引会报
-- `no such column: sha256`，导致所有已迁移的库都起不来。

CREATE TABLE IF NOT EXISTS prompt (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    asset_id   INTEGER NOT NULL REFERENCES asset (id) ON DELETE CASCADE,
    role       TEXT    NOT NULL DEFAULT 'caption',
    text       TEXT    NOT NULL,
    created_at TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_prompt_asset ON prompt (asset_id);
CREATE INDEX IF NOT EXISTS idx_prompt_text  ON prompt (text);

-- 图 / 视频的向量状态：指纹必须与向量同生命周期
CREATE TABLE IF NOT EXISTS asset_vector (
    asset_id      INTEGER PRIMARY KEY REFERENCES asset (id) ON DELETE CASCADE,
    fingerprint   TEXT    NOT NULL,
    dim           INTEGER NOT NULL,
    resized       TEXT,
    vision_tokens INTEGER,
    encoded_at    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS prompt_vector (
    prompt_id   INTEGER PRIMARY KEY REFERENCES prompt (id) ON DELETE CASCADE,
    fingerprint TEXT    NOT NULL,
    dim         INTEGER NOT NULL,
    encoded_at  TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS tag (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT    NOT NULL UNIQUE,
    created_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS asset_tag (
    asset_id INTEGER NOT NULL REFERENCES asset (id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
    PRIMARY KEY (asset_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_asset_tag_tag ON asset_tag (tag_id);

-- 批量作业：状态必须落库（开发文档 §7.2），cursor 是断点
CREATE TABLE IF NOT EXISTS import_job (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('import', 'encode')),
    status      TEXT NOT NULL CHECK (status IN ('pending', 'running', 'paused', 'succeeded', 'failed', 'cancelled')),
    root        TEXT,
    total       INTEGER NOT NULL DEFAULT 0,
    done        INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    skipped     INTEGER NOT NULL DEFAULT 0,
    cursor      TEXT,
    fingerprint TEXT,
    params      TEXT,
    message     TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    finished_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_job_status  ON import_job (status);
CREATE INDEX IF NOT EXISTS idx_job_created ON import_job (created_at DESC);

CREATE TABLE IF NOT EXISTS import_job_error (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id   TEXT    NOT NULL REFERENCES import_job (id) ON DELETE CASCADE,
    rel_path TEXT,
    code     TEXT    NOT NULL,
    message  TEXT    NOT NULL,
    at       TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_job_error_job ON import_job_error (job_id);
