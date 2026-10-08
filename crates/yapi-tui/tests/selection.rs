//! Fullscreen mouse handling against pi-tui's, recorded by
//! `tests/fixtures/pi/generator/selection.mjs`: the screen after each mouse
//! report or key, with reversed cells in brackets, the text copied, the
//! links opened and what a docked list reports. The dock is lines, a focused
//! editor that completes slash commands, a select list, a searchable settings
//! list or an input. Also the rows timed wheel events scroll.

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
use yapi_tui::screen::{AltScreen, MouseAction, MouseEvent, Scrollbar, WheelScroll};
use yapi_tui::select_list::{
    SelectEvent, SelectItem, SelectList, SelectListLayout, SelectListTheme,
};
use yapi_tui::settings_list::{SettingItem, SettingsEvent, SettingsList, SettingsListTheme};
use yapi_tui::text_input::TextInput;

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

/// The component a case docks, as the generator sets it up.
enum Docked {
    Editor(Box<Editor>),
    Select(SelectList),
    Settings(SettingsList),
    Input(TextInput),
}

impl Docked {
    fn new(case: &Value, rows: usize) -> Option<Docked> {
        if let Some(spec) = case["select"].as_object() {
            let items = spec["items"].as_array().unwrap();
            let items = items
                .iter()
                .map(|value| SelectItem::new(value.as_str().unwrap()))
                .collect();
            let max = spec["maxVisible"].as_u64().unwrap() as usize;
            let theme = SelectListTheme::default();
            return Some(Docked::Select(SelectList::new(
                items,
                max,
                theme,
                SelectListLayout::default(),
            )));
        }
        if let Some(spec) = case["settings"].as_object() {
            let items = spec["items"].as_array().unwrap();
            let text = |item: &Value, key: &str| item[key].as_str().unwrap().to_owned();
            let items = items
                .iter()
                .map(|item| SettingItem {
                    id: text(item, "id"),
                    label: text(item, "label"),
                    description: Some(text(item, "description")),
                    current_value: text(item, "currentValue"),
                    values: serde_json::from_value(item["values"].clone()).unwrap(),
                    submenu: false,
                })
                .collect();
            let max = spec["maxVisible"].as_u64().unwrap() as usize;
            let theme = SettingsListTheme::default();
            return Some(Docked::Settings(SettingsList::new(items, max, theme, true)));
        }
        if let Some(value) = case["input"].as_str() {
            let mut input = TextInput::default();
            input.focused = true;
            input.set_value(value);
            return Some(Docked::Input(input));
        }
        editor(&case["editor"], rows).map(|editor| Docked::Editor(Box::new(editor)))
    }

    fn render(&mut self, width: usize) -> Vec<StyledLine> {
        match self {
            Docked::Editor(editor) => editor.render(width),
            Docked::Select(list) => list.render(width),
            Docked::Settings(list) => list.render(width),
            Docked::Input(input) => vec![input.render(width)],
        }
    }

    /// `event` at row `y` of the dock; whether the component took it, and
    /// what it reports as pi's callbacks do.
    fn mouse(&mut self, event: MouseEvent, y: usize, events: &mut Vec<String>) -> bool {
        match self {
            Docked::Editor(editor) => editor.mouse(event.kind, event.x, y),
            Docked::Select(list) => {
                let taken = list.mouse(event.kind, y);
                match &taken {
                    Some(SelectEvent::Moved) => {
                        let item = list.selected_item().unwrap();
                        events.push(format!("move {}", item.value));
                    }
                    Some(SelectEvent::Selected(item)) => {
                        events.push(format!("select {}", item.value));
                    }
                    _ => {}
                }
                taken.is_some()
            }
            Docked::Settings(list) => {
                let taken = list.mouse(event.kind, event.x, y);
                if let Some(SettingsEvent::Changed { id, value }) = &taken {
                    events.push(format!("change {id} {value}"));
                }
                taken.is_some()
            }
            Docked::Input(input) => y == 0 && input.mouse(event.kind, event.x),
        }
    }

    fn handle_input(&mut self, input: &str, keybindings: &Keybindings) {
        match self {
            Docked::Editor(editor) => drop(editor.handle_input(input, keybindings)),
            Docked::Select(list) => drop(list.handle_input(input, keybindings)),
            Docked::Settings(list) => drop(list.handle_input(input, keybindings)),
            Docked::Input(field) => drop(field.handle_input(input, keybindings)),
        }
    }
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
        let mut docked = Docked::new(case, height);
        let mut dock = match &mut docked {
            Some(docked) => docked.render(width),
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
            let mut events = Vec::new();
            if input == "tick" {
                // A held drag scrolls on the first frame 50 ms after the last.
                std::thread::sleep(Duration::from_millis(60));
            } else {
                // The dock sits at the bottom, and only a component takes events.
                let top = height - dock.len();
                let action = screen.mouse(input, &transcript, |event| {
                    let Some(docked) = docked.as_mut().filter(|_| event.y >= top) else {
                        return false;
                    };
                    docked.mouse(event, event.y - top, &mut events)
                });
                match action {
                    MouseAction::Copy(text) => {
                        screen.flash("Copied!", Duration::from_secs(60));
                        copied.push(text);
                    }
                    MouseAction::Open(url) => opened.push(url),
                    MouseAction::Handled => {}
                    MouseAction::Unhandled => match &mut docked {
                        Some(docked) => docked.handle_input(input, &keybindings),
                        None => failures.push(format!("{name}: {input:?} unhandled")),
                    },
                }
            }
            if let Some(docked) = &mut docked {
                dock = docked.render(width);
            }
            screen.frame(&transcript, &dock, None, width, height);
            let expected_copied: Vec<&str> = step["copied"]
                .as_array()
                .unwrap()
                .iter()
                .map(|text| text.as_str().unwrap())
                .collect();
            let expected_events: Vec<&str> = step["events"]
                .as_array()
                .map(|events| events.iter().map(|event| event.as_str().unwrap()).collect())
                .unwrap_or_default();
            if events != expected_events {
                failures.push(format!(
                    "{name} step {index} {input:?}: pi reported {expected_events:?}, yapi {events:?}"
                ));
            }
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
