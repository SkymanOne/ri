//! Grapheme and word segmentation with atomic paste markers, and word motion.
//!
//! Ports of `segmentWithMarkers` in `packages/tui/src/components/editor.ts` and
//! of `packages/tui/src/word-navigation.ts` in pi `v1.0.0`. Indices are byte
//! offsets.

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{has_whitespace, is_ascii_punctuation};

/// A segment of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment<'a> {
    /// Byte offset in the segmented text.
    pub index: usize,
    /// The segment.
    pub text: &'a str,
    /// Contains letters, digits or ideographs (word segmentation only).
    pub word_like: bool,
}

/// Grapheme clusters or UAX #29 words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Granularity {
    /// Extended grapheme clusters.
    Grapheme,
    /// Word boundaries.
    Word,
}

/// `[paste #N]`, `[paste #N +L lines]` or `[paste #N C chars]` at the start of
/// `text`: the id and the marker's byte length.
pub(crate) fn parse_paste_marker(text: &str) -> Option<(u32, usize)> {
    let rest = text.strip_prefix("[paste #")?;
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let id: u32 = rest[..digits].parse().ok()?;
    let after = &rest[digits..];
    // An optional ` +L lines` or ` C chars` suffix.
    let suffix = |lead: &str, unit: &str| -> Option<usize> {
        let digits = after.strip_prefix(lead)?;
        let count = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        (count > 0 && digits[count..].starts_with(unit))
            .then(|| lead.len() + count + unit.len() - 1)
    };
    let consumed = if after.starts_with(']') {
        0
    } else {
        suffix(" +", " lines]").or_else(|| suffix(" ", " chars]"))?
    };
    if !after[consumed..].starts_with(']') {
        return None;
    }
    Some((id, "[paste #".len() + digits + consumed + 1))
}

/// Whether a whole segment is a paste marker.
pub(crate) fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10 && parse_paste_marker(segment).is_some_and(|(_, len)| len == segment.len())
}

/// Byte spans of the paste markers in `text`, in order.
pub(crate) fn paste_marker_spans(text: &str) -> Vec<(u32, usize, usize)> {
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("[paste #") {
        let start = from + found;
        match parse_paste_marker(&text[start..]) {
            Some((id, len)) => {
                spans.push((id, start, start + len));
                from = start + len;
            }
            None => from = start + 1,
        }
    }
    spans
}

fn word_like(segment: &str) -> bool {
    segment.chars().any(char::is_alphanumeric)
}

/// Segments `text`, merging paste markers whose id is in `valid_ids` into one
/// segment each.
pub(crate) fn segment<'a>(
    text: &'a str,
    granularity: Granularity,
    valid_ids: &[u32],
) -> Vec<Segment<'a>> {
    let base: Vec<Segment<'a>> = match granularity {
        Granularity::Grapheme => text
            .grapheme_indices(true)
            .map(|(index, text)| Segment {
                index,
                text,
                word_like: false,
            })
            .collect(),
        Granularity::Word => text
            .split_word_bound_indices()
            .map(|(index, text)| Segment {
                index,
                text,
                word_like: word_like(text),
            })
            .collect(),
    };
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return base;
    }
    let markers: Vec<(usize, usize)> = paste_marker_spans(text)
        .into_iter()
        .filter(|(id, _, _)| valid_ids.contains(id))
        .map(|(_, start, end)| (start, end))
        .collect();
    if markers.is_empty() {
        return base;
    }
    let mut result = Vec::with_capacity(base.len());
    let mut marker = 0;
    for seg in base {
        while marker < markers.len() && markers[marker].1 <= seg.index {
            marker += 1;
        }
        match markers.get(marker) {
            Some(&(start, end)) if seg.index >= start && seg.index < end => {
                if seg.index == start {
                    result.push(Segment {
                        index: start,
                        text: &text[start..end],
                        word_like: false,
                    });
                }
            }
            _ => result.push(seg),
        }
    }
    result
}

