//! Glob matching with the defaults of minimatch, which pi uses for package
//! resource patterns and model scopes.
//!
//! Supported: brace expansion (`{a,b}`, `{1..3}`), `*` and `?` within a path
//! segment, `[...]` classes with `!` or `^` negation and ranges, `**` across
//! segments, and `\` escapes. As in minimatch, `*`, `?` and `**` do not match
//! a segment's leading `.`, and a leading `./` in the pattern is ignored.
//! Extglobs such as `+(a|b)` are not supported.

/// Whether `text` matches `pattern`.
pub fn matches(pattern: &str, text: &str) -> bool {
    matches_with(pattern, text, false)
}

/// Whether `text` matches `pattern`, ignoring case when `nocase` is set.
pub fn matches_with(pattern: &str, text: &str, nocase: bool) -> bool {
    let fold = |value: &str| {
        if nocase {
            value.to_lowercase()
        } else {
            value.to_owned()
        }
    };
    let text = fold(text);
    let text: Vec<&str> = text.split('/').collect();
    expand_braces(pattern).iter().any(|pattern| {
        let pattern = fold(pattern);
        let mut pattern = pattern.as_str();
        while let Some(rest) = pattern.strip_prefix("./") {
            pattern = rest;
        }
        let segments: Vec<&str> = pattern.split('/').collect();
        match_segments(&segments, &text)
    })
}

fn match_segments(pattern: &[&str], text: &[&str]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            match_segments(&pattern[1..], text)
                || text.first().is_some_and(|segment| {
                    !segment.starts_with('.') && match_segments(pattern, &text[1..])
                })
        }
        (Some(part), Some(segment)) => {
            match_segment(part, segment) && match_segments(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

/// One element of a segment pattern.
#[derive(Debug)]
enum Token {
    Char(char),
    Any,
    One,
    Class {
        negated: bool,
        items: Vec<(char, char)>,
    },
}

fn tokens(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' if index + 1 < chars.len() => {
                out.push(Token::Char(chars[index + 1]));
                index += 2;
            }
            '*' => {
                if !matches!(out.last(), Some(Token::Any)) {
                    out.push(Token::Any);
                }
                index += 1;
            }
            '?' => {
                out.push(Token::One);
                index += 1;
            }
            '[' => match class(&chars, index) {
                Some((token, end)) => {
                    out.push(token);
                    index = end;
                }
                None => {
                    out.push(Token::Char('['));
                    index += 1;
                }
            },
            other => {
                out.push(Token::Char(other));
                index += 1;
            }
        }
    }
    out
}

/// The class opening at `start`, and the index after its `]`.
fn class(chars: &[char], start: usize) -> Option<(Token, usize)> {
    let mut index = start + 1;
    let negated = matches!(chars.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut items = Vec::new();
    let first = index;
    while index < chars.len() {
        let mut low = chars[index];
        if low == ']' && index > first {
            return Some((Token::Class { negated, items }, index + 1));
        }
        if low == '\\' && index + 1 < chars.len() {
            index += 1;
            low = chars[index];
        }
        if chars.get(index + 1) == Some(&'-') && chars.get(index + 2).is_some_and(|c| *c != ']') {
            items.push((low, chars[index + 2]));
            index += 3;
        } else {
            items.push((low, low));
            index += 1;
        }
    }
    None
}

fn match_segment(pattern: &str, text: &str) -> bool {
    let tokens = tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    if text.first() == Some(&'.') && !matches!(tokens.first(), Some(Token::Char('.'))) {
        return false;
    }
    match_tokens(&tokens, &text)
}

fn match_tokens(tokens: &[Token], text: &[char]) -> bool {
    match tokens.first() {
        None => text.is_empty(),
        Some(Token::Any) => (0..=text.len()).any(|skip| match_tokens(&tokens[1..], &text[skip..])),
        Some(token) => {
            let Some(&c) = text.first() else {
                return false;
            };
            let ok = match token {
                Token::Char(expected) => *expected == c,
                Token::One => true,
                Token::Class { negated, items } => {
                    items.iter().any(|(low, high)| (*low..=*high).contains(&c)) != *negated
                }
                Token::Any => unreachable!("handled above"),
            };
            ok && match_tokens(&tokens[1..], &text[1..])
        }
    }
}

/// minimatch's brace expansion: `{a,b}` alternatives and `{1..3}` ranges,
/// nested; a brace with neither stays literal.
fn expand_braces(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut depth = 0usize;
    let mut open = None;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 1,
            '{' => {
                if depth == 0 {
                    open = Some(index);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = open.take()
                {
                    let prefix: String = chars[..start].iter().collect();
                    let body: String = chars[start + 1..index].iter().collect();
                    let suffix: String = chars[index + 1..].iter().collect();
                    let Some(options) = brace_options(&body) else {
                        // Literal braces: expand what follows them.
                        return expand_braces(&suffix)
                            .into_iter()
                            .map(|rest| format!("{prefix}{{{body}}}{rest}"))
                            .collect();
                    };
                    let mut out = Vec::new();
                    for option in options {
                        for expanded in expand_braces(&format!("{prefix}{option}{suffix}")) {
                            out.push(expanded);
                        }
                    }
                    return out;
                }
            }
            _ => {}
        }
        index += 1;
    }
    vec![pattern.to_owned()]
}

