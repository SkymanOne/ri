//! `--list-models`: the models with credentials, optionally fuzzy-filtered.
//!
//! Port of `packages/coding-agent/src/cli/list-models.ts` in pi `v1.0.0`.

use std::cmp::Ordering;
use std::io::Write;

use ri_ai::registry::ModelRegistry;
use ri_core::config::agent_dir;
use ri_tui::fuzzy::fuzzy_filter;
use ri_types::model::Model;

/// `x.toFixed(1)`: rounds half up on the exact value, as JavaScript does.
fn to_fixed_1(value: f64) -> String {
    let wide = format!("{value:.30}");
    let Some((whole, fraction)) = wide.split_once('.') else {
        return wide;
    };
    let mut digits: Vec<u8> = whole.bytes().chain(fraction.bytes().take(1)).collect();
    if fraction
        .as_bytes()
        .get(1)
        .is_some_and(|digit| *digit >= b'5')
    {
        let mut index = digits.len();
        loop {
            if index == 0 {
                digits.insert(0, b'1');
                break;
            }
            index -= 1;
            if digits[index] == b'9' {
                digits[index] = b'0';
            } else {
                digits[index] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&digits).into_owned();
    let split = text.len() - 1;
    format!("{}.{}", &text[..split], &text[split..])
}

/// `200000` as `200K`, `1000000` as `1M`, `1500000` as `1.5M`.
fn format_token_count(count: u64) -> String {
    let scaled = |divisor: f64, suffix: &str| {
        let value = count as f64 / divisor;
        if value.fract() == 0.0 {
            format!("{value}{suffix}")
        } else {
            format!("{}{suffix}", to_fixed_1(value))
        }
    };
    if count >= 1_000_000 {
        scaled(1_000_000.0, "M")
    } else if count >= 1_000 {
        scaled(1_000.0, "K")
    } else {
        count.to_string()
    }
}

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
fn locale_compare(a: &str, b: &str) -> Ordering {
    let primaries = |text: &str| text.chars().map(primary).collect::<Vec<_>>();
    primaries(a)
        .cmp(&primaries(b))
        .then_with(|| {
            let case = |text: &str| text.chars().map(char::is_uppercase).collect::<Vec<_>>();
            case(a).cmp(&case(b))
        })
        .then_with(|| a.cmp(b))
}

/// Prints the table and returns the exit code.
pub fn run(pattern: Option<&str>) -> u8 {
    let registry = ModelRegistry::load(&agent_dir());
    if let Some(error) = registry.error() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let mut out = std::io::stdout().lock();
    let available: Vec<&Model> = registry
        .models()
        .iter()
        .filter(|model| registry.has_auth(&model.provider))
        .collect();
    if available.is_empty() {
        let _ = writeln!(
            out,
            "No models available. Use /login to log into a provider via OAuth or API key."
        );
        return 0;
    }
    let mut models = match pattern {
        Some(pattern) => fuzzy_filter(available, pattern, |model| {
            format!("{} {}", model.provider, model.id)
        }),
        None => available,
    };
    if models.is_empty() {
        let _ = writeln!(
            out,
            "No models matching \"{}\"",
            pattern.unwrap_or_default()
        );
        return 0;
    }
    models.sort_by(|a, b| {
        locale_compare(&a.provider, &b.provider).then_with(|| locale_compare(&a.id, &b.id))
    });
    let header = [
        "provider", "model", "context", "max-out", "thinking", "images",
    ]
    .map(str::to_owned);
    let rows: Vec<[String; 6]> = models
        .iter()
        .map(|model| {
            [
                model.provider.clone(),
                model.id.clone(),
                format_token_count(model.context_window),
                format_token_count(model.max_tokens),
                if model.reasoning { "yes" } else { "no" }.to_owned(),
                if model.accepts_images() { "yes" } else { "no" }.to_owned(),
            ]
        })
        .collect();
    let width = |column: usize| {
        std::iter::once(&header)
            .chain(&rows)
            .map(|row| row[column].encode_utf16().count())
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..6).map(width).collect();
    for row in std::iter::once(&header).chain(&rows) {
        let line = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| {
                let pad = width.saturating_sub(cell.encode_utf16().count());
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("  ");
        let _ = writeln!(out, "{line}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_like_pi() {
        assert_eq!(format_token_count(200_000), "200K");
        assert_eq!(format_token_count(1_000_000), "1M");
        assert_eq!(format_token_count(1_048_576), "1.0M");
        assert_eq!(format_token_count(1_250_000), "1.3M");
        assert_eq!(format_token_count(131_072), "131.1K");
        assert_eq!(format_token_count(999), "999");
    }

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
