//! 配置文件（`config.toml`）加载。
//!
//! 查找顺序（第一个命中为准）：
//! 1. `--config <path>`（显式指定；文件不存在直接报错）
//! 2. `PV_CONFIG` 环境变量（同上）
//! 3. 可执行文件目录下的 `config.toml`
//! 4. 当前工作目录下的 `config.toml`
//!
//! 配置文件是**可选**的：自动查找没找到就返回 `None`，服务照常启动。
//! 但**找到了就严格解析**：TOML 语法错误或字段类型不对会直接让启动失败，
//! 错误信息带文件路径和行号——而不是静默用默认值，让人以为配置生效了。

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub comfyui: ComfyuiSection,
    #[serde(default)]
    pub import: ImportSection,
    #[serde(default)]
    pub embedding: EmbeddingSection,
}

#[derive(Debug, Default, Deserialize)]
pub struct ComfyuiSection {
    /// ComfyUI 的 output 目录。绝对路径原样使用；相对路径按可执行文件目录解析。
    pub output_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ImportSection {
    /// `true`（默认）= 复制源文件进 media，源目录保持完整；
    /// `false` = 移动（导入后源文件消失，和旧行为一致）。
    pub copy: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct EmbeddingSection {
    /// OpenAI 兼容服务地址（含 `/v1`），如 `https://api.siliconflow.cn/v1`
    pub base_url: Option<String>,
    /// 模型名，如 `Qwen/Qwen3-VL-Embedding-8B`
    pub model: Option<String>,
    /// API Key（明文放在本地 config.toml；注意文件权限，别提交进 git）
    pub api_key: Option<String>,
    /// 输出维度（硅基 Qwen3-VL-Embedding-8B 原生 4096，可截断到 2048）
    pub dimensions: Option<usize>,
    /// 单次请求超时（秒），默认 60
    pub timeout_secs: Option<u64>,
}

impl FileConfig {
    /// 规范化后的 ComfyUI output 目录；空串/纯空白视同未配置。
    pub fn comfyui_output_dir(&self) -> Option<PathBuf> {
        self.comfyui
            .output_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    /// 导入时是否复制源文件（默认复制）。
    pub fn import_copy(&self) -> bool {
        self.import.copy.unwrap_or(true)
    }

    pub fn embedding_base_url(&self) -> Option<String> {
        non_empty(self.embedding.base_url.as_deref())
    }

    pub fn embedding_model(&self) -> Option<String> {
        non_empty(self.embedding.model.as_deref())
    }

    pub fn embedding_api_key(&self) -> Option<String> {
        non_empty(self.embedding.api_key.as_deref())
    }

    pub fn embedding_dimensions(&self) -> Option<usize> {
        self.embedding.dimensions.filter(|d| *d > 0)
    }

    pub fn embedding_timeout_secs(&self) -> Option<u64> {
        self.embedding.timeout_secs.filter(|s| *s >= 5)
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 配置文件模板：首次启动时生成，填好再重新运行。
pub const TEMPLATE: &str = r#"# prompt-vault 配置文件
#
# 首次启动自动生成。填写下面的 [embedding] 后重新运行即可。

[embedding]
# OpenAI 兼容的 embedding 服务地址（含 /v1）
base_url = "https://api.siliconflow.cn/v1"
# 模型名
model = "Qwen/Qwen3-VL-Embedding-8B"
# API Key（明文保存在本文件；不要提交进 git）
api_key = ""
# 向量维度（Qwen3-VL-Embedding-8B 原生 4096）
dimensions = 4096
# 单次 HTTP 请求超时（秒）
timeout_secs = 60

[comfyui]
# ComfyUI 的 output 目录：导入作业不填 root 时默认扫这里。
# 绝对路径原样使用；相对路径按可执行文件目录解析。
# output_dir = "D:/ComfyUI/output"

[import]
# true（默认）= 复制源文件进 media，源目录保持完整；
# false = 移动（导入后源文件消失）。
copy = true
"#;

/// 候选位置，按优先级：显式指定 → PV_CONFIG → 可执行文件目录 → 当前目录。
fn candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(p.to_path_buf());
    }
    if let Ok(raw) = std::env::var("PV_CONFIG") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            out.push(PathBuf::from(trimmed));
        }
    }
    if let Some(dir) = crate::paths::exe_dir() {
        out.push(dir.join("config.toml"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("config.toml"));
    }
    out
}

/// 已存在的配置文件路径。
fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
    candidates(explicit).into_iter().find(|p| p.is_file())
}

/// 没有配置文件时，模板应该生成到哪里（第一个候选位置）。
pub fn desired_path(explicit: Option<&Path>) -> PathBuf {
    candidates(explicit)
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// 加载配置。`Ok(None)` = 没有配置文件（由调用方生成模板并退出）。
pub fn load(explicit: Option<&Path>) -> anyhow::Result<Option<FileConfig>> {
    let Some(path) = locate(explicit) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    let cfg: FileConfig = toml::from_str(&text)
        .with_context(|| format!("配置文件解析失败: {}", path.display()))?;
    Ok(Some(cfg))
}

/// 生成配置模板。已存在则不覆盖（避免把用户填好的配置冲掉）。
pub fn create_template(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let path = desired_path(explicit);
    if path.exists() {
        anyhow::bail!("配置文件已存在，不覆盖: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    std::fs::write(&path, TEMPLATE)
        .with_context(|| format!("生成配置文件失败: {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let text = r#"
[comfyui]
output_dir = "D:/ComfyUI/output"

[import]
copy = true
"#;
        let cfg: FileConfig = toml::from_str(text).expect("示例配置应能解析");
        assert_eq!(
            cfg.comfyui_output_dir(),
            Some(PathBuf::from("D:/ComfyUI/output"))
        );
        assert!(cfg.import_copy());
    }

    #[test]
    fn empty_file_uses_defaults() {
        let cfg: FileConfig = toml::from_str("").expect("空配置合法");
        assert_eq!(cfg.comfyui_output_dir(), None);
        assert!(cfg.import_copy(), "默认必须是复制");
    }

    #[test]
    fn blank_output_dir_is_treated_as_missing() {
        let cfg: FileConfig = toml::from_str("[comfyui]\noutput_dir = \"  \"\n").expect("应能解析");
        assert_eq!(cfg.comfyui_output_dir(), None);
    }
}
