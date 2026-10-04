//! Fuzzy matching for selectors and filters.
//!
//! Port of `packages/tui/src/fuzzy.ts` in pi `v1.0.0`: characters must appear in
//! order; lower scores are better.

/// Whether `query` matches `text`, and how well; lower scores are better.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    /// All query characters were found in order.
    pub matches: bool,
    /// Match quality; lower is better.
    pub score: f64,
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn match_units(query: &[u16], text: &[u16]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }
    let boundary = |unit: u16| {
        char::from_u32(u32::from(unit))
            .is_some_and(|c| c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | ':'))
    };
    let mut query_index = 0;
    let mut score = 0.0;
    // pi starts at -1, so a match at the first position counts as
    // consecutive.
    let mut last: isize = -1;
    let mut consecutive = 0.0;
    while query_index < query.len() {
        let start = usize::try_from(last + 1).unwrap_or(0).min(text.len());
        let Some(offset) = text[start..]
            .iter()
            .position(|&unit| unit == query[query_index])
        else {
            break;
        };
        let i = start + offset;
        let word_boundary = i == 0 || boundary(text[i - 1]);
        if last + 1 == i as isize {
            consecutive += 1.0;
            score -= consecutive * 5.0;
        } else {
            consecutive = 0.0;
            if last >= 0 {
                score += (i as isize - last - 1) as f64 * 2.0;
            }
        }
        if word_boundary {
            score -= 10.0;
        }
        score += i as f64 * 0.1;
        last = i as isize;
        query_index += 1;
    }
    if query_index < query.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }
    if query == text {
        score -= 100.0;
    }
    FuzzyMatch {
        matches: true,
        score,
    }
}

/// `abc123` and `123abc` also match with the halves swapped, at a small cost.
fn swapped(query: &str) -> Option<String> {
    let letters = query.chars().take_while(char::is_ascii_lowercase).count();
    let digits = query.chars().take_while(char::is_ascii_digit).count();
    let all = |count: usize, f: fn(&char) -> bool| {
        count > 0 && query[count..].chars().all(|c| f(&c)) && query.len() > count
    };
    if all(letters, char::is_ascii_digit) {
        Some(format!("{}{}", &query[letters..], &query[..letters]))
    } else if all(digits, char::is_ascii_lowercase) {
        Some(format!("{}{}", &query[digits..], &query[..digits]))
    } else {
        None
    }
}

/// Matches `query` against `text`, case-insensitively.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query = query.to_lowercase();
    let text = units(&text.to_lowercase());
    let primary = match_units(&units(&query), &text);
    if primary.matches {
        return primary;
    }
    match swapped(&query) {
        Some(swapped) => {
            let result = match_units(&units(&swapped), &text);
            if result.matches {
                FuzzyMatch {
                    matches: true,
                    score: result.score + 5.0,
                }
            } else {
                primary
            }
        }
        None => primary,
    }
}

/// Items whose text matches every whitespace- or slash-separated token of
/// `query`, best first. An empty query keeps every item in order.
pub fn fuzzy_filter<T>(items: Vec<T>, query: &str, text: impl Fn(&T) -> String) -> Vec<T> {
    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return items;
    }
    let mut results: Vec<(T, f64)> = Vec::new();
    for item in items {
        let text = text(&item);
        let mut total = 0.0;
        let mut all = true;
        for token in &tokens {
            let result = fuzzy_match(token, &text);
            if !result.matches {
                all = false;
                break;
            }
            total += result.score;
        }
        if all {
            results.push((item, total));
        }
    }
    // A stable sort, as Array.prototype.sort is.
    results.sort_by(|a, b| a.1.total_cmp(&b.1));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_as_pi_tui() {
        // Values from pi-tui's `fuzzyMatch`.
        for (text, score) in [
            (
                "anthropic anthropic/claude-sonnet-4-5 anthropic claude-sonnet-4-5 Claude Sonnet 4.5 (latest)",
                29.1,
            ),
            (
                "opencode-go opencode-go/muse-spark-1.2-contributor opencode-go muse-spark-1.2-contributor Muse Spark 1.2 Contributor",
                21.2,
            ),
            (
                "opencode opencode/claude-sonnet-5-5 opencode claude-sonnet-5-5 Claude Sonnet 5.5",
                13.7,
            ),
        ] {
            let found = fuzzy_match("opus", text);
            assert!(found.matches);
            assert!(
                (found.score - score).abs() < 1e-9,
                "{text}: {}",
                found.score
            );
        }
    }

    #[test]
    fn matches_in_order() {
        assert!(fuzzy_match("gpt5", "openai gpt-5").matches);
        assert!(!fuzzy_match("xyz", "openai gpt-5").matches);
        assert!(fuzzy_match("5gpt", "gpt5").matches);
        let filtered = fuzzy_filter(
            vec!["anthropic claude-opus", "openai gpt-5", "openai gpt-5-mini"],
            "openai/gpt-5",
            |item| (*item).to_owned(),
        );
        assert_eq!(filtered, ["openai gpt-5", "openai gpt-5-mini"]);
    }
}
