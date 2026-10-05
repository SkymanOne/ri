//! String ordering as JavaScript's `localeCompare` gives it for identifiers and
//! file names.

use std::cmp::Ordering;

/// Primary weight of a character in ICU root collation, for ASCII: whitespace,
/// then punctuation and symbols in DUCET order, then digits, then letters
/// without case.
fn primary(c: char) -> (u8, u32) {
    const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if c.is_whitespace() {
        (0, c as u32)
    } else if let Some(rank) = PUNCTUATION.find(c) {
        (1, rank as u32)
    } else if c.is_ascii_digit() {
        (2, c as u32)
    } else if c.is_alphabetic() {
        (3, c.to_lowercase().next().unwrap_or(c) as u32)
    } else {
        (4, c as u32)
    }
}

/// `a.localeCompare(b)` for identifiers: primary weights first, then lowercase
/// before uppercase, then code points.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let primaries = |text: &str| text.chars().map(primary).collect::<Vec<_>>();
    primaries(a)
        .cmp(&primaries(b))
        .then_with(|| {
            let case = |text: &str| text.chars().map(char::is_uppercase).collect::<Vec<_>>();
            case(a).cmp(&case(b))
        })
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_locale_compare() {
        // Orders checked against Node's localeCompare over the whole catalog.
        let mut ids = vec![
            "gpt-4o", "gpt-4.1", "GPT-4", "gpt-4", "gpt-40", "a~b", "a:b", "a/b", "a@b", "a-b",
        ];
        ids.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            ids,
            [
                "a-b", "a:b", "a@b", "a/b", "a~b", "gpt-4", "GPT-4", "gpt-4.1", "gpt-40", "gpt-4o"
            ]
        );
    }
}
