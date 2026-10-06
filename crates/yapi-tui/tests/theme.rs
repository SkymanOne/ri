//! Theme colors against pi's, recorded by `tests/fixtures/pi/generator/theme.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use ratatui_core::style::{Color as TermColor, Modifier, Style};
use serde_json::Value;
use yapi_tui::color::ColorMode;
use yapi_tui::theme::{BACKGROUND_TOKENS, Theme};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/theme/theme.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The style an escape sequence such as `\x1b[38;2;1;2;3m\x1b[2m` sets.
fn style_of(ansi: &str) -> Style {
    let mut style = Style::new();
    for code in ansi.split('\x1b').filter(|code| !code.is_empty()) {
        let params: Vec<&str> = code
            .trim_start_matches('[')
            .trim_end_matches('m')
            .split(';')
            .collect();
        let color = match params.as_slice() {
            [_, "2", r, g, b] => {
                TermColor::Rgb(r.parse().unwrap(), g.parse().unwrap(), b.parse().unwrap())
            }
            [_, "5", index] => TermColor::Indexed(index.parse().unwrap()),
            ["39"] | ["49"] => TermColor::Reset,
            ["2"] => {
                style = style.add_modifier(Modifier::DIM);
                continue;
            }
            other => panic!("unexpected escape {other:?}"),
        };
        style = if params[0].starts_with('4') {
            style.bg(color)
        } else {
            style.fg(color)
        };
    }
    style
}

#[test]
fn builtin_themes_match_pi() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for (key, tokens) in fixture["builtin"].as_object().unwrap() {
        let (name, mode) = key.split_once('/').unwrap();
        let mode = if mode == "truecolor" {
            ColorMode::TrueColor
        } else {
            ColorMode::Ansi256
        };
        let theme = Theme::builtin(name, mode).unwrap();
        for (token, ansi) in tokens.as_object().unwrap() {
            let actual = if BACKGROUND_TOKENS.contains(&token.as_str()) {
                theme.bg(token)
            } else {
                theme.fg(token)
            };
            let expected = style_of(ansi.as_str().unwrap());
            if actual != expected {
                failures.push(format!("{key} {token}: pi {expected:?}, yapi {actual:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
