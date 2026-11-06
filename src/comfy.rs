//! ComfyUI 元数据解析（PNG `tEXt` / `zTXt` / `iTXt`）。
//!
//! ComfyUI 的 `SaveImage` 节点会把 API 格式的 `prompt`（节点图）和 UI 格式的
//! `workflow` 写进 PNG 的文本块。这里只做三件事：
//! 1. 读出文本块；
//! 2. 从 `prompt` 里解析出 KSampler 直接/间接引用的正负提示词；
//! 3. 把 `workflow` 原样带回去。
//!
//! 解析失败一律返回 `None`，**绝不让导入失败**——普通图片本来就没有这些块。

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct ComfyMeta {
    pub positive: Option<String>,
    pub negative: Option<String>,
    pub workflow: Option<String>,
}

/// 读取 PNG 里的 ComfyUI 元数据；不是 PNG / 没有文本块 / 解析失败都返回 None。
pub fn read_png_meta(path: &Path) -> Option<ComfyMeta> {
    let data = std::fs::read(path).ok()?;
    let texts = read_text_chunks(&data)?;
    let prompt = texts.get("prompt");
    let workflow = texts.get("workflow");

    if prompt.is_none() && workflow.is_none() {
        return None;
    }

    let (positive, negative) = prompt.map(|s| parse_prompt(s)).unwrap_or((None, None));
    Some(ComfyMeta {
        positive,
        negative,
        workflow: workflow.cloned(),
    })
}

/// 解析 PNG 文本块。返回 key → 原文。
fn read_text_chunks(data: &[u8]) -> Option<HashMap<String, String>> {
    const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if data.len() < 8 || &data[..8] != PNG_SIG {
        return None;
    }
    let mut out = HashMap::new();
    let mut i = 8usize;
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes(data[i..i + 4].try_into().ok()?) as usize;
        let kind = &data[i + 4..i + 8];
        let start = i + 8;
        let end = start.checked_add(len)?;
        if end + 4 > data.len() {
            break; // 截断的文件，放弃
        }
        let chunk = &data[start..end];
        match kind {
            b"tEXt" => {
                if let Some((k, v)) = split_once(chunk) {
                    out.insert(latin1(k), decode_text(v));
                }
            }
            b"zTXt" => {
                if let Some((k, rest)) = split_once(chunk) {
                    if let Some(body) = rest.get(1..) {
                        if let Ok(text) = inflate(body) {
                            out.insert(latin1(k), decode_text(&text));
                        }
                    }
                }
            }
            b"iTXt" => {
                if let Some((k, rest)) = split_once(chunk) {
                    // flags(1) + method(1) + lang\0 + translated\0 + text
                    if rest.len() > 2 {
                        let compressed = rest[0] == 1;
                        let mut body = &rest[2..];
                        if let Some((_lang, r)) = split_once(body) {
                            body = r;
                        }
                        if let Some((_trans, r)) = split_once(body) {
                            body = r;
                        }
                        let text = if compressed {
                            inflate(body).unwrap_or_default()
                        } else {
                            body.to_vec()
                        };
                        out.insert(latin1(k), String::from_utf8_lossy(&text).into_owned());
                    }
                }
            }
            b"IEND" => break,
            _ => {}
        }
        i = end + 4; // 跳过 CRC
    }
    Some(out)
}

fn split_once(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let pos = data.iter().position(|b| *b == 0)?;
    Some((&data[..pos], &data[pos + 1..]))
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, ()> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|_| ())?;
    Ok(out)
}

fn latin1(data: &[u8]) -> String {
    data.iter().map(|b| *b as char).collect()
}

/// 文本块正文：ComfyUI 实际写 UTF-8（PNG 规范的 tEXt 是 Latin-1），
/// 所以先按 UTF-8 解，失败再退回 Latin-1。
fn decode_text(data: &[u8]) -> String {
    std::str::from_utf8(data)
        .map(str::to_string)
        .unwrap_or_else(|_| latin1(data))
}

/// 从 ComfyUI 的 `prompt`（API 格式）里解析正负提示词。
///
/// 优先找 KSampler 的 `positive` / `negative` 引用；引用可能经过
/// `ConditioningZeroOut` 之类的中间节点，所以沿 `conditioning` 链递归。
pub fn parse_prompt(text: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return (None, None);
    };
    let Some(nodes) = value.as_object() else {
        return (None, None);
    };

    for node in nodes.values() {
        if node.get("class_type").and_then(|v| v.as_str()) != Some("KSampler") {
            continue;
        }
        let inputs = node.get("inputs");
        let positive = inputs
            .and_then(|i| i.get("positive"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .and_then(|id| resolve_text(nodes, id, 0));
        let negative = inputs
            .and_then(|i| i.get("negative"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .and_then(|id| resolve_text(nodes, id, 0));
        if positive.is_some() || negative.is_some() {
            return (positive, negative);
        }
    }

    // 没有 KSampler：退化为收集所有 CLIPTextEncode 的文本，第一条当正面。
    let texts: Vec<String> = nodes
        .values()
        .filter(|n| {
            n.get("class_type")
                .and_then(|v| v.as_str())
                .map(|t| t.contains("CLIPTextEncode"))
                .unwrap_or(false)
        })
        .filter_map(|n| n.get("inputs")?.get("text")?.as_str().map(str::to_string))
        .filter(|t| !t.trim().is_empty())
        .collect();
    match texts.split_first() {
        Some((first, _)) => (Some(first.clone()), None),
        None => (None, None),
    }
}

fn resolve_text(nodes: &serde_json::Map<String, Value>, id: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let node = nodes.get(id)?;
    let inputs = node.get("inputs")?;
    if let Some(text) = inputs.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    for key in ["conditioning", "conditioning_1", "conditioning_2"] {
        if let Some(next) = inputs
            .get(key)
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
        {
            if let Some(text) = resolve_text(nodes, next, depth + 1) {
                return Some(text);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ksampler_prompts() {
        let json = r#"{
          "3": {"class_type": "KSampler", "inputs": {"positive": ["6", 0], "negative": ["7", 0]}},
          "6": {"class_type": "CLIPTextEncode", "inputs": {"text": "a red apple"}},
          "7": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry"}}
        }"#;
        let (pos, neg) = parse_prompt(json);
        assert_eq!(pos.as_deref(), Some("a red apple"));
        assert_eq!(neg.as_deref(), Some("blurry"));
    }

    #[test]
    fn resolves_conditioning_zero_out() {
        let json = r#"{
          "3": {"class_type": "KSampler", "inputs": {"positive": ["6", 0], "negative": ["8", 0]}},
          "6": {"class_type": "CLIPTextEncode", "inputs": {"text": "positive text"}},
          "8": {"class_type": "ConditioningZeroOut", "inputs": {"conditioning": ["9", 0]}},
          "9": {"class_type": "CLIPTextEncode", "inputs": {"text": "negative text"}}
        }"#;
        let (pos, neg) = parse_prompt(json);
        assert_eq!(pos.as_deref(), Some("positive text"));
        assert_eq!(neg.as_deref(), Some("negative text"));
    }

    #[test]
    fn non_json_returns_none() {
        assert_eq!(parse_prompt("not json"), (None, None));
    }

    #[test]
    fn decodes_utf8_text_chunk() {
        assert_eq!(decode_text("一只橘猫".as_bytes()), "一只橘猫");
    }
}
