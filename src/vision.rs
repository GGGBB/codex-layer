//! 逐模型图片处理策略。
//!
//! 移植自 CodexPlusPlus PR #2449 的 `crates/codex-plus-core/src/vision.rs`
//! （commit f9be7cf），只保留「原样发送」与「剥离」两档。
//! 上游的第三档 VLM 分析管线（调视觉模型把图片转成文字描述，3000 余行）属于
//! CodexPlusPlus 的增值功能，依赖它自己的缓存与配置，独立项目不携带。
//!
//! 图片块的识别覆盖 `image_url` 与 `input_image` 双形态：Responses 协议的历史项
//! 用前者，请求侧新输入用后者，两边都要认，否则剥离会空转。

use std::collections::BTreeMap;

use serde_json::Value;

/// 图片处理模式（逐模型）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageHandling {
    /// 图片原样保留，不做任何处理（默认）。
    #[serde(rename = "send-as-is")]
    SendAsIs,
    /// 剥离图片块，替换为占位符。
    #[serde(rename = "strip")]
    Strip,
}

/// 模型名归一：小写 + 去掉供应商前缀（`vendor/model` -> `model`）。
fn normalize_model_key(model: &str) -> String {
    let trimmed = model.trim().to_lowercase();
    match trimmed.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => tail.to_string(),
        _ => trimmed,
    }
}

fn lookup_model_map<'a>(
    map: &'a BTreeMap<String, ImageHandling>,
    model: &str,
) -> Option<&'a ImageHandling> {
    if let Some(value) = map.get(model) {
        return Some(value);
    }
    let slug = normalize_model_key(model);
    if let Some(value) = map.get(&slug) {
        return Some(value);
    }
    map.iter()
        .find(|(key, _)| normalize_model_key(key) == slug)
        .map(|(_, value)| value)
}

/// 解析图片策略表（JSON map），返回该模型的处理模式。
/// 未命中任何一级回退时返回 `SendAsIs`。
pub fn image_handling_mode(model: &str, model_vlm_json: &str) -> ImageHandling {
    if let Ok(map) = serde_json::from_str::<BTreeMap<String, ImageHandling>>(model_vlm_json)
        && let Some(mode) = lookup_model_map(&map, model)
    {
        return *mode;
    }
    ImageHandling::SendAsIs
}

/// 图片块所在的字段名。
///
/// Chat Completions（以及 Responses 的普通消息）落在 `content`；
/// Responses 协议下 tool 输出是 `function_call_output.output[]`，没有 `content` 字段。
/// 两边都要扫到，否则剥离对 tool 返回的图片会整个空转。
fn content_key(msg: &Value) -> Option<&'static str> {
    if msg.get("content").is_some() {
        Some("content")
    } else if msg.get("output").is_some() {
        Some("output")
    } else {
        None
    }
}

/// 纯剥离模式：删除所有消息中的图片块，替换为 "[图片已省略]"。
pub fn strip_images_only(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        let Some(key) = content_key(msg) else {
            continue;
        };
        // Responses 的 function_call_output.output[] 用 output_text，普通 content 用 text。
        let block_type = if key == "output" { "output_text" } else { "text" };
        let Some(content) = msg.get_mut(key) else {
            continue;
        };

        match &content {
            Value::Array(parts) => {
                let mut new_content: Vec<Value> = Vec::new();
                for part in parts.iter() {
                    let is_image = part
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "image_url" || kind == "input_image");
                    if is_image {
                        new_content
                            .push(serde_json::json!({"type": block_type, "text": "[图片已省略]"}));
                    } else {
                        new_content.push(part.clone());
                    }
                }
                *content = Value::Array(new_content);
            }
            // 字符串 content 到这里已经不该再含图片了：图片在转换层就被搬进结构化数组。
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_only_replaces_image_parts_in_content_and_output() {
        let mut messages = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "看这张图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]}),
            json!({"type": "function_call_output", "output": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,BBB"}}
            ]}),
        ];
        strip_images_only(&mut messages);
        let text = serde_json::to_string(&messages).unwrap();
        assert!(!text.contains("base64"));
        assert!(text.contains("[图片已省略]"));
        // output 数组的占位符必须是 output_text，否则 Responses 侧不合法。
        assert!(text.contains("output_text"));
    }

    #[test]
    fn model_lookup_falls_back_to_slug() {
        let table = r#"{"qwen3-vl": "strip"}"#;
        assert_eq!(
            image_handling_mode("vendor/qwen3-vl", table),
            ImageHandling::Strip
        );
        assert_eq!(
            image_handling_mode("gpt-5", table),
            ImageHandling::SendAsIs
        );
        assert_eq!(image_handling_mode("gpt-5", ""), ImageHandling::SendAsIs);
    }
}
