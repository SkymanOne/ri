//! `--list-models`: the models with credentials, optionally fuzzy-filtered.
//!
//! Port of `packages/coding-agent/src/cli/list-models.ts` in pi `v1.0.0`.

use std::io::Write;

use yapi_ai::registry::ModelRegistry;
use yapi_core::config::agent_dir;
use yapi_tui::fuzzy::fuzzy_filter;
use yapi_types::collate::locale_compare;

/// `200000` as `200K`, `1000000` as `1M`, `1500000` as `1.5M`.
fn format_token_count(count: u64) -> String {
    let scaled = |divisor: f64, suffix: &str| {
        let value = count as f64 / divisor;
        if value.fract() == 0.0 {
            format!("{value}{suffix}")
        } else {
            format!("{}{suffix}", yapi_types::js::to_fixed(value, 1))
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

/// Prints the table of `registry`'s models, to stderr when `to_stderr` is
/// set, and returns the exit code.
pub fn run(registry: &ModelRegistry, pattern: Option<&str>, to_stderr: bool) -> u8 {
    if let Some(error) = registry.error() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let mut out: Box<dyn Write> = if to_stderr {
        Box::new(std::io::stderr().lock())
    } else {
        Box::new(std::io::stdout().lock())
    };
    let available = registry.available();
    if available.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            yapi_core::auth_guidance::no_models_available(&yapi_core::docs::Locations::find(
                &agent_dir()
            ))
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
            .map(|row| yapi_types::js::len(&row[column]))
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..6).map(width).collect();
    for row in std::iter::once(&header).chain(&rows) {
        let line = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| {
                let pad = width.saturating_sub(yapi_types::js::len(cell));
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
}
