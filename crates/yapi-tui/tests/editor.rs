//! The editor against pi-tui's Editor on the same key sequences, recorded by
//! `tests/fixtures/pi/generator/editor.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use serde_json::Value;
use yapi_tui::editor::{Editor, EditorEvent, EditorTheme};
use yapi_tui::keybindings::{Keybindings, UserBindings, tui_definitions};
use yapi_tui::keys::Keys;

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn edits_like_pi() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/editor/editor.json"
    ))
    .unwrap();
    let cases: Vec<Value> = serde_json::from_str(&text).unwrap();
    let keybindings = Keybindings::new(Keys::default(), tui_definitions(), &UserBindings::new());
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let width = case["width"].as_u64().unwrap() as usize;
        let mut editor = Editor::new(
            EditorTheme::default(),
            case["paddingX"].as_u64().unwrap() as usize,
            5,
        );
        editor.focused = false;
        editor.set_terminal_rows(case["rows"].as_u64().unwrap() as usize);
        for entry in strings(&case["history"]) {
            editor.add_to_history(&entry);
        }
        if let Some(text) = case["setText"].as_str() {
            editor.set_text(text);
        }
        editor.render(width);
        let mut submitted = Vec::new();
        for (step, state) in case["states"].as_array().unwrap().iter().enumerate() {
            let key = state["key"].as_str().unwrap();
            if let EditorEvent::Submit(text) = editor.handle_input(key, &keybindings) {
                submitted.push(text);
            }
            let render: Vec<String> = editor
                .render(width)
                .iter()
                .map(ToString::to_string)
                .collect();
            let (line, col) = editor.cursor();
            let col16 = editor.lines()[line][..col].encode_utf16().count();
            let actual = (editor.text(), [line, col16], render, submitted.clone());
            let expected = (
                state["text"].as_str().unwrap().to_owned(),
                [
                    state["cursor"][0].as_u64().unwrap() as usize,
                    state["cursor"][1].as_u64().unwrap() as usize,
                ],
                strings(&state["render"]),
                strings(&state["submitted"]),
            );
            if actual != expected {
                failures.push(format!(
                    "{name} step {step} after {key:?}:\n  pi {expected:?}\n  yapi {actual:?}"
                ));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
