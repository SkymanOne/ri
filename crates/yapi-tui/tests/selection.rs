//! Fullscreen mouse selection against pi-tui's, recorded by
//! `tests/fixtures/pi/generator/selection.mjs`: the screen after each mouse
//! report, with reversed cells in brackets, and the text copied; and the rows
//! timed wheel events scroll.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use std::time::{Duration, Instant};

use ratatui_core::style::Modifier;
use serde_json::Value;
use yapi_tui::ansi::parse_line;
use yapi_tui::lines::StyledLine;
use yapi_tui::screen::{AltScreen, MouseAction, Scrollbar, WheelScroll};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../tests/fixtures/pi/selection/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn lines(value: &Value) -> Vec<StyledLine> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|line| parse_line(line.as_str().unwrap()).0)
        .collect()
}

/// A row as text, with each run of reversed cells in brackets.
fn marked(line: &StyledLine) -> String {
    let mut out = String::new();
    let mut reversed = false;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let on = style.add_modifier.contains(Modifier::REVERSED);
        if on != reversed && !span.content.is_empty() {
            out.push(if on { '[' } else { ']' });
            reversed = on;
        }
        out.push_str(&span.content);
    }
    if reversed {
        out.push(']');
    }
    out.trim_end().to_owned()
}

#[test]
fn selects_like_pi() {
    let mut failures = Vec::new();
    for case in fixture("selection.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let (width, height) = (
            case["columns"].as_u64().unwrap() as usize,
            case["rows"].as_u64().unwrap() as usize,
        );
        let transcript = lines(&case["transcript"]);
        let dock = lines(&case["dock"]);
        let mut screen = AltScreen::new();
        screen.scrollbar = match case["scrollbar"].as_str().unwrap() {
            "always" => Scrollbar::Always,
            "auto" => Scrollbar::Auto,
            _ => Scrollbar::Hidden,
        };
        screen.copy_on_select = case["copyOnSelect"].as_bool().unwrap();
        screen.frame(&transcript, &dock, None, width, height);
        for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let input = step["input"].as_str().unwrap();
            let mut copied = Vec::new();
            if input == "tick" {
                // A held drag scrolls on the first frame 50 ms after the last.
                std::thread::sleep(Duration::from_millis(60));
            } else {
                match screen.mouse(input, &transcript) {
                    MouseAction::Copy(text) => {
                        screen.flash("Copied!", Duration::from_secs(60));
                        copied.push(text);
                    }
                    MouseAction::Handled => {}
                    MouseAction::Unhandled => failures.push(format!("{name}: {input:?} unhandled")),
                }
            }
            screen.frame(&transcript, &dock, None, width, height);
            let expected_copied: Vec<&str> = step["copied"]
                .as_array()
                .unwrap()
                .iter()
                .map(|text| text.as_str().unwrap())
                .collect();
            if copied != expected_copied {
                failures.push(format!(
                    "{name} step {index} {input:?}: pi copied {expected_copied:?}, yapi {copied:?}"
                ));
            }
            let expected: Vec<String> = lines(&step["screen"]).iter().map(marked).collect();
            let actual: Vec<String> = screen.screen_lines().iter().map(marked).collect();
            if actual != expected {
                failures.push(format!(
                    "{name} step {index} {input:?}:\n  pi   {expected:?}\n  yapi {actual:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn accelerates_the_wheel_like_pi() {
    let start = Instant::now();
    for case in fixture("wheel.json").as_array().unwrap() {
        let mut wheel = WheelScroll::default();
        wheel.accelerate = case["accelerate"].as_bool().unwrap();
        let lines = case["lines"].as_u64().map(|lines| lines as usize);
        let steps: Vec<u64> = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| {
                let direction = event[0].as_i64().unwrap() as isize;
                let at = start + Duration::from_millis(event[1].as_u64().unwrap());
                wheel.next(lines, direction, at) as u64
            })
            .collect();
        let expected: Vec<u64> = case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lines| lines.as_u64().unwrap())
            .collect();
        assert_eq!(steps, expected, "{}", case["name"]);
    }
}
