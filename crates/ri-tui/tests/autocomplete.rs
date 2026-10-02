//! Completion against pi-tui's `CombinedAutocompleteProvider`, recorded by
//! `tests/fixtures/pi/generator/autocomplete.mjs`.

#![allow(clippy::unwrap_used, reason = "test fixture access")]

use std::path::PathBuf;

use ri_tui::autocomplete::{AutocompleteProvider, CombinedProvider, SlashCommand};
use ri_tui::fuzzy::fuzzy_filter;
use ri_tui::select_list::SelectItem;
use serde_json::{Value, json};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pi/autocomplete/cases.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn find_fd() -> Option<PathBuf> {
    ["fd", "fdfind"].into_iter().find_map(|name| {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| PathBuf::from(name))
    })
}

fn item_json(item: &SelectItem) -> Value {
    let mut value = json!({"value": item.value, "label": item.label});
    if let Some(description) = &item.description {
        value["description"] = json!(description);
    }
    value
}

#[test]
fn completes_like_pi() {
    let fixture = fixture();
    let base = std::env::temp_dir().join(format!("ri-autocomplete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for path in fixture["tree"].as_array().unwrap() {
        let path = path.as_str().unwrap();
        if let Some(dir) = path.strip_suffix('/') {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        } else {
            std::fs::write(base.join(path), "").unwrap();
        }
    }
    let levels: Vec<String> = fixture["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|level| level.as_str().unwrap().to_owned())
        .collect();
    let commands = fixture["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|spec| {
            let levels = levels.clone();
            SlashCommand {
                name: spec["name"].as_str().unwrap().to_owned(),
                description: spec["description"].as_str().map(str::to_owned),
                argument_hint: spec["argumentHint"].as_str().map(str::to_owned),
                complete: spec["levels"].as_bool().map(|_| {
                    Box::new(move |prefix: &str| {
                        let filtered = fuzzy_filter(levels.clone(), prefix, Clone::clone);
                        (!filtered.is_empty())
                            .then(|| filtered.into_iter().map(SelectItem::new).collect())
                    }) as ri_tui::autocomplete::ArgumentCompleter
                }),
            }
        })
        .collect();
    let fd = find_fd();
    if fd.is_none() {
        eprintln!("fd not found: skipping @ cases");
    }
    let provider = CombinedProvider::new(commands, base.clone(), fd.clone(), None);
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        if case["fd"].as_bool().unwrap() && fd.is_none() {
            continue;
        }
        let lines: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect();
        let line = case["line"].as_u64().unwrap() as usize;
        let col = case["col"].as_u64().unwrap() as usize;
        let force = case["force"].as_bool().unwrap();
        let suggestions = provider.suggestions(&lines, line, col, force);
        let actual = suggestions.as_ref().map(|suggestions| {
            json!({
                "items": suggestions.items.iter().map(item_json).collect::<Vec<_>>(),
                "prefix": suggestions.prefix,
            })
        });
        let applied = suggestions.as_ref().map(|suggestions| {
            let completion = provider.apply(
                &lines,
                line,
                col,
                &suggestions.items[0],
                &suggestions.prefix,
            );
            json!({
                "lines": completion.lines,
                "cursorLine": completion.cursor_line,
                "cursorCol": completion.cursor_col,
            })
        });
        let actual = json!({
            "suggestions": actual,
            "applied": applied,
            "fileCompletion": provider.should_trigger_file_completion(&lines, line, col),
        });
        let expected = json!({
            "suggestions": case["suggestions"],
            "applied": case["applied"],
            "fileCompletion": case["fileCompletion"],
        });
        if actual != expected {
            failures.push(format!(
                "{lines:?} @{line}:{col} force={force}\n  expected {expected}\n  actual   {actual}"
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
