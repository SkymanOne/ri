//! jsdiff's `diffWords` (version 8), which pi uses to highlight the changed
//! words of a replaced line: words and punctuation carry their surrounding
//! whitespace, compare without it, and lose duplicated whitespace afterwards.

/// What a part of the diff is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// In both texts.
    Keep,
    /// Only in the old text.
    Removed,
    /// Only in the new text.
    Added,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == '_'
        || matches!(c,
            '\u{AD}'
            | '\u{C0}'..='\u{D6}'
            | '\u{D8}'..='\u{F6}'
            | '\u{F8}'..='\u{2C6}'
            | '\u{2C8}'..='\u{2D7}'
            | '\u{2DE}'..='\u{2FF}'
            | '\u{1E00}'..='\u{1EFF}')
}

/// jsdiff's `tokenizeIncludingWhitespace` matches: runs of word characters,
/// runs of whitespace, and single other characters.
fn parts(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let same: fn(char) -> bool = if is_word_char(c) {
            is_word_char
        } else if c.is_whitespace() {
            char::is_whitespace
        } else {
            out.push(&text[start..start + c.len_utf8()]);
            continue;
        };
        let mut end = start + c.len_utf8();
        while let Some(&(index, next)) = chars.peek() {
            if !same(next) {
                break;
            }
            end = index + next.len_utf8();
            chars.next();
        }
        out.push(&text[start..end]);
    }
    out
}

fn is_space(part: &str) -> bool {
    part.chars().next().is_some_and(char::is_whitespace)
}

/// `WordDiff.tokenize`: whitespace joins the tokens on both sides of it.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut previous: Option<&str> = None;
    for part in parts(text) {
        if is_space(part) {
            match tokens.last_mut() {
                Some(last) if previous.is_some() => last.push_str(part),
                _ => tokens.push(part.to_owned()),
            }
        } else if let Some(space) = previous.filter(|previous| is_space(previous)) {
            match tokens.last_mut() {
                Some(last) if last == space => last.push_str(part),
                _ => tokens.push(format!("{space}{part}")),
            }
        } else {
            tokens.push(part.to_owned());
        }
        previous = Some(part);
    }
    tokens
}

/// `WordDiff.join`: every token but the first loses its leading whitespace.
fn join(tokens: &[String]) -> String {
    tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            if index == 0 {
                token.as_str()
            } else {
                token.trim_start()
            }
        })
        .collect()
}

fn leading_ws(text: &str) -> &str {
    &text[..text.len() - text.trim_start().len()]
}

fn trailing_ws(text: &str) -> &str {
    &text[text.trim_end().len()..]
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a
        .char_indices()
        .zip(b.chars())
        .take_while(|((_, x), y)| x == y)
        .last()
        .map_or(0, |((index, c), _)| index + c.len_utf8());
    &a[..len]
}

fn common_suffix<'a>(a: &'a str, b: &str) -> &'a str {
    let len: usize = a
        .chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(c, _)| c.len_utf8())
        .sum();
    &a[a.len() - len..]
}

/// The longest suffix of `a` that is a prefix of `b`.
fn maximum_overlap<'a>(a: &'a str, b: &str) -> &'a str {
    (0..=a.len())
        .filter(|start| a.is_char_boundary(*start))
        .map(|start| &a[start..])
        .find(|suffix| b.starts_with(suffix))
        .unwrap_or_default()
}

fn replace_prefix(text: &str, old: &str, new: &str) -> String {
    match text.strip_prefix(old) {
        Some(rest) => format!("{new}{rest}"),
        None => text.to_owned(),
    }
}

fn replace_suffix(text: &str, old: &str, new: &str) -> String {
    match text.strip_suffix(old) {
        Some(rest) => format!("{rest}{new}"),
        None => text.to_owned(),
    }
}

