//! Tool result content for a model. Port of `toLlmContent` in
//! `protocol/content.ts` of pi-mcp `v1.0.0`.

use serde_json::Value;
use yapi_types::message::{ContentBlock, ImageContent, TextContent};

/// A text block.
pub fn text(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
        text_signature: None,
    })
}

fn image(data: &str, mime_type: &str) -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: data.to_owned(),
        mime_type: mime_type.to_owned(),
    })
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// One MCP content block as text or an image: embedded text resources become
/// text, image resources images, and audio, links and other binary resources a
/// short placeholder.
pub fn block_to_llm(block: &Value) -> ContentBlock {
    match field(block, "type") {
        "text" => text(field(block, "text")),
        "image" => image(field(block, "data"), field(block, "mimeType")),
        "audio" => text(format!("[audio {} omitted]", field(block, "mimeType"))),
        "resource_link" => text(format!("{}: {}", field(block, "name"), field(block, "uri"))),
        "resource" => {
            let resource = &block["resource"];
            if let Some(content) = resource.get("text").and_then(Value::as_str) {
                return text(content);
            }
            let mime_type = resource.get("mimeType").and_then(Value::as_str);
            match mime_type {
                Some(mime) if mime.starts_with("image/") => image(field(resource, "blob"), mime),
                _ => text(format!(
                    "[binary resource {} ({}) omitted]",
                    field(resource, "uri"),
                    mime_type.unwrap_or("unknown type")
                )),
            }
        }
        other => text(format!("[unsupported MCP content {other}]")),
    }
}

/// A tool result's content blocks; without blocks, its `structuredContent` as
/// JSON, since servers should but do not always mirror it as text.
pub fn to_llm_content(result: &Value) -> Vec<ContentBlock> {
    let mut content: Vec<ContentBlock> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(block_to_llm).collect())
        .unwrap_or_default();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
        && let Ok(json) = yapi_types::json::to_string_pretty(structured, "  ")
    {
        content.push(text(json));
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_like_pi() {
        let result = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "audio", "data": "x", "mimeType": "audio/wav"},
            {"type": "resource_link", "uri": "u://x", "name": "x"},
            {"type": "resource", "resource": {"uri": "u://b", "blob": "AA==", "mimeType": "application/zip"}},
            {"type": "resource", "resource": {"uri": "u://p", "blob": "AA==", "mimeType": "image/png"}},
            {"type": "widget"},
        ]});
        assert_eq!(
            to_llm_content(&result),
            vec![
                text("a"),
                text("[audio audio/wav omitted]"),
                text("x: u://x"),
                text("[binary resource u://b (application/zip) omitted]"),
                image("AA==", "image/png"),
                text("[unsupported MCP content widget]"),
            ]
        );
        assert_eq!(
            to_llm_content(&json!({"content": [], "structuredContent": {"sum": 3}})),
            vec![text("{\n  \"sum\": 3\n}")]
        );
    }
}