/// The cursor after moving one word left from `cursor`: trailing whitespace is
/// skipped, then one word, punctuation run or atomic segment.
pub(crate) fn find_word_backward(text: &str, cursor: usize, valid_ids: &[u32]) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut segments = segment(&text[..cursor], Granularity::Word, valid_ids);
    let mut new_cursor = cursor;
    let atomic = |seg: &Segment<'_>| is_paste_marker(seg.text);
    while let Some(last) = segments.last() {
        if atomic(last) || !has_whitespace(last.text) {
            break;
        }
        new_cursor -= last.text.len();
        segments.pop();
    }
    let Some(last) = segments.last() else {
        return new_cursor;
    };
    if atomic(last) {
        new_cursor -= last.text.len();
    } else if last.word_like {
        // Stop at the last ASCII punctuation inside the word, such as the dot in
        // `foo.bar`.
        let inner = last
            .text
            .char_indices()
            .rfind(|(_, c)| is_ascii_punctuation(*c))
            .map_or(0, |(index, c)| index + c.len_utf8());
        new_cursor -= last.text.len() - inner;
    } else {
        while let Some(last) = segments.last() {
            if atomic(last) || last.word_like || has_whitespace(last.text) {
                break;
            }
            new_cursor -= last.text.len();
            segments.pop();
        }
    }
    new_cursor
}

/// The cursor after moving one word right from `cursor`: leading whitespace is
/// skipped, then one word, punctuation run or atomic segment.
pub(crate) fn find_word_forward(text: &str, cursor: usize, valid_ids: &[u32]) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let segments = segment(&text[cursor..], Granularity::Word, valid_ids);
    let mut iter = segments.iter().peekable();
    let mut new_cursor = cursor;
    let atomic = |seg: &Segment<'_>| is_paste_marker(seg.text);
    while let Some(seg) = iter.peek() {
        if atomic(seg) || !has_whitespace(seg.text) {
            break;
        }
        new_cursor += seg.text.len();
        iter.next();
    }
    let Some(first) = iter.peek() else {
        return new_cursor;
    };
    if atomic(first) {
        new_cursor += first.text.len();
    } else if first.word_like {
        new_cursor += first
            .text
            .char_indices()
            .find(|(_, c)| is_ascii_punctuation(*c))
            .map_or(first.text.len(), |(index, _)| index);
    } else {
        while let Some(seg) = iter.peek() {
            if atomic(seg) || seg.word_like || has_whitespace(seg.text) {
                break;
            }
            new_cursor += seg.text.len();
            iter.next();
        }
    }
    new_cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markers() {
        assert_eq!(parse_paste_marker("[paste #1 +12 lines]x"), Some((1, 20)));
        assert_eq!(parse_paste_marker("[paste #23 1500 chars]"), Some((23, 22)));
        assert_eq!(parse_paste_marker("[paste #4]"), Some((4, 10)));
        assert_eq!(parse_paste_marker("[paste #4 +x lines]"), None);
        assert!(is_paste_marker("[paste #1 +12 lines]"));
        assert_eq!(
            paste_marker_spans("a [paste #1 5 chars] b [paste #2]"),
            vec![(1, 2, 20), (2, 23, 33)]
        );
    }

    #[test]
    fn moves_by_words() {
        let text = "hello world.foo  bar";
        assert_eq!(find_word_forward(text, 0, &[]), 5);
        assert_eq!(find_word_forward(text, 5, &[]), 11);
        assert_eq!(find_word_forward(text, 11, &[]), 12);
        assert_eq!(find_word_backward(text, text.len(), &[]), 17);
        assert_eq!(find_word_backward(text, 17, &[]), 12);
        assert_eq!(find_word_backward(text, 12, &[]), 11);
        assert_eq!(find_word_backward(text, 11, &[]), 6);
        assert_eq!(find_word_backward("a ...", 5, &[]), 2);
    }

    #[test]
    fn treats_valid_markers_as_atoms() {
        let text = "x [paste #1 +11 lines] y";
        assert_eq!(find_word_forward(text, 1, &[1]), 22);
        assert_eq!(find_word_backward(text, 22, &[1]), 2);
        assert_eq!(find_word_forward(text, 1, &[]), 3);
        let graphemes = segment(text, Granularity::Grapheme, &[1]);
        assert_eq!(graphemes[2].text, "[paste #1 +11 lines]");
    }
}
