//! Text replacement for `edit`: exact matching with a fuzzy fallback, and the diffs
//! shown for an edit.

use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

/// One requested replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    /// Text to find; must occur exactly once.
    pub old_text: String,
    /// Text to put in its place.
    pub new_text: String,
}

/// The line ending a file mostly uses, judged by its first line break.
pub fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// Converts `\r\n` and lone `\r` to `\n`.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Converts `\n` back to `ending`.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}

/// The form fuzzy matching compares: NFKC, no trailing spaces, ASCII quotes,
/// dashes and spaces.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let normalized: String = text.nfkc().collect();
    normalized
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

struct Match {
    edit_index: usize,
    index: usize,
    length: usize,
    new_text: String,
}

/// Finds `old` in `content`, exactly or after fuzzy normalization of both. Returns
/// the offset and length in the searched text and whether fuzzy matching was used.
fn fuzzy_find(content: &str, old: &str) -> Option<(usize, usize, bool)> {
    if let Some(index) = content.find(old) {
        return Some((index, old.len(), false));
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old);
    fuzzy_content
        .find(&fuzzy_old)
        .map(|index| (index, fuzzy_old.len(), true))
}

fn count_occurrences(content: &str, old: &str) -> usize {
    let fuzzy_old = normalize_for_fuzzy_match(old);
    if fuzzy_old.is_empty() {
        return 0;
    }
    normalize_for_fuzzy_match(content)
        .matches(&fuzzy_old)
        .count()
}

/// Applies `edits` to LF-normalized `content`. Every `oldText` is matched against
/// the original, must be unique and must not overlap another. When fuzzy matching
/// was needed, lines outside the edited ones keep their original text.
pub fn apply_edits(content: &str, edits: &[Replacement], path: &str) -> Result<String, String> {
    let total = edits.len();
    let edits: Vec<Replacement> = edits
        .iter()
        .map(|edit| Replacement {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if total == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{index}].oldText must not be empty in {path}.")
            });
        }
    }
    let used_fuzzy = edits
        .iter()
        .any(|edit| fuzzy_find(content, &edit.old_text).is_some_and(|(_, _, fuzzy)| fuzzy));
    let base = if used_fuzzy {
        normalize_for_fuzzy_match(content)
    } else {
        content.to_owned()
    };

    let mut matches = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let Some((at, length, _)) = fuzzy_find(&base, &edit.old_text) else {
            return Err(if total == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            });
        };
        let occurrences = count_occurrences(&base, &edit.old_text);
        if occurrences > 1 {
            return Err(if total == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            });
        }
        matches.push(Match {
            edit_index: index,
            index: at,
            length,
            new_text: edit.new_text.clone(),
        });
    }
    matches.sort_by_key(|m| m.index);
    for pair in matches.windows(2) {
        if pair[0].index + pair[0].length > pair[1].index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }

    let new_content = if used_fuzzy {
        replace_preserving_lines(content, &base, &matches)?
    } else {
        replace(&base, &matches, 0)
    };
    if new_content == content {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    Ok(new_content)
}

fn replace(content: &str, matches: &[Match], offset: usize) -> String {
    let mut result = content.to_owned();
    for m in matches.iter().rev() {
        let start = m.index - offset;
        result.replace_range(start..start + m.length, &m.new_text);
    }
    result
}

/// Applies replacements found in the normalized `base` to `original`, copying
/// unchanged lines from the original.
fn replace_preserving_lines(
    original: &str,
    base: &str,
    matches: &[Match],
) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in base.split_inclusive('\n') {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    if original_lines.len() != spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }
    let outside = || "Replacement range is outside the base content.".to_owned();
    // Groups of matches by the line range they touch.
    let mut groups: Vec<(usize, usize, Vec<&Match>)> = Vec::new();
    for m in matches {
        let start_line = spans
            .iter()
            .position(|(start, end)| m.index >= *start && m.index < *end)
            .ok_or_else(outside)?;
        let mut end_line = start_line;
        while end_line < spans.len() && spans[end_line].1 < m.index + m.length {
            end_line += 1;
        }
        if end_line >= spans.len() {
            return Err(outside());
        }
        let end_line = end_line + 1;
        match groups.last_mut() {
            Some(group) if start_line < group.1 => {
                group.1 = group.1.max(end_line);
                group.2.push(m);
            }
            _ => groups.push((start_line, end_line, vec![m])),
        }
    }
    let mut result = String::new();
    let mut line = 0;
    for (start, end, group) in groups {
        result.push_str(&original_lines[line..start].concat());
        let from = spans[start].0;
        let to = spans[end - 1].1;
        let owned: Vec<Match> = group
            .iter()
            .map(|m| Match {
                edit_index: m.edit_index,
                index: m.index,
                length: m.length,
                new_text: m.new_text.clone(),
            })
            .collect();
        result += &replace(&base[from..to], &owned, from);
        line = end;
    }
    result.push_str(&original_lines[line..].concat());
    Ok(result)
}