/// The alternatives of a brace body, or `None` when it has neither a
/// top-level comma nor a range.
fn brace_options(body: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !parts.is_empty() {
        parts.push(current);
        return Some(parts);
    }
    let (start, end) = body.split_once("..")?;
    if let (Ok(start), Ok(end)) = (start.parse::<i64>(), end.parse::<i64>()) {
        let range: Vec<i64> = if start <= end {
            (start..=end).collect()
        } else {
            (end..=start).rev().collect()
        };
        return Some(range.into_iter().map(|n| n.to_string()).collect());
    }
    let (mut start_chars, mut end_chars) = (start.chars(), end.chars());
    match (
        start_chars.next(),
        start_chars.next(),
        end_chars.next(),
        end_chars.next(),
    ) {
        (Some(a), None, Some(b), None) => {
            let (low, high) = (a.min(b), a.max(b));
            let mut range: Vec<String> = (low..=high).map(String::from).collect();
            if a > b {
                range.reverse();
            }
            Some(range)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_and_wildcards() {
        assert!(matches("*.md", "a.md"));
        assert!(!matches("*.md", "dir/a.md"));
        assert!(matches("prompts/*.md", "prompts/a.md"));
        assert!(matches("./prompts/*.md", "prompts/a.md"));
        assert!(matches("**/*.md", "a.md"));
        assert!(matches("**/*.md", "x/y/a.md"));
        assert!(!matches("**/*.md", ".git/a.md"));
        assert!(!matches("*", ".hidden"));
        assert!(matches(".*", ".hidden"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "a/c"));
        assert!(matches("builtin:*", "builtin:mcp"));
    }

    #[test]
    fn classes_and_escapes() {
        assert!(matches("[abc].ts", "b.ts"));
        assert!(!matches("[!abc].ts", "b.ts"));
        assert!(matches("[^abc].ts", "d.ts"));
        assert!(matches("v[0-9]", "v7"));
        assert!(matches("a\\*", "a*"));
        assert!(!matches("a\\*", "ab"));
        assert!(matches("[", "["));
    }

    #[test]
    fn braces() {
        assert!(matches("*.{ts,js}", "a.js"));
        assert!(!matches("*.{ts,js}", "a.md"));
        assert!(matches("v{1..3}", "v2"));
        assert!(!matches("v{1..3}", "v4"));
        assert!(matches("{a,b{c,d}}", "bd"));
        assert!(matches("{x}", "{x}"));
    }

    #[test]
    fn model_ids_ignore_case() {
        assert!(matches_with("*SONNET*", "claude-sonnet-4-5", true));
        assert!(!matches_with(
            "*sonnet*",
            "anthropic/claude-sonnet-4-5",
            true
        ));
        assert!(matches_with(
            "anthropic/*",
            "anthropic/claude-sonnet-4-5",
            true
        ));
        assert!(!matches_with(
            "anthropic/*",
            "openrouter/anthropic/claude",
            true
        ));
        assert!(!matches("*SONNET*", "claude-sonnet-4-5"));
    }
}
