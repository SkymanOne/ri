//! Output truncation shared by the tools: by lines and by bytes, from the head or
//! the tail.

use serde::Serialize;

/// Lines kept by default.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Bytes kept by default: 50 KB.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Characters kept per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit truncated the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    /// The line limit.
    Lines,
    /// The byte limit.
    Bytes,
}

/// What was kept, as stored in tool result details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Truncation {
    /// The kept text.
    pub content: String,
    /// Whether anything was dropped.
    pub truncated: bool,
    /// The limit that dropped content.
    pub truncated_by: Option<TruncatedBy>,
    /// Lines in the input.
    pub total_lines: usize,
    /// Bytes in the input.
    pub total_bytes: usize,
    /// Lines kept.
    pub output_lines: usize,
    /// Bytes kept.
    pub output_bytes: usize,
    /// The only kept line was cut (tail truncation).
    pub last_line_partial: bool,
    /// The first line alone exceeds the byte limit (head truncation).
    pub first_line_exceeds_limit: bool,
    /// Line limit applied.
    pub max_lines: usize,
    /// Byte limit applied.
    pub max_bytes: usize,
}

fn count_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// What [`truncate_middle`] kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiddleTruncation {
    /// The text, with the middle replaced when truncated.
    pub content: String,
    /// Whether the middle was cut.
    pub truncated: bool,
    /// Bytes of the original.
    pub total_bytes: usize,
    /// Lines of the original.
    pub total_lines: usize,
}

/// pi's `truncateMiddle`: keeps the start and end of `content`, half of
/// `max_bytes` each, with a `…N chars truncated…` marker between, cutting only
/// at character boundaries.
pub fn truncate_middle(content: &str, max_bytes: usize) -> MiddleTruncation {
    let total_lines = count_lines(content).len();
    let total_bytes = content.len();
    if total_bytes <= max_bytes {
        return MiddleTruncation {
            content: content.to_owned(),
            truncated: false,
            total_bytes,
            total_lines,
        };
    }
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = total_bytes - (max_bytes - max_bytes / 2);
    while tail_start < total_bytes && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let removed = content[head_end..tail_start].chars().count();
    MiddleTruncation {
        content: format!(
            "{}…{removed} chars truncated…{}",
            &content[..head_end],
            &content[tail_start..]
        ),
        truncated: true,
        total_bytes,
        total_lines,
    }
}

/// pi's size format: `512B`, `1.5KB`, `2.0MB`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn untruncated(content: &str, lines: usize, max_lines: usize, max_bytes: usize) -> Truncation {
    Truncation {
        content: content.to_owned(),
        truncated: false,
        truncated_by: None,
        total_lines: lines,
        total_bytes: content.len(),
        output_lines: lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keeps whole lines from the start.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = count_lines(content);
    let total_bytes = content.len();
    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, lines.len(), max_lines, max_bytes);
    }
    let base = Truncation {
        content: String::new(),
        truncated: true,
        truncated_by: Some(TruncatedBy::Bytes),
        total_lines: lines.len(),
        total_bytes,
        output_lines: 0,
        output_bytes: 0,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if lines.first().is_some_and(|line| line.len() > max_bytes) {
        return Truncation {
            first_line_exceeds_limit: true,
            ..base
        };
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let size = line.len() + usize::from(index > 0);
        if bytes + size > max_bytes {
            by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        bytes += size;
    }
    if kept.len() >= max_lines && bytes <= max_bytes {
        by = TruncatedBy::Lines;
    }
    let content = kept.join("\n");
    Truncation {
        output_lines: kept.len(),
        output_bytes: content.len(),
        content,
        truncated_by: Some(by),
        ..base
    }
}

/// Keeps whole lines from the end; a single over-long last line keeps its tail.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = count_lines(content);
    let total_bytes = content.len();
    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, lines.len(), max_lines, max_bytes);
    }
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    let mut partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > max_bytes {
            by = TruncatedBy::Bytes;
            if kept.is_empty() {
                let tail = tail_bytes(line, max_bytes);
                bytes = tail.len();
                kept.push(tail.to_owned());
                partial = true;
            }
            break;
        }
        kept.push((*line).to_owned());
        bytes += size;
    }
    if kept.len() >= max_lines && bytes <= max_bytes {
        by = TruncatedBy::Lines;
    }
    kept.reverse();
    let content = kept.join("\n");
    Truncation {
        output_lines: kept.len(),
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(by),
        total_lines: lines.len(),
        total_bytes,
        last_line_partial: partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// The last `max_bytes` bytes of `text`, starting at a character boundary.
pub fn tail_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Cuts a line to `max_chars` UTF-16 units, as pi counts them.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    let units: Vec<u16> = line.encode_utf16().collect();
    if units.len() <= max_chars {
        return (line.to_owned(), false);
    }
    (
        format!(
            "{}... [truncated]",
            String::from_utf16_lossy(&units[..max_chars])
        ),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_whole_lines() {
        let text = "a\nbb\nccc\n";
        let result = truncate_head(text, 2, 100);
        assert_eq!(result.content, "a\nbb");
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 3);
        let result = truncate_head(text, 10, 4);
        assert_eq!(result.content, "a\nbb");
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(truncate_head("toolong\nx", 10, 3).first_line_exceeds_limit);
    }

    #[test]
    fn tail_keeps_the_end() {
        let result = truncate_tail("1\n2\n3\n4", 2, 100);
        assert_eq!(result.content, "3\n4");
        let result = truncate_tail("short\nwayyyyyyyy too long", 10, 8);
        assert_eq!(result.content, "too long");
        assert!(result.last_line_partial);
        assert_eq!(tail_bytes("aé", 1), "");
    }

    #[test]
    fn formats_sizes_like_pi() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(51200), "50.0KB");
        assert_eq!(format_size(1536 * 1024), "1.5MB");
    }
}
