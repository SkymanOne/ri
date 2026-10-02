//! Theme colors against pi's, recorded by `tests/fixtures/pi/generator/theme.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use ratatui::style::{Color as TermColor, Modifier, Style};
use ri_tui::color::ColorMode;
use ri_tui::theme::{
    Appearance, BACKGROUND_TOKENS, SystemThemeInput, SystemValue, Theme, generate_system_theme,
};
use serde_json::Value;

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
                failures.push(format!("{key} {token}: pi {expected:?}, ri {actual:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn rgb(value: &Value) -> Option<[f64; 3]> {
    let value = value.as_object()?;
    Some(["r", "g", "b"].map(|channel| value[channel].as_f64().unwrap()))
}

#[test]
fn system_themes_match_pi() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for (name, case) in fixture["system"].as_object().unwrap() {
        let input = &case["input"];
        let generated = generate_system_theme(&SystemThemeInput {
            foreground: rgb(&input["foreground"]),
            background: rgb(&input["background"]),
            palette: input["palette"]
                .as_array()
                .map(|palette| palette.iter().map(|entry| rgb(entry).unwrap()).collect()),
            saturation: input["saturation"].as_f64().unwrap_or(1.0),
            appearance_hint: match input["appearanceHint"].as_str() {
                Some("light") => Some(Appearance::Light),
                Some("dark") => Some(Appearance::Dark),
                _ => None,
            },
        });
        for (token, value) in case["colors"].as_object().unwrap() {
            let expected = match value {
                Value::Number(index) => SystemValue::Index(index.as_u64().unwrap() as u8),
                Value::String(text) if text.is_empty() => SystemValue::Default,
                Value::String(hex) => SystemValue::Rgb(
                    [1, 3, 5]
                        .map(|at| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap())),
                ),
                other => panic!("unexpected value {other}"),
            };
            let actual = generated
                .colors
                .iter()
                .find(|(t, _)| t == token)
                .map(|(_, value)| *value);
            if actual != Some(expected) {
                failures.push(format!("{name} {token}: pi {expected:?}, ri {actual:?}"));
            }
        }
        let dim: Vec<&str> = case["dim"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token.as_str().unwrap())
            .collect();
        if generated.dim != dim {
            failures.push(format!("{name} dim: pi {dim:?}, ri {:?}", generated.dim));
        }
        let appearance = match case["appearance"].as_str() {
            Some("dark") => Some(Appearance::Dark),
            Some("light") => Some(Appearance::Light),
            _ => None,
        };
        if generated.appearance != appearance {
            failures.push(format!("{name} appearance"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
