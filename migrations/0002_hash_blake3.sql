-- 0002：哈希从 sha256 换成 blake3；kind 允许 'other'；清库重导（需求 4、5）。
--
-- 为什么必须重建表：SQLite 不支持直接修改 CHECK 约束，也不支持把一列改名同时又改约束，
-- 只能「新表 → INSERT SELECT → DROP 旧表 → RENAME」。
--
-- 数据策略：用户明确要求**清空旧数据重导**（旧数据的 sha256 与新 blake3 不能混在一张表里，
-- 否则「同 hash 即同一文件」的判定会失效）。所以先清空依赖 asset 的子表，再清 asset。
--
-- 幂等性由 Rust 侧保证（`src/db/mod.rs::apply_migrations` 用「asset 是否有 hash 列」判断
-- 这份脚本是否已应用），**不能**重复执行——重复执行会把已经导入的资产再清一遍。

-- 子表在前：即使 PRAGMA foreign_keys 被关掉，也不会留下指向不存在资产的孤儿行。
DELETE FROM asset_vector;
DELETE FROM prompt_vector;
DELETE FROM prompt;
DELETE FROM asset_tag;
DELETE FROM asset;

CREATE TABLE asset_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL CHECK (kind IN ('image', 'video', 'other')),
    rel_path    TEXT    NOT NULL UNIQUE,
    bytes       INTEGER NOT NULL DEFAULT 0,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    -- blake3 摘要（十六进制，64 字符）；>100MB 的文件只对前 100MB 求值
    hash        TEXT,
    -- 哈希算法标识：以后换算法时靠它区分，不靠猜
    hash_algo   TEXT    NOT NULL DEFAULT 'blake3' CHECK (hash_algo IN ('blake3')),
    nsfw        INTEGER NOT NULL DEFAULT 0,
    added_at    TEXT    NOT NULL
);

-- 数据已在上面的 DELETE 里清空；这条 INSERT ... SELECT 保留，是为了让
-- 「0001 建表 → 0002 迁移」与「旧库迁移」走完全相同的代码路径（此处必然 0 行）。
INSERT INTO asset_new (id, kind, rel_path, bytes, width, height, duration_ms, hash, hash_algo, nsfw, added_at)
SELECT id, kind, rel_path, bytes, width, height, duration_ms, sha256, 'blake3', nsfw, added_at
  FROM asset;

DROP TABLE asset;
ALTER TABLE asset_new RENAME TO asset;

-- 索引随旧表一起被 DROP 掉了，重建（sha256 索引换成 hash 索引）
CREATE INDEX IF NOT EXISTS idx_asset_added_at ON asset (added_at DESC);
CREATE INDEX IF NOT EXISTS idx_asset_kind     ON asset (kind);
CREATE INDEX IF NOT EXISTS idx_asset_nsfw     ON asset (nsfw);
CREATE INDEX IF NOT EXISTS idx_asset_hash     ON asset (hash);
