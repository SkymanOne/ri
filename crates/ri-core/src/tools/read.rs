//! `read`: text files with offset and limit, images as attachments.

use futures_util::future::BoxFuture;
use ri_agent::{Tool, UpdateSink};
use ri_types::event::ToolResult;
use ri_types::message::{ContentBlock, ImageContent, ToolDeclaration};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_head,
};
use super::{ToolEnv, declaration, node_error, text, text_result};

/// The `read` tool.
pub struct Read {
    env: ToolEnv,
    declaration: ToolDeclaration,
}

impl Read {
    /// A `read` tool for `env`.
    pub fn new(env: ToolEnv) -> Read {
        Read {
            env,
            declaration: declaration(
                "read",
                format!(
                    "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                    DEFAULT_MAX_BYTES / 1024
                ),
                json!({"type":"object","required":["path"],"properties":{
                    "path":{"type":"string","description":"Path to the file to read (relative or absolute)"},
                    "offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},
                    "limit":{"type":"number","description":"Maximum number of lines to read"}}}),
            ),
        }
    }
}

/// Default inline image limit when the model sets none: 4.5 MB of base64.
const DEFAULT_MAX_IMAGE_BYTES: usize = 4_718_592;

impl Tool for Read {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        cancel: CancellationToken,
        _updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let run = self.read(args);
            tokio::select! {
                () = cancel.cancelled() => Err("Operation aborted".to_owned()),
                result = run => result,
            }
        })
    }
}

impl Read {
    async fn read(&self, args: Value) -> Result<ToolResult, String> {
        let path = args["path"].as_str().unwrap_or_default().to_owned();
        let offset = args["offset"].as_f64();
        let limit = args["limit"].as_f64();
        let absolute = super::path::resolve_read_path(&path, &self.env.cwd);
        tokio::fs::metadata(&absolute)
            .await
            .map_err(|err| node_error(&err, "access", &absolute))?;
        let bytes = tokio::fs::read(&absolute)
            .await
            .map_err(|err| node_error(&err, "read", &absolute))?;
        let model = self.env.runtime().model;
        let non_vision_note = model
            .as_ref()
            .filter(|model| !model.accepts_images())
            .map(|_| "[Current model does not support images. The image will be omitted from this request.]");

        if let Some(mime_type) = image_mime_type(&bytes) {
            let max_bytes = model
                .as_ref()
                .and_then(|model| model.input_limits.as_ref())
                .and_then(|limits| limits.images.as_ref())
                .and_then(|images| images.resize.as_ref())
                .and_then(|resize| resize.max_bytes)
                .map_or(DEFAULT_MAX_IMAGE_BYTES, |max| max as usize);
            let data = base64(&bytes);
            if mime_type == "image/bmp" || data.len() > max_bytes {
                let reason = if mime_type == "image/bmp" {
                    "[Image omitted: could not be converted to a supported inline image format.]"
                } else {
                    "[Image omitted: could not be resized below the inline image size limit.]"
                };
                let mut note = format!("Read image file [{mime_type}]\n{reason}");
                if let Some(extra) = non_vision_note {
                    note += &format!("\n{extra}");
                }
                return Ok(text_result(note, None));
            }
            let mut note = format!("Read image file [{mime_type}]");
            if let Some(extra) = non_vision_note {
                note += &format!("\n{extra}");
            }
            return Ok(ToolResult {
                content: vec![
                    text(note),
                    ContentBlock::Image(ImageContent {
                        data,
                        mime_type: mime_type.to_owned(),
                    }),
                ],
                ..ToolResult::default()
            });
        }

        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len();
        let start = match offset {
            Some(offset) if offset != 0.0 => (offset - 1.0).max(0.0) as usize,
            _ => 0,
        };
        let start_display = start + 1;
        if start >= total {
            return Err(format!(
                "Offset {} is beyond end of file ({total} lines total)",
                js_number(offset.unwrap_or(0.0))
            ));
        }
        let (selected, user_limited) = match limit {
            Some(limit) => {
                let end = (start + limit.max(0.0) as usize).min(total);
                (lines[start..end].join("\n"), Some(end - start))
            }
            None => (lines[start..].join("\n"), None),
        };
        let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut details = None;
        let output = if truncation.first_line_exceeds_limit {
            let size = format_size(lines[start].len());
            details = Some(json!({ "truncation": truncation }));
            format!(
                "[Line {start_display} is {size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES)
            )
        } else if truncation.truncated {
            let end_display = start_display + truncation.output_lines - 1;
            let next = end_display + 1;
            let mut output = truncation.content.clone();
            if truncation.truncated_by == Some(TruncatedBy::Lines) {
                output += &format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total}. Use offset={next} to continue.]"
                );
            } else {
                output += &format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total} ({} limit). Use offset={next} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                );
            }
            details = Some(json!({ "truncation": truncation }));
            output
        } else if let Some(count) = user_limited.filter(|count| start + count < total) {
            let remaining = total - (start + count);
            let next = start + count + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]",
                truncation.content
            )
        } else {
            truncation.content
        };
        Ok(text_result(output, details))
    }
}

fn js_number(value: f64) -> String {
    ri_types::json::to_string(&value).unwrap_or_default()
}

/// The MIME type of a supported image, from its leading bytes.
fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        let is_png =
            bytes.len() >= 16 && bytes[8..12] == [0, 0, 0, 13] && &bytes[12..16] == b"IHDR";
        return (is_png && !is_animated_png(bytes)).then_some("image/png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        return Some("image/bmp");
    }
    None
}

fn is_animated_png(bytes: &[u8]) -> bool {
    let mut offset = 8;
    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        match &bytes[offset + 4..offset + 8] {
            b"acTL" => return true,
            b"IDAT" => return false,
            _ => {}
        }
        let next = offset + 12 + length;
        if next <= offset || next > bytes.len() {
            return false;
        }
        offset = next;
    }
    false
}

/// Standard base64 with padding.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
