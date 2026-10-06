//! Named key bindings: defaults, user overrides and matching.
//!
//! Port of `packages/tui/src/keybindings.ts` in pi `v1.0.0`.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::keys::Keys;

/// A bindable action and its default keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// Action id, such as `"tui.editor.undo"`.
    pub id: &'static str,
    /// Key ids bound when the user does not rebind the action.
    pub default_keys: &'static [&'static str],
    /// Description shown in `/hotkeys`.
    pub description: &'static str,
}

impl Definition {
    /// A definition from its parts.
    pub fn new(
        id: &'static str,
        default_keys: &'static [&'static str],
        description: &'static str,
    ) -> Self {
        Definition {
            id,
            default_keys,
            description,
        }
    }
}

/// The editor, input, selection and fullscreen viewport actions, in pi's order.
pub fn tui_definitions() -> Vec<Definition> {
    [
        ("tui.editor.cursorUp", &["up"][..], "Move cursor up"),
        ("tui.editor.cursorDown", &["down"], "Move cursor down"),
        (
            "tui.editor.historyPrevious",
            &[],
            "Select previous prompt history entry",
        ),
        (
            "tui.editor.historyNext",
            &[],
            "Select next prompt history entry",
        ),
        (
            "tui.editor.cursorLeft",
            &["left", "ctrl+b"],
            "Move cursor left",
        ),
        (
            "tui.editor.cursorRight",
            &["right", "ctrl+f"],
            "Move cursor right",
        ),
        (
            "tui.editor.cursorWordLeft",
            &["alt+left", "ctrl+left", "alt+b"],
            "Move cursor word left",
        ),
        (
            "tui.editor.cursorWordRight",
            &["alt+right", "ctrl+right", "alt+f"],
            "Move cursor word right",
        ),
        (
            "tui.editor.cursorLineStart",
            &["home", "ctrl+home", "ctrl+a"],
            "Move to line start",
        ),
        (
            "tui.editor.cursorLineEnd",
            &["end", "ctrl+end", "ctrl+e"],
            "Move to line end",
        ),
        (
            "tui.editor.jumpForward",
            &["ctrl+]"],
            "Jump forward to character",
        ),
        (
            "tui.editor.jumpBackward",
            &["ctrl+alt+]"],
            "Jump backward to character",
        ),
        ("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
        (
            "tui.editor.pageDown",
            &["pageDown", "ctrl+pageDown"],
            "Page down",
        ),
        (
            "tui.editor.deleteCharBackward",
            &["backspace"],
            "Delete character backward",
        ),
        (
            "tui.editor.deleteCharForward",
            &["delete", "ctrl+d"],
            "Delete character forward",
        ),
        (
            "tui.editor.deleteWordBackward",
            &["ctrl+w", "alt+backspace"],
            "Delete word backward",
        ),
        (
            "tui.editor.deleteWordForward",
            &["alt+d", "alt+delete"],
            "Delete word forward",
        ),
        (
            "tui.editor.deleteToLineStart",
            &["ctrl+u"],
            "Delete to line start",
        ),
        (
            "tui.editor.deleteToLineEnd",
            &["ctrl+k"],
            "Delete to line end",
        ),
        ("tui.editor.yank", &["ctrl+y"], "Yank"),
        ("tui.editor.yankPop", &["alt+y"], "Yank pop"),
        ("tui.editor.undo", &["ctrl+-"], "Undo"),
        (
            "tui.input.newLine",
            &["shift+enter", "ctrl+j"],
            "Insert newline",
        ),
        ("tui.input.submit", &["enter"], "Submit input"),
        ("tui.input.tab", &["tab"], "Tab / autocomplete"),
        ("tui.input.copy", &["ctrl+c"], "Copy selection"),
        ("tui.select.up", &["up"], "Move selection up"),
        ("tui.select.down", &["down"], "Move selection down"),
        ("tui.select.pageUp", &["pageUp"], "Selection page up"),
        ("tui.select.pageDown", &["pageDown"], "Selection page down"),
        ("tui.select.confirm", &["enter"], "Confirm selection"),
        (
            "tui.select.cancel",
            &["escape", "ctrl+c"],
            "Cancel selection",
        ),
        (
            "tui.altScreen.pageUp",
            &["pageUp"],
            "Scroll viewport up one page",
        ),
        (
            "tui.altScreen.pageDown",
            &["pageDown"],
            "Scroll viewport down one page",
        ),
        (
            "tui.altScreen.halfPageUp",
            &[],
            "Scroll viewport up half a page",
        ),
        (
            "tui.altScreen.halfPageDown",
            &[],
            "Scroll viewport down half a page",
        ),
        ("tui.altScreen.lineUp", &[], "Scroll viewport up one line"),
        (
            "tui.altScreen.lineDown",
            &[],
            "Scroll viewport down one line",
        ),
        (
            "tui.altScreen.previousPrompt",
            &["ctrl+shift+up", "ctrl+up"],
            "Jump to previous semantic prompt",
        ),
        (
            "tui.altScreen.nextPrompt",
            &["ctrl+shift+down", "ctrl+down"],
            "Jump to next semantic prompt",
        ),
        (
            "tui.altScreen.search",
            &["ctrl+shift+f"],
            "Search the primary scroll view",
        ),
        (
            "tui.altScreen.searchNext",
            &["enter", "ctrl+g"],
            "Select the next search match",
        ),
        (
            "tui.altScreen.searchPrevious",
            &["shift+enter", "ctrl+shift+g"],
            "Select the previous search match",
        ),
        (
            "tui.altScreen.searchClose",
            &["escape"],
            "Close transcript search",
        ),
        ("tui.altScreen.top", &["home"], "Scroll viewport to top"),
        (
            "tui.altScreen.bottom",
            &["end"],
            "Scroll viewport to bottom",
        ),
    ]
    .into_iter()
    .map(|(id, keys, description)| Definition::new(id, keys, description))
    .collect()
}

/// User bindings in file order: action id to key ids. An empty list unbinds
/// the action.
pub type UserBindings = IndexMap<String, Vec<String>>;

fn dedup(keys: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = Vec::new();
    for key in keys {
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen
}

/// Resolved key bindings for a set of definitions.
#[derive(Clone, Debug)]
pub struct Keybindings {
    keys: Keys,
    definitions: Vec<Definition>,
    resolved: HashMap<&'static str, Vec<String>>,
}

impl Keybindings {
    /// Bindings for `definitions`, with `user` replacing an action's defaults
    /// wherever it names that action.
    pub fn new(keys: Keys, definitions: Vec<Definition>, user: &UserBindings) -> Keybindings {
        let resolved = definitions
            .iter()
            .map(|definition| {
                let keys = match user.get(definition.id) {
                    Some(keys) => dedup(keys.iter().cloned()),
                    None => dedup(definition.default_keys.iter().map(|key| (*key).to_owned())),
                };
                (definition.id, keys)
            })
            .collect();
        Keybindings {
            keys,
            definitions,
            resolved,
        }
    }

    /// The key decoder in use.
    pub fn decoder(&self) -> Keys {
        self.keys
    }

    /// Switches decoding once the Kitty keyboard protocol is negotiated.
    pub fn set_kitty(&mut self, kitty: bool) {
        self.keys.kitty = kitty;
    }

    /// Whether `data` is one of the keys bound to `action`.
    pub fn matches(&self, data: &str, action: &str) -> bool {
        self.resolved
            .get(action)
            .is_some_and(|keys| keys.iter().any(|key| self.keys.matches(data, key)))
    }

    /// The keys bound to `action`.
    pub fn keys(&self, action: &str) -> &[String] {
        self.resolved.get(action).map_or(&[], Vec::as_slice)
    }

    /// The definitions, in declaration order.
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(entries: &[(&str, &[&str])]) -> UserBindings {
        entries
            .iter()
            .map(|(id, keys)| {
                (
                    (*id).to_owned(),
                    keys.iter().map(|key| (*key).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn user_bindings_replace_defaults() {
        let map = user(&[
            ("tui.editor.cursorUp", &["up", "ctrl+p", "up"]),
            ("tui.editor.undo", &[]),
            ("unknown.action", &["ctrl+q"]),
        ]);
        let bindings = Keybindings::new(Keys::default(), tui_definitions(), &map);
        assert_eq!(bindings.keys("tui.editor.cursorUp"), ["up", "ctrl+p"]);
        assert!(bindings.matches("\x10", "tui.editor.cursorUp"));
        assert!(bindings.keys("tui.editor.undo").is_empty());
        assert!(!bindings.matches("\x1f", "tui.editor.undo"));
        assert!(bindings.matches("\x1b[D", "tui.editor.cursorLeft"));
        assert!(bindings.keys("unknown.action").is_empty());
    }
}
