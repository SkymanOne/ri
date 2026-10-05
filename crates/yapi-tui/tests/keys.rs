//! Key decoding against pi-tui's own results over a corpus of terminal input,
//! recorded by `tests/fixtures/pi/generator/keys.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use serde_json::Value;
use yapi_tui::keys::{Keys, decode_printable, is_key_release, is_key_repeat};

fn check(keys: Keys, ids: &[&str], entries: &[Value]) -> Vec<String> {
    let mut failures = Vec::new();
    for entry in entries {
        let data = entry["data"].as_str().unwrap();
        let expected: Vec<&str> = entry["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        let actual: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| keys.matches(data, id))
            .collect();
        if actual != expected {
            failures.push(format!(
                "{data:?} matches: pi {expected:?}, yapi {actual:?}"
            ));
        }
        let parse = keys.parse(data);
        if parse.as_deref() != entry["parse"].as_str() {
            failures.push(format!(
                "{data:?} parse: pi {}, yapi {parse:?}",
                entry["parse"]
            ));
        }
        let printable = decode_printable(data);
        if printable.as_deref() != entry["printable"].as_str() {
            failures.push(format!(
                "{data:?} printable: pi {}, yapi {printable:?}",
                entry["printable"]
            ));
        }
        if is_key_release(data) != (entry["release"] == true) {
            failures.push(format!("{data:?} release"));
        }
        if is_key_repeat(data) != (entry["repeat"] == true) {
            failures.push(format!("{data:?} repeat"));
        }
    }
    failures
}

#[test]
fn decodes_like_pi() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/keys/keys.json"
    ))
    .unwrap();
    let fixture: Value = serde_json::from_str(&text).unwrap();
    let ids: Vec<&str> = fixture["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let modes = [
        ("legacy", Keys::default()),
        (
            "kitty",
            Keys {
                kitty: true,
                windows_terminal: false,
            },
        ),
        (
            "windowsTerminal",
            Keys {
                kitty: false,
                windows_terminal: true,
            },
        ),
    ];
    let mut failures = Vec::new();
    for (name, keys) in modes {
        let entries = fixture["modes"][name].as_array().unwrap();
        failures.extend(
            check(keys, &ids, entries)
                .into_iter()
                .map(|failure| format!("[{name}] {failure}")),
        );
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures[..failures.len().min(40)].join("\n")
    );
}

#[test]
fn buffers_input_like_pi() {
    use yapi_tui::input::{Input, InputBuffer};

    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/keys/input.json"
    ))
    .unwrap();
    let cases: Vec<Value> = serde_json::from_str(&text).unwrap();
    let mut failures = Vec::new();
    for case in &cases {
        let mut buffer = InputBuffer::new();
        let mut events = Vec::new();
        for chunk in case["chunks"].as_array().unwrap() {
            match chunk.as_str().unwrap() {
                "<flush>" => events.extend(buffer.flush()),
                chunk => events.extend(buffer.push(chunk.as_bytes())),
            }
        }
        let expected: Vec<Input> = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| match event["key"].as_str() {
                Some(key) => Input::Key(key.to_owned()),
                None => Input::Paste(event["paste"].as_str().unwrap().to_owned()),
            })
            .collect();
        let buffered = buffer.flush();
        let expected_buffered = case["buffered"].as_str().unwrap();
        let buffered_matches = match buffered.as_slice() {
            [] => expected_buffered.is_empty(),
            [Input::Key(key)] => key == expected_buffered,
            _ => false,
        };
        if events != expected || !buffered_matches {
            failures.push(format!(
                "{}: pi {expected:?} + {expected_buffered:?}, yapi {events:?} + {buffered:?}",
                case["chunks"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn keeps_astral_characters_whole() {
    use yapi_tui::input::{Input, InputBuffer};

    // pi splits these into UTF-16 halves; yapi emits the character.
    assert_eq!(
        InputBuffer::new().push("😀".as_bytes()),
        vec![Input::Key("😀".into())]
    );
}
