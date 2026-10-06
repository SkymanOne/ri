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

/// `ellipsis` and its width, clipped to `max_width` columns when wider.
pub(crate) fn fit_ellipsis(ellipsis: &str, max_width: usize) -> (String, usize) {
    let width = visible_width(ellipsis);
    if width < max_width {
        return (ellipsis.to_owned(), width);
    }
    let clipped = take_width(ellipsis, max_width);
    let width = visible_width(&clipped);
    (clipped, width)
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
    let (ellipsis, ellipsis_width) = fit_ellipsis(ellipsis, max_width);
    let mut out = take_width(text, max_width - ellipsis_width);
    let kept = visible_width(&out);
    out.push_str(&ellipsis);
    if pad {
        out.extend(std::iter::repeat_n(' ', max_width - kept - ellipsis_width));
    }
    out
}

/// The longest grapheme-aligned prefix of `text` within `width` columns.
pub(crate) fn take_width(text: &str, width: usize) -> String {
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
pub(crate) fn is_js_whitespace(c: char) -> bool {
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
pub(crate) fn has_whitespace(text: &str) -> bool {
    text.chars().any(is_js_whitespace)
}

/// pi's `PUNCTUATION_REGEX` class.
pub(crate) fn is_ascii_punctuation(c: char) -> bool {
    "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".contains(c)
}

/// Han, Hiragana, Katakana, Hangul and Bopomofo characters by
/// Script_Extensions, between which lines may break anywhere: pi's
/// `cjkBreakRegex`. The ranges are the code points that regex matches in
/// Node 22 (Unicode 16.0).
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0xB7
            | 0x2C7
            | 0x2C9..=0x2CB
            | 0x2D9
            | 0x2EA..=0x2EB
            | 0x305
            | 0x323
            | 0x1100..=0x11FF
            | 0x2E80..=0x2E99
            | 0x2E9B..=0x2EF3
            | 0x2F00..=0x2FD5
            | 0x2FF0..=0x2FFF
            | 0x3001..=0x3003
            | 0x3005..=0x3011
            | 0x3013..=0x301F
            | 0x3021..=0x3035
            | 0x3037..=0x303F
            | 0x3041..=0x3096
            | 0x3099..=0x30FF
            | 0x3105..=0x312F
            | 0x3131..=0x318E
            | 0x3190..=0x31E5
            | 0x31EF..=0x321E
            | 0x3220..=0x3247
            | 0x3260..=0x327E
            | 0x3280..=0x32B0
            | 0x32C0..=0x32CB
            | 0x32D0..=0x3370
            | 0x337B..=0x337F
            | 0x33E0..=0x33FE
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA700..=0xA707
            | 0xA960..=0xA97C
            | 0xAC00..=0xD7A3
            | 0xD7B0..=0xD7C6
            | 0xD7CB..=0xD7FB
            | 0xF900..=0xFA6D
            | 0xFA70..=0xFAD9
            | 0xFE45..=0xFE46
            | 0xFF61..=0xFFBE
            | 0xFFC2..=0xFFC7
            | 0xFFCA..=0xFFCF
            | 0xFFD2..=0xFFD7
            | 0xFFDA..=0xFFDC
            | 0x16FE2..=0x16FE3
            | 0x16FF0..=0x16FF1
            | 0x1AFF0..=0x1AFF3
            | 0x1AFF5..=0x1AFFB
            | 0x1AFFD..=0x1AFFE
            | 0x1B000..=0x1B122
            | 0x1B132
            | 0x1B150..=0x1B152
            | 0x1B155
            | 0x1B164..=0x1B167
            | 0x1D360..=0x1D371
            | 0x1F200
            | 0x1F250..=0x1F251
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B739
            | 0x2B740..=0x2B81D
            | 0x2B820..=0x2CEA1
            | 0x2CEB0..=0x2EBE0
            | 0x2EBF0..=0x2EE5D
            | 0x2F800..=0x2FA1D
            | 0x30000..=0x3134A
            | 0x31350..=0x323AF
    )
}

/// Whether a grapheme contains a CJK character.
pub(crate) fn has_cjk(text: &str) -> bool {
    text.chars().any(is_cjk)
}

/// Characters that end an autocomplete token: whitespace and pi's
/// `cjkPunctuationRegex`, CJK characters that are punctuation plus
/// fullwidth marks. The ranges are the code points it matches in Node 22.
pub(crate) fn is_autocomplete_separator(c: char) -> bool {
    is_js_whitespace(c)
        || matches!(
            u32::from(c),
            0xB7
                | 0x2014
                | 0x2018..=0x2019
                | 0x201C..=0x201D
                | 0x2026
                | 0x3001..=0x3003
                | 0x3008..=0x3011
                | 0x3014..=0x301F
                | 0x3030
                | 0x303D
                | 0x30A0
                | 0x30FB
                | 0xFE45..=0xFE46
                | 0xFF01
                | 0xFF08..=0xFF09
                | 0xFF0C
                | 0xFF0E
                | 0xFF1A..=0xFF1B
                | 0xFF1F
                | 0xFF3B
                | 0xFF3D
                | 0xFF5B
                | 0xFF5D
                | 0xFF61..=0xFF65
                | 0x16FE2
        )
}

#[cfg(test)]
mod tests {
    /// pi's regexes, as Node 22 evaluates them on these characters.
    #[test]
    fn classifies_cjk_like_pi() {
        for c in ['·', '漢', 'か', 'カ', '한', 'ㄅ', '、', '〆'] {
            assert!(is_cjk(c), "{c}");
        }
        for c in ['〄', '〒', '〠', '㉈', '㎏', '︰', 'a'] {
            assert!(!is_cjk(c), "{c}");
        }
        for c in ['·', '〰', '〽', '゠', '，', '—', ' '] {
            assert!(is_autocomplete_separator(c), "{c}");
        }
        for c in ['︰', '﹏', '漢', 'a', '/'] {
            assert!(!is_autocomplete_separator(c), "{c}");
        }
    }

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
