//! prompt-vault 库入口：把主服务的各层暴露出来，供 bin 与集成测试共用。
//!
//! 分层（开发文档 §3）：`db`（SQLite）→ `vector`（Qdrant Edge）→ `encode`（candle 进程内编码）
//! → `search`（多路召回 + RRF）→ `jobs`（批量作业）→ `api`（Salvo 路由）。

pub mod api;
pub mod db;
pub mod encode;
pub mod error;
pub mod jobs;
pub mod paths;
pub mod search;
pub mod state;
pub mod types;
pub mod vector;