/// A unified patch with file headers only and 4 lines of context.
pub fn unified_patch(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = diff
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string();
    if output.is_empty() {
        output = format!("--- {path}\n+++ {path}\n");
    }
    output
}

/// pi's numbered diff for the UI, and the first changed line of the new text.
pub fn display_diff(old: &str, new: &str) -> (String, Option<usize>) {
    const CONTEXT: usize = 4;
    let diff = TextDiff::from_lines(old, new);
    // Group consecutive changes of one kind, as jsdiff's diffLines parts.
    let mut parts: Vec<(ChangeTag, Vec<String>)> = Vec::new();
    for change in diff.iter_all_changes() {
        let line = change.value().trim_end_matches('\n').to_owned();
        match parts.last_mut() {
            Some((tag, lines)) if *tag == change.tag() => lines.push(line),
            _ => parts.push((change.tag(), vec![line])),
        }
    }
    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let pad = |n: usize| format!("{n:>width$}");
    let blank = " ".repeat(width);
    let mut output = Vec::new();
    let (mut old_line, mut new_line) = (1usize, 1usize);
    let mut last_was_change = false;
    let mut first_changed = None;
    for (index, (tag, lines)) in parts.iter().enumerate() {
        match tag {
            ChangeTag::Insert | ChangeTag::Delete => {
                first_changed.get_or_insert(new_line);
                for line in lines {
                    if *tag == ChangeTag::Insert {
                        output.push(format!("+{} {line}", pad(new_line)));
                        new_line += 1;
                    } else {
                        output.push(format!("-{} {line}", pad(old_line)));
                        old_line += 1;
                    }
                }
                last_was_change = true;
            }
            ChangeTag::Equal => {
                let next_is_change = parts
                    .get(index + 1)
                    .is_some_and(|(next, _)| *next != ChangeTag::Equal);
                let show = |lines: &[String],
                            output: &mut Vec<String>,
                            old: &mut usize,
                            new: &mut usize| {
                    for line in lines {
                        output.push(format!(" {} {line}", pad(*old)));
                        *old += 1;
                        *new += 1;
                    }
                };
                if last_was_change && next_is_change {
                    if lines.len() <= CONTEXT * 2 {
                        show(lines, &mut output, &mut old_line, &mut new_line);
                    } else {
                        show(&lines[..CONTEXT], &mut output, &mut old_line, &mut new_line);
                        output.push(format!(" {blank} ..."));
                        let skipped = lines.len() - CONTEXT * 2;
                        old_line += skipped;
                        new_line += skipped;
                        show(
                            &lines[lines.len() - CONTEXT..],
                            &mut output,
                            &mut old_line,
                            &mut new_line,
                        );
                    }
                } else if last_was_change {
                    let shown = lines.len().min(CONTEXT);
                    show(&lines[..shown], &mut output, &mut old_line, &mut new_line);
                    let skipped = lines.len() - shown;
                    if skipped > 0 {
                        output.push(format!(" {blank} ..."));
                        old_line += skipped;
                        new_line += skipped;
                    }
                } else if next_is_change {
                    let skipped = lines.len().saturating_sub(CONTEXT);
                    if skipped > 0 {
                        output.push(format!(" {blank} ..."));
                        old_line += skipped;
                        new_line += skipped;
                    }
                    show(&lines[skipped..], &mut output, &mut old_line, &mut new_line);
                } else {
                    old_line += lines.len();
                    new_line += lines.len();
                }
                last_was_change = false;
            }
        }
    }
    (output.join("\n"), first_changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Replacement {
        Replacement {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn replaces_exact_and_fuzzy_matches() {
        assert_eq!(
            apply_edits("a\nb\nc\n", &[edit("b", "B"), edit("c", "C")], "f").unwrap(),
            "a\nB\nC\n"
        );
        // Curly quotes and trailing spaces in the file still match; other lines
        // keep their original text.
        let content = "say \u{201C}hi\u{201D}  \nkeep\u{2019}s\n";
        assert_eq!(
            apply_edits(content, &[edit("say \"hi\"", "say \"bye\"")], "f").unwrap(),
            "say \"bye\"\nkeep\u{2019}s\n"
        );
    }

    #[test]
    fn reports_pi_errors() {
        assert_eq!(
            apply_edits("aa", &[edit("a", "b")], "f").unwrap_err(),
            "Found 2 occurrences of the text in f. The text must be unique. Please provide more context to make it unique."
        );
        assert!(
            apply_edits("abc", &[edit("ab", "x"), edit("bc", "y")], "f")
                .unwrap_err()
                .contains("overlap")
        );
        assert!(
            apply_edits("abc", &[edit("zz", "x")], "f")
                .unwrap_err()
                .starts_with("Could not find the exact text in f.")
        );
    }

    #[test]
    fn diffs_with_line_numbers() {
        let (diff, first) = display_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(diff, " 1 a\n-2 b\n+2 B\n 3 c");
        assert_eq!(first, Some(2));
    }
}