/// jsdiff's `dedupeWhitespaceInChangeObjects` for the changes between two
/// kept parts.
fn dedupe(
    start: Option<&mut String>,
    deletion: Option<&mut String>,
    insertion: Option<&mut String>,
    end: Option<&mut String>,
) {
    match (deletion, insertion) {
        (Some(deletion), Some(insertion)) => {
            let (old_prefix, old_suffix) = (
                leading_ws(deletion).to_owned(),
                trailing_ws(deletion).to_owned(),
            );
            let (new_prefix, new_suffix) = (
                leading_ws(insertion).to_owned(),
                trailing_ws(insertion).to_owned(),
            );
            if let Some(start) = start {
                let common = common_prefix(&old_prefix, &new_prefix);
                *start = replace_suffix(start, &new_prefix, common);
                *deletion = deletion[common.len()..].to_owned();
                *insertion = insertion[common.len()..].to_owned();
            }
            if let Some(end) = end {
                let common = common_suffix(&old_suffix, &new_suffix);
                *end = replace_prefix(end, &new_suffix, common);
                deletion.truncate(deletion.len() - common.len());
                insertion.truncate(insertion.len() - common.len());
            }
        }
        (None, Some(insertion)) => {
            if start.is_some() {
                let ws = leading_ws(insertion).len();
                insertion.drain(..ws);
            }
            if let Some(end) = end {
                let ws = leading_ws(end).len();
                end.drain(..ws);
            }
        }
        (Some(deletion), None) => match (start, end) {
            (Some(start), Some(end)) => {
                let new_full = leading_ws(end).to_owned();
                let (del_start, del_end) = (
                    leading_ws(deletion).to_owned(),
                    trailing_ws(deletion).to_owned(),
                );
                let new_start = common_prefix(&new_full, &del_start).to_owned();
                deletion.drain(..new_start.len());
                let rest = &new_full[new_start.len()..];
                let new_end = common_suffix(rest, &del_end).to_owned();
                deletion.truncate(deletion.len() - new_end.len());
                *end = replace_prefix(end, &new_full, &new_end);
                *start = replace_suffix(
                    start,
                    &new_full,
                    &new_full[..new_full.len() - new_end.len()],
                );
            }
            (None, Some(end)) => {
                let overlap = maximum_overlap(trailing_ws(deletion), leading_ws(end)).len();
                deletion.truncate(deletion.len() - overlap);
            }
            (Some(start), None) => {
                let overlap = maximum_overlap(trailing_ws(start), leading_ws(deletion)).len();
                deletion.drain(..overlap);
            }
            (None, None) => {}
        },
        (None, None) => {}
    }
}

/// The parts of the word diff from `old` to `new`, in order.
pub fn diff_words(old: &str, new: &str) -> Vec<(Tag, String)> {
    use similar::{Algorithm, DiffOp, capture_diff_slices};
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let old_keys: Vec<&str> = old_tokens.iter().map(|token| token.trim()).collect();
    let new_keys: Vec<&str> = new_tokens.iter().map(|token| token.trim()).collect();
    let mut parts: Vec<(Tag, String)> = Vec::new();
    let mut push = |tag: Tag, value: String| match parts.last_mut() {
        Some((last, text)) if *last == tag && tag != Tag::Keep => text.push_str(value.trim_start()),
        _ => parts.push((tag, value)),
    };
    for op in capture_diff_slices(Algorithm::Myers, &old_keys, &new_keys) {
        match op {
            // Kept text is the new text's, as in jsdiff.
            DiffOp::Equal { new_index, len, .. } => {
                push(Tag::Keep, join(&new_tokens[new_index..new_index + len]))
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => push(
                Tag::Removed,
                join(&old_tokens[old_index..old_index + old_len]),
            ),
            DiffOp::Insert {
                new_index, new_len, ..
            } => push(
                Tag::Added,
                join(&new_tokens[new_index..new_index + new_len]),
            ),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                push(
                    Tag::Removed,
                    join(&old_tokens[old_index..old_index + old_len]),
                );
                push(
                    Tag::Added,
                    join(&new_tokens[new_index..new_index + new_len]),
                );
            }
        }
    }
    // jsdiff's postProcess, over each run of changes between kept parts.
    let mut index = 0;
    while index < parts.len() {
        if parts[index].0 == Tag::Keep {
            index += 1;
            continue;
        }
        let run_start = index;
        while index < parts.len() && parts[index].0 != Tag::Keep {
            index += 1;
        }
        let (before, rest) = parts.split_at_mut(run_start);
        let (run, after) = rest.split_at_mut(index - run_start);
        let mut deletion = None;
        let mut insertion = None;
        for (tag, text) in run.iter_mut() {
            match tag {
                Tag::Removed => deletion = Some(text),
                Tag::Added => insertion = Some(text),
                Tag::Keep => {}
            }
        }
        dedupe(
            before.last_mut().map(|(_, text)| text),
            deletion,
            insertion,
            after.first_mut().map(|(_, text)| text),
        );
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(parts: &[(Tag, String)]) -> String {
        parts
            .iter()
            .map(|(tag, text)| match tag {
                Tag::Keep => text.clone(),
                Tag::Removed => format!("[-{text}-]"),
                Tag::Added => format!("{{+{text}+}}"),
            })
            .collect()
    }

    #[test]
    fn tokenizes_like_jsdiff() {
        assert_eq!(
            tokenize("value = 'a b'"),
            ["value ", " = ", " '", "a ", " b", "'"]
        );
        assert_eq!(tokenize("  x"), ["  x"]);
    }

    #[test]
    fn matches_jsdiff_whitespace_handling() {
        assert_eq!(
            show(&diff_words(
                "value = 'something long here'",
                "value = 'something much longer here with more words'"
            )),
            "value = 'something [-long-]{+much longer+} here {+with more words+}'"
        );
        assert_eq!(
            show(&diff_words("foo bar baz", "foo baz")),
            "foo [-bar -]baz"
        );
        assert_eq!(
            show(&diff_words("foo bar baz", "foo qux baz")),
            "foo [-bar-]{+qux+} baz"
        );
    }
}
