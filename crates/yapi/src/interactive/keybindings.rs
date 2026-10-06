//! The coding agent's key bindings: pi-tui's actions plus the app actions,
//! loaded from `keybindings.json`.
//!
//! Port of `packages/coding-agent/src/core/keybindings.ts` in pi `v1.0.0`.

use std::path::Path;

use serde_json::{Map, Value};
use yapi_tui::keybindings::{Definition, Keybindings, UserBindings, tui_definitions};
use yapi_tui::keys::Keys;

/// WSL and Windows get different defaults for keys their consoles reserve.
fn windows_keys() -> bool {
    cfg!(windows)
        || (cfg!(target_os = "linux")
            && ["WSL_DISTRO_NAME", "WSL_INTEROP"]
                .iter()
                .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty())))
}

/// Every action in pi's order: pi-tui's, then the app's.
pub fn definitions() -> Vec<Definition> {
    let windows = windows_keys();
    let mut definitions = tui_definitions();
    for definition in &mut definitions {
        let keys: Option<&[&'static str]> = match definition.id {
            "tui.editor.undo" if cfg!(windows) => Some(&["ctrl+z"]),
            "tui.editor.undo" if windows => Some(&["alt+z"]),
            "tui.altScreen.previousPrompt" if windows => Some(&["ctrl+up"]),
            "tui.altScreen.nextPrompt" if windows => Some(&["ctrl+down"]),
            "tui.altScreen.search" if windows => Some(&["ctrl+f"]),
            _ => None,
        };
        if let Some(keys) = keys {
            definition.default_keys = keys;
        }
    }
    let mac = cfg!(target_os = "macos");
    let app: Vec<(&'static str, &'static [&'static str], &'static str)> = vec![
        ("app.interrupt", &["escape"], "Cancel or abort"),
        ("app.clear", &["ctrl+c"], "Clear editor"),
        ("app.exit", &["ctrl+d"], "Exit when editor is empty"),
        (
            "app.suspend",
            if cfg!(windows) { &[] } else { &["ctrl+z"] },
            "Suspend to background",
        ),
        ("app.thinking.cycle", &["shift+tab"], "Cycle thinking level"),
        ("app.thinking.save", &["ctrl+s"], "Save thinking level"),
        ("app.model.cycleForward", &["ctrl+p"], "Cycle to next model"),
        (
            "app.model.cycleBackward",
            if windows {
                &["alt+p"]
            } else {
                &["shift+ctrl+p"]
            },
            "Cycle to previous model",
        ),
        ("app.model.select", &["ctrl+l"], "Open model selector"),
        ("app.tools.expand", &["ctrl+o"], "Toggle tool output"),
        ("app.thinking.toggle", &["ctrl+t"], "Toggle thinking blocks"),
        (
            "app.session.toggleNamedFilter",
            &["ctrl+n"],
            "Toggle named session filter",
        ),
        ("app.editor.external", &["ctrl+g"], "Open external editor"),
        (
            "app.message.copy",
            &["ctrl+x"],
            "Copy selection or last assistant message",
        ),
        (
            "app.message.followUp",
            if windows { &["ctrl+q"] } else { &["alt+enter"] },
            "Queue follow-up message",
        ),
        (
            "app.message.dequeue",
            if windows { &["alt+q"] } else { &["alt+up"] },
            "Restore queued messages",
        ),
        (
            "app.clipboard.pasteImage",
            if windows { &["alt+v"] } else { &["ctrl+v"] },
            "Paste files on macOS, images, or text from clipboard",
        ),
        ("app.session.new", &[], "Start a new session"),
        ("app.session.tree", &[], "Open session tree"),
        ("app.session.fork", &[], "Fork current session"),
        ("app.session.resume", &[], "Resume a session"),
        (
            "app.tree.foldOrUp",
            if mac {
                &["alt+left", "ctrl+left"]
            } else {
                &["ctrl+left", "alt+left"]
            },
            "Fold tree branch or move up",
        ),
        (
            "app.tree.unfoldOrDown",
            if mac {
                &["alt+right", "ctrl+right"]
            } else {
                &["ctrl+right", "alt+right"]
            },
            "Unfold tree branch or move down",
        ),
        ("app.tree.editLabel", &["shift+l"], "Edit tree label"),
        (
            "app.tree.toggleLabelTimestamp",
            &["shift+t"],
            "Toggle tree label timestamps",
        ),
        (
            "app.session.togglePath",
            &["ctrl+p"],
            "Toggle session path display",
        ),
        (
            "app.session.toggleSort",
            &["ctrl+s"],
            "Toggle session sort mode",
        ),
        ("app.session.rename", &["ctrl+r"], "Rename session"),
        ("app.session.delete", &["ctrl+d"], "Delete session"),
        (
            "app.session.deleteNoninvasive",
            &["ctrl+backspace"],
            "Delete session when query is empty",
        ),
        ("app.models.save", &["ctrl+s"], "Save model selection"),
        ("app.models.enableAll", &["ctrl+a"], "Enable all models"),
        ("app.models.clearAll", &["ctrl+x"], "Clear all models"),
        (
            "app.models.toggleProvider",
            &["ctrl+p"],
            "Toggle all models for provider",
        ),
        (
            "app.models.reorderUp",
            &["alt+up"],
            "Move model up in order",
        ),
        (
            "app.models.reorderDown",
            &["alt+down"],
            "Move model down in order",
        ),
        (
            "app.tree.filter.default",
            &["ctrl+d"],
            "Tree filter: default view",
        ),
        (
            "app.tree.filter.noTools",
            &["ctrl+t"],
            "Tree filter: hide tool results",
        ),
        (
            "app.tree.filter.userOnly",
            &["ctrl+u"],
            "Tree filter: user messages only",
        ),
        (
            "app.tree.filter.labeledOnly",
            &["ctrl+l"],
            "Tree filter: labeled entries only",
        ),
        (
            "app.tree.filter.all",
            &["ctrl+a"],
            "Tree filter: show all entries",
        ),
        (
            "app.tree.filter.cycleForward",
            &["ctrl+o"],
            "Tree filter: cycle forward",
        ),
        (
            "app.tree.filter.cycleBackward",
            &["shift+ctrl+o"],
            "Tree filter: cycle backward",
        ),
    ];
    definitions.extend(
        app.into_iter()
            .map(|(id, keys, description)| Definition::new(id, keys, description)),
    );
    definitions
}

/// Legacy action names and their ids.
const MIGRATIONS: [(&str, &str); 59] = [
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

fn migrated_name(key: &str) -> &str {
    MIGRATIONS
        .iter()
        .find(|(legacy, _)| *legacy == key)
        .map_or(key, |(_, id)| id)
}

/// Rewrites legacy action names; returns the document in definition order and
/// whether anything changed. A new id beats its legacy name.
pub fn migrate(raw: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut config = Map::new();
    let mut migrated = false;
    for (key, value) in raw {
        let next = migrated_name(key);
        if next != key {
            migrated = true;
            if raw.contains_key(next) {
                continue;
            }
        }
        config.insert(next.to_owned(), value.clone());
    }
    let mut ordered = Map::new();
    for definition in definitions() {
        if let Some(value) = config.remove(definition.id) {
            ordered.insert(definition.id.to_owned(), value);
        }
    }
    let mut extras: Vec<(String, Value)> = config.into_iter().collect();
    extras.sort_by(|a, b| a.0.cmp(&b.0));
    ordered.extend(extras);
    (ordered, migrated)
}

fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    match serde_json::from_str(text).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// pi's startup migration: rewrites a `keybindings.json` that uses legacy
/// names. Errors are ignored, as in pi.
pub fn migrate_file(agent_dir: &Path) {
    let path = agent_dir.join("keybindings.json");
    let Some(raw) = read_object(&path) else {
        return;
    };
    let (config, migrated) = migrate(&raw);
    if !migrated {
        return;
    }
    if let Ok(text) = yapi_types::json::to_string_pretty(&Value::Object(config), "  ") {
        let _ = std::fs::write(&path, format!("{text}\n"));
    }
}

/// The effective bindings: defaults with `keybindings.json` applied.
pub fn load(agent_dir: &Path, keys: Keys) -> Keybindings {
    let mut user = UserBindings::new();
    if let Some(raw) = read_object(&agent_dir.join("keybindings.json")) {
        for (id, value) in migrate(&raw).0 {
            let keys = match value {
                Value::String(key) => vec![key],
                Value::Array(items) if items.iter().all(Value::is_string) => items
                    .into_iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect(),
                _ => continue,
            };
            user.insert(id, keys);
        }
    }
    Keybindings::new(keys, definitions(), &user)
}

/// A key id for display: `ctrl+c`, with `option` for `alt` on macOS.
pub fn key_text(key: &str) -> String {
    if cfg!(target_os = "macos") {
        key.split('+')
            .map(|part| if part == "alt" { "option" } else { part })
            .collect::<Vec<_>>()
            .join("+")
    } else {
        key.to_owned()
    }
}

/// The keys bound to `action`, joined by `/`.
pub fn keys_text(bindings: &Keybindings, action: &str) -> String {
    bindings
        .keys(action)
        .iter()
        .map(|key| key_text(key))
        .collect::<Vec<_>>()
        .join("/")
}

/// Like [`keys_text`] with each part capitalized: `Alt+Up`.
pub fn keys_display(bindings: &Keybindings, action: &str) -> String {
    keys_text(bindings, action)
        .split('/')
        .map(capitalized)
        .collect::<Vec<_>>()
        .join("/")
}

/// pi's `formatKeyText` with `capitalize`: `alt+u` as `Alt+U`.
pub fn key_display(key: &str) -> String {
    capitalized(&key_text(key))
}

fn capitalized(key: &str) -> String {
    key.split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_names_in_definition_order() {
        let raw: Map<String, Value> = serde_json::from_str(
            r#"{"zeta": "x", "interrupt": "escape", "cursorUp": ["up", "ctrl+p"], "app.clear": "ctrl+x", "clear": "ctrl+c"}"#,
        )
        .unwrap_or_default();
        let (config, migrated) = migrate(&raw);
        assert!(migrated);
        let keys: Vec<&str> = config.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["tui.editor.cursorUp", "app.interrupt", "app.clear", "zeta"]
        );
        assert_eq!(config["app.clear"], "ctrl+x");
    }

    #[test]
    fn formats_keys() {
        let bindings = Keybindings::new(Keys::default(), definitions(), &UserBindings::new());
        assert_eq!(keys_text(&bindings, "tui.select.cancel"), "escape/ctrl+c");
        if !windows_keys() && !cfg!(target_os = "macos") {
            assert_eq!(keys_display(&bindings, "app.message.dequeue"), "Alt+Up");
        }
    }
}
