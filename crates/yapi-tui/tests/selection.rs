//! Fullscreen mouse handling against pi-tui's, recorded by
//! `tests/fixtures/pi/generator/selection.mjs`: the screen after each mouse
//! report or key, with reversed cells in brackets, the text copied and the
//! links opened. The dock is lines or a focused editor that completes slash
//! commands. Also the rows timed wheel events scroll.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use std::time::{Duration, Instant};

use ratatui_core::style::Modifier;
use serde_json::Value;
use yapi_tui::ansi::parse_line;
use yapi_tui::autocomplete::{CombinedProvider, SlashCommand};
use yapi_tui::editor::{Editor, EditorTheme};
use yapi_tui::keybindings::{Keybindings, UserBindings, tui_definitions};
use yapi_tui::keys::Keys;
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

/// The editor a case docks, as the generator sets it up.
fn editor(spec: &Value, rows: usize) -> Option<Editor> {
    if spec.is_null() {
        return None;
    }
    let padding = spec["paddingX"].as_u64().unwrap_or(0) as usize;
    let mut editor = Editor::new(EditorTheme::default(), padding, 5);
    editor.set_terminal_rows(rows);
    editor.set_text(spec["text"].as_str().unwrap());
    if let Some(commands) = spec["commands"].as_array() {
        let commands = commands
            .iter()
            .map(|command| SlashCommand {
                name: command["name"].as_str().unwrap().to_owned(),
                description: command["description"].as_str().map(str::to_owned),
                argument_hint: None,
                complete: None,
            })
            .collect();
        let provider = CombinedProvider::new(commands, std::env::temp_dir(), None, None);
        editor.set_autocomplete(Box::new(provider));
    }
    Some(editor)
}

#[test]
fn handles_the_mouse_like_pi() {
    let keybindings = Keybindings::new(Keys::default(), tui_definitions(), &UserBindings::new());
    let mut failures = Vec::new();
    for case in fixture("selection.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let (width, height) = (
            case["columns"].as_u64().unwrap() as usize,
            case["rows"].as_u64().unwrap() as usize,
        );
        let transcript = lines(&case["transcript"]);
        let mut editor = editor(&case["editor"], height);
        let mut dock = match &mut editor {
            Some(editor) => editor.render(width),
            None => lines(&case["dock"]),
        };
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
            let mut opened = Vec::new();
            if input == "tick" {
                // A held drag scrolls on the first frame 50 ms after the last.
                std::thread::sleep(Duration::from_millis(60));
            } else {
                // The dock sits at the bottom, and only the editor takes events.
                let top = height - dock.len();
                let action = screen.mouse(input, &transcript, |event| {
                    let Some(editor) = editor.as_mut().filter(|_| event.y >= top) else {
                        return false;
                    };
                    editor.mouse(event.kind, event.x, event.y - top)
                });
                match action {
                    MouseAction::Copy(text) => {
                        screen.flash("Copied!", Duration::from_secs(60));
                        copied.push(text);
                    }
                    MouseAction::Open(url) => opened.push(url),
                    MouseAction::Handled => {}
                    MouseAction::Unhandled => match &mut editor {
                        Some(editor) => drop(editor.handle_input(input, &keybindings)),
                        None => failures.push(format!("{name}: {input:?} unhandled")),
                    },
                }
            }
            if let Some(editor) = &mut editor {
                dock = editor.render(width);
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
            let expected_opened: Vec<&str> = step["opened"]
                .as_array()
                .map(|urls| urls.iter().map(|url| url.as_str().unwrap()).collect())
                .unwrap_or_default();
            if opened != expected_opened {
                failures.push(format!(
                    "{name} step {index} {input:?}: pi opened {expected_opened:?}, yapi {opened:?}"
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
