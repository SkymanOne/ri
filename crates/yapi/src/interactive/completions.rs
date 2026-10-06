//! Completion through extensions: pi's `addAutocompleteProvider`, whose
//! providers wrap the built-in one. The providers run in the extension
//! runtime, so the editor asks for their suggestions in the background, as
//! it does for `@` file search, and gets what applying each one gives with
//! them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use yapi_core::extensions::ComponentHost;
use yapi_tui::autocomplete::{
    AutocompleteProvider, Completion, Suggestions, apply_completion, should_trigger_file_completion,
};
use yapi_tui::select_list::SelectItem;
use yapi_types::autocomplete::{AutocompleteItem, AutocompleteSuggestions, EditorState};
use yapi_types::js::{byte_index, utf16_index};
use yapi_types::sync::lock;

/// An editor state from JavaScript, in byte columns.
fn from_js(state: EditorState) -> Completion {
    let line = state
        .lines
        .get(state.cursor_line)
        .map_or("", String::as_str);
    Completion {
        cursor_col: byte_index(line, state.cursor_col),
        cursor_line: state.cursor_line,
        lines: state.lines,
    }
}

/// The editor state at byte column `col`, for JavaScript.
fn to_js(lines: &[String], line: usize, col: usize) -> EditorState {
    EditorState {
        lines: lines.to_vec(),
        cursor_line: line,
        cursor_col: utf16_index(lines.get(line).map_or("", String::as_str), col),
    }
}

/// The editor state and `force` flag of an extension's request for
/// suggestions, in byte columns; `None` when it is malformed.
pub(super) fn request(value: &Value) -> Option<(Completion, bool)> {
    let state = serde_json::from_value(value.clone()).ok()?;
    Some((from_js(state), value["force"] == true))
}

/// Suggestions as pi's `AutocompleteSuggestions`.
pub(super) fn suggestions_json(suggestions: Suggestions) -> Value {
    let suggestions = AutocompleteSuggestions {
        items: suggestions
            .items
            .into_iter()
            .map(AutocompleteItem::from)
            .collect(),
        prefix: suggestions.prefix,
    };
    serde_json::to_value(suggestions).unwrap_or(Value::Null)
}

/// The built-in provider's `applyCompletion` for an extension's request:
/// pi's provider arguments with `item` and `prefix`.
pub(super) fn apply(request: &Value) -> Value {
    let (Ok(state), Ok(item)) = (
        serde_json::from_value::<EditorState>(request.clone()),
        serde_json::from_value::<AutocompleteItem>(request["item"].clone()),
    ) else {
        return Value::Null;
    };
    let at = from_js(state);
    let prefix = request["prefix"].as_str().unwrap_or_default();
    let applied = apply_completion(
        &at.lines,
        at.cursor_line,
        at.cursor_col,
        &item.into(),
        prefix,
    );
    serde_json::to_value(to_js(
        &applied.lines,
        applied.cursor_line,
        applied.cursor_col,
    ))
    .unwrap_or(Value::Null)
}

/// What the extension runtime answers the editor: the composed providers'
/// suggestions and what applying each one gives.
#[derive(Deserialize)]
struct Answer {
    #[serde(flatten)]
    suggestions: AutocompleteSuggestions,
    #[serde(default)]
    applied: Vec<Option<EditorState>>,
}

/// The editor's position when it asked: lines, cursor and `force`.
type Key = (Vec<String>, usize, usize, bool);

/// The extensions' suggestions with what applying each item gives.
type Answered = Option<(Suggestions, Vec<Option<Completion>>)>;

/// The editor's last request, and its answer once it came.
struct Request {
    key: Key,
    answer: Option<Answered>,
}

/// The editor's provider while extensions add providers: their composition
/// in the JS runtime.
pub(super) struct ExtensionCompletions {
    providers: Arc<dyn ComponentHost>,
    triggers: Vec<char>,
    notify: Arc<dyn Fn() + Send + Sync>,
    last: Arc<Mutex<Option<Request>>>,
    pending: AtomicBool,
}

impl ExtensionCompletions {
    /// Asks `providers` and calls `notify` when an answer arrives; `triggers`
    /// open completion as `@` does.
    pub fn new(
        providers: Arc<dyn ComponentHost>,
        triggers: Vec<char>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> ExtensionCompletions {
        ExtensionCompletions {
            providers,
            triggers,
            notify,
            last: Arc::default(),
            pending: AtomicBool::new(false),
        }
    }
}

impl AutocompleteProvider for ExtensionCompletions {
    fn suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<Suggestions> {
        let key: Key = (lines.to_vec(), line, col, force);
        {
            let mut last = lock(&self.last);
            if let Some(request) = last.as_ref().filter(|request| request.key == key) {
                self.pending
                    .store(request.answer.is_none(), Ordering::Relaxed);
                return request
                    .answer
                    .clone()
                    .flatten()
                    .map(|(suggestions, _)| suggestions);
            }
            *last = Some(Request {
                key: key.clone(),
                answer: None,
            });
        }
        self.pending.store(true, Ordering::Relaxed);
        let mut arguments = serde_json::to_value(to_js(lines, line, col)).unwrap_or(Value::Null);
        arguments["force"] = json!(force);
        let (providers, last, notify) = (
            self.providers.clone(),
            self.last.clone(),
            self.notify.clone(),
        );
        tokio::spawn(async move {
            let value = providers.suggestions(arguments).await;
            let answered = serde_json::from_value::<Answer>(value)
                .ok()
                .filter(|answer| !answer.suggestions.items.is_empty())
                .map(|answer| {
                    let suggestions = Suggestions {
                        items: answer
                            .suggestions
                            .items
                            .into_iter()
                            .map(SelectItem::from)
                            .collect(),
                        prefix: answer.suggestions.prefix,
                    };
                    let applied = answer.applied.into_iter().map(|state| state.map(from_js));
                    (suggestions, applied.collect())
                });
            // A later request replaced this one.
            match lock(&last).as_mut() {
                Some(request) if request.key == key => request.answer = Some(answered),
                _ => return,
            }
            notify();
        });
        None
    }

    /// What the extensions' providers gave for `item` when they suggested
    /// it here, else the built-in provider's.
    fn apply(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> Completion {
        if let Some(Request {
            key,
            answer: Some(Some((suggestions, applied))),
        }) = lock(&self.last).as_ref()
            && key.0 == lines
            && (key.1, key.2) == (line, col)
            && let Some(Some(completion)) = suggestions
                .items
                .iter()
                .position(|suggested| {
                    suggested.value == item.value && suggested.label == item.label
                })
                .and_then(|index| applied.get(index))
        {
            return completion.clone();
        }
        apply_completion(lines, line, col, item, prefix)
    }

    fn should_trigger_file_completion(&self, lines: &[String], line: usize, col: usize) -> bool {
        should_trigger_file_completion(lines, line, col)
    }

    fn trigger_characters(&self) -> Vec<char> {
        self.triggers.clone()
    }

    fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_answers_in_utf16_columns() {
        let request = json!({
            "lines": ["é $P"], "cursorLine": 0, "cursorCol": 4,
            "item": {"value": "$PATH", "label": "$PATH"}, "prefix": "$P",
        });
        assert_eq!(
            apply(&request),
            json!({"lines": ["é $PATH"], "cursorLine": 0, "cursorCol": 7})
        );
    }
}
