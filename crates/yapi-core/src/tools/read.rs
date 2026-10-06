//! `read`: text files with offset and limit, images as attachments.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_types::event::ToolResult;
use yapi_types::message::{ContentBlock, ToolDeclaration};

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_head,
};
use super::{ToolEnv, declaration, js_number, node_error, text_result};

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
        let runtime = self.env.runtime();
        let non_vision_note = runtime
            .model
            .as_ref()
            .filter(|model| !model.accepts_images())
            .map(|_| "[Current model does not support images. The image will be omitted from this request.]");

        if let Some(mime_type) = image_mime_type(&bytes) {
            let limits = runtime
                .model
                .as_ref()
                .and_then(|model| model.image_resize().cloned());
            let auto_resize = runtime.auto_resize_images.unwrap_or(true);
            let processed = tokio::task::spawn_blocking(move || {
                crate::images::process(&bytes, mime_type, auto_resize, limits.as_ref())
            })
            .await
            .map_err(|err| err.to_string())?;
            let (mut note, image) = match processed {
                Ok(processed) => {
                    let mut note = format!("Read image file [{}]", processed.image.mime_type);
                    for hint in processed.hints {
                        note += &format!("\n{hint}");
                    }
                    (note, Some(processed.image))
                }
                Err(message) => (format!("Read image file [{mime_type}]\n{message}"), None),
            };
            if let Some(extra) = non_vision_note {
                note += &format!("\n{extra}");
            }
            let mut content = vec![ContentBlock::text(note)];
            content.extend(image.map(ContentBlock::Image));
            return Ok(ToolResult {
                content,
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

/// The MIME type of a supported image, from its first 4100 bytes as pi
/// sniffs them.
pub fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    let bytes = &bytes[..bytes.len().min(4100)];
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
    if bytes.starts_with(b"BM") && is_bmp(bytes) {
        return Some("image/bmp");
    }
    None
}

/// pi's `isBmp`: a plausible BMP header with a known pixel format.
fn is_bmp(bytes: &[u8]) -> bool {
    if bytes.len() < 26 {
        return false;
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| {
        u64::from(u32::from_le_bytes([
            bytes[at],
            bytes[at + 1],
            bytes[at + 2],
            bytes[at + 3],
        ]))
    };
    let (file_size, pixel_offset, header_size) = (u32_at(2), u32_at(10), u32_at(14));
    if (file_size != 0 && file_size < 26)
        || pixel_offset < 14 + header_size
        || (file_size != 0 && pixel_offset >= file_size)
    {
        return false;
    }
    let (planes, bits) = match header_size {
        12 => (u16_at(22), u16_at(24)),
        40..=124 if bytes.len() >= 30 => (u16_at(26), u16_at(28)),
        _ => return false,
    };
    planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits)
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
