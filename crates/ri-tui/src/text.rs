//! Display width, truncation and character classes shared by the widgets.
//!
//! Ports of the helpers in `packages/tui/src/utils.ts` in pi `v1.0.0` that
//! apply to plain text; styled text is handled by ratatui spans.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Terminal columns occupied by `text`, which has no escape sequences.
pub fn visible_width(text: &str) -> usize {
    text.graphemes(true).map(grapheme_width).sum()
}

/// Columns of one grapheme cluster: tabs count as three, as in pi.
pub fn grapheme_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        return 3;
    }
    let width = grapheme.width();
    // Emoji presentation sequences render two columns wide.
    if grapheme.contains('\u{fe0f}') {
        return width.max(2);
    }
    width
}

/// `text` cut to `max_width` columns with `ellipsis` appended when cut, and
/// padded with spaces to `max_width` when `pad` is set.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = visible_width(text);
    if width <= max_width {
        let mut out = text.to_owned();
        if pad {
            out.extend(std::iter::repeat_n(' ', max_width - width));
        }
        return out;
    }
    let ellipsis_width = visible_width(ellipsis);
    let (ellipsis, ellipsis_width) = if ellipsis_width >= max_width {
        let clipped = take_width(ellipsis, max_width);
        let clipped_width = visible_width(&clipped);
        (clipped, clipped_width)
    } else {
        (ellipsis.to_owned(), ellipsis_width)
    };
    let mut out = take_width(text, max_width - ellipsis_width);
    let kept = visible_width(&out);
    out.push_str(&ellipsis);
    if pad {
        out.extend(std::iter::repeat_n(' ', max_width - kept - ellipsis_width));
    }
    out
}

/// The longest grapheme-aligned prefix of `text` within `width` columns.
pub fn take_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme_width(grapheme);
        if used + next > width {
            break;
        }
        used += next;
        out.push_str(grapheme);
    }
    out
}

/// JavaScript's `\s`.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Whether `text` contains a JavaScript `\s` character.
pub fn has_whitespace(text: &str) -> bool {
    text.chars().any(is_js_whitespace)
}

/// pi's `PUNCTUATION_REGEX` class.
pub fn is_ascii_punctuation(c: char) -> bool {
    "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".contains(c)
}

/// Han, Hiragana, Katakana, Hangul and Bopomofo characters, between which
/// lines may break anywhere.
pub fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF
            | 0x2E80..=0x2FDF
            | 0x2FF0..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x31FF
            | 0x3200..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF61..=0xFFDC
            | 0x16FE0..=0x16FFF
            | 0x1B000..=0x1B16F
            | 0x20000..=0x3134F
    )
}

/// Whether a grapheme contains a CJK character.
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

/// Characters that end an autocomplete token: whitespace and CJK punctuation.
pub fn is_autocomplete_separator(c: char) -> bool {
    is_js_whitespace(c)
        || "，．：；！？（）［］｛｝“”‘’…—".contains(c)
        || matches!(
            u32::from(c),
            0x3001..=0x3003 | 0x3008..=0x3011 | 0x3014..=0x301F | 0x30FB | 0xFE30..=0xFE4F | 0xFF61..=0xFF65
        )
}

/// Length of `text` in UTF-16 code units, which pi reports as a character count.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_and_truncates() {
        assert_eq!(visible_width("abc"), 3);
        assert_eq!(visible_width("日本"), 4);
        assert_eq!(visible_width("a\tb"), 5);
        assert_eq!(
            truncate_to_width("hello world", 8, "...", false),
            "hello..."
        );
        assert_eq!(truncate_to_width("hello", 8, "...", true), "hello   ");
        assert_eq!(truncate_to_width("日本語", 5, "", false), "日本");
        assert_eq!(truncate_to_width("日本語", 5, "", true), "日本 ");
        assert_eq!(truncate_to_width("hello", 2, "...", false), "..");
    }
}
