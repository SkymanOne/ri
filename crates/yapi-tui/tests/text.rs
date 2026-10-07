//! Text layout against pi-tui's, recorded by `tests/fixtures/pi/generator/text.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use ratatui_core::style::Modifier;
use ratatui_core::text::Line;
use serde_json::Value;
use yapi_tui::ansi::parse_line;
use yapi_tui::lines::{plain, raw, truncate, wrap};
use yapi_tui::markdown::{MarkdownOptions, MarkdownTheme, render};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/text/text.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn wraps_and_truncates_like_pi() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in fixture["wrap"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let width = case["width"].as_u64().unwrap() as usize;
        let actual: Vec<String> = wrap(&raw(text.replace('\t', "   ")), width)
            .iter()
            .map(plain)
            .collect();
        let expected = strings(&case["lines"]);
        if actual != expected {
            failures.push(format!(
                "wrap {text:?} at {width}: pi {expected:?}, yapi {actual:?}"
            ));
        }
    }
    for case in fixture["truncate"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let width = case["width"].as_u64().unwrap() as usize;
        let ellipsis = case["ellipsis"].as_str().unwrap();
        let actual = plain(&truncate(&raw(text), width, ellipsis));
        let expected = case["line"].as_str().unwrap().replace("\x1b[0m", "");
        if actual != expected {
            failures.push(format!(
                "truncate {text:?} at {width}: pi {expected:?}, yapi {actual:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A line's text with its underlined runs, the links of the cases that
/// underline them, in brackets.
fn underlined(line: &Line) -> String {
    let mut out = String::new();
    let mut open = false;
    for span in line.spans.iter().filter(|span| !span.content.is_empty()) {
        let underline = span.style.add_modifier.contains(Modifier::UNDERLINED);
        if underline != open {
            out.push(if underline { '⟨' } else { '⟩' });
            open = underline;
        }
        out.push_str(&span.content);
    }
    if open {
        out.push('⟩');
    }
    out
}

#[test]
fn renders_markdown_like_pi() {
    let fixture = fixture();
    let theme = MarkdownTheme::default();
    let mut failures = Vec::new();
    for case in fixture["markdown"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let width = case["width"].as_u64().unwrap() as usize;
        let px = case["paddingX"].as_u64().unwrap() as usize;
        let py = case["paddingY"].as_u64().unwrap() as usize;
        let preserve = case["preserve"].as_bool().unwrap();
        let options = MarkdownOptions {
            preserve_list_markers: preserve,
            preserve_backslash_escapes: preserve,
            ..MarkdownOptions::default()
        };
        let underline = case["underline"].as_bool() == Some(true);
        let actual: Vec<String> = render(
            case["text"].as_str().unwrap(),
            width,
            px,
            py,
            &theme,
            options,
        )
        .iter()
        .map(|line| {
            if underline {
                underlined(line)
            } else {
                plain(line)
            }
        })
        .collect();
        // pi closes styles inside wrapped table cells even with an identity theme.
        let expected: Vec<String> = strings(&case["lines"])
            .iter()
            .map(|line| match underline {
                true => underlined(&parse_line(line).0),
                false => line.replace("\x1b[22;23;24;25;27;28;29;39m", ""),
            })
            .collect();
        if actual != expected {
            failures.push(format!(
                "{name} width {width} pad {px},{py} preserve {preserve}:\n  pi {expected:#?}\n  yapi {actual:#?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures\n{}",
        failures.len(),
        failures.join("\n")
    );
}
