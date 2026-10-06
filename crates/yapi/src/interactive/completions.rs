//! Completion through extensions: pi's `addAutocompleteProvider`, whose
//! providers wrap the built-in one. The providers run in the extension
//! runtime, so the editor asks for their suggestions in the background, as
//! it does for `@` file search, and gets what applying each one gives with
//! them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yapi_core::extensions::ComponentHost;
use yapi_tui::autocomplete::{
    AutocompleteProvider, Completion, Suggestions, apply_completion, should_trigger_file_completion,
};
use yapi_tui::select_list::SelectItem;
use yapi_types::sync::lock;

/// Byte column `byte` of `line` in UTF-16 code units, as JavaScript counts.
fn utf16_col(line: &str, byte: usize) -> usize {
    line.char_indices()
        .take_while(|(index, _)| *index < byte)
        .map(|(_, c)| c.len_utf16())
        .sum()
}

/// The byte column of UTF-16 column `units` in `line`.
fn byte_col(line: &str, units: usize) -> usize {
    let mut counted = 0;
    for (index, c) in line.char_indices() {
        if counted >= units {
            return index;
        }
        counted += c.len_utf16();
    }
    line.len()
}

/// The lines and cursor of pi's provider arguments, in byte columns.
fn cursor(value: &Value) -> (Vec<String>, usize, usize) {
    let lines: Vec<String> = value["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    let line = value["cursorLine"].as_u64().unwrap_or(0) as usize;
    let units = value["cursorCol"].as_u64().unwrap_or(0) as usize;
    let col = byte_col(lines.get(line).map_or("", String::as_str), units);
    (lines, line, col)
}

/// pi's provider arguments for the cursor at byte column `col`.
fn arguments(lines: &[String], line: usize, col: usize, force: bool) -> Value {
    let units = utf16_col(lines.get(line).map_or("", String::as_str), col);
    json!({"lines": lines, "cursorLine": line, "cursorCol": units, "force": force})
}

fn item(value: &Value) -> SelectItem {
    let text = |key: &str| value[key].as_str().map(str::to_owned);
    let item_value = text("value").unwrap_or_default();
    SelectItem {
        label: text("label").unwrap_or_else(|| item_value.clone()),
        value: item_value,
        description: text("description"),
    }
}

fn completion(value: &Value) -> Option<Completion> {
    value.is_object().then(|| {
        let (lines, cursor_line, cursor_col) = cursor(value);
        Completion {
            lines,
            cursor_line,
            cursor_col,
        }
    })
}

/// The lines, cursor and `force` flag of a request for suggestions from an
/// extension, in byte columns.
pub(super) fn request(value: &Value) -> (Vec<String>, usize, usize, bool) {
    let (lines, line, col) = cursor(value);
    (lines, line, col, value["force"] == true)
}

/// Suggestions as pi's `AutocompleteSuggestions`.
pub(super) fn suggestions_json(suggestions: &Suggestions) -> Value {
    let items: Vec<Value> = suggestions
        .items
        .iter()
        .map(|item| json!({"value": item.value, "label": item.label, "description": item.description}))
        .collect();
    json!({"prefix": suggestions.prefix, "items": items})
}

/// The built-in provider's `applyCompletion` for pi's arguments `request`.
pub(super) fn apply(request: &Value) -> Value {
    let (lines, line, col) = cursor(request);
    let prefix = request["prefix"].as_str().unwrap_or_default();
    let applied = apply_completion(&lines, line, col, &item(&request["item"]), prefix);
    let units = utf16_col(
        applied
            .lines
            .get(applied.cursor_line)
            .map_or("", String::as_str),
        applied.cursor_col,
    );
    json!({"lines": applied.lines, "cursorLine": applied.cursor_line, "cursorCol": units})
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
        let arguments = arguments(lines, line, col, force);
        let (providers, last, notify) = (
            self.providers.clone(),
            self.last.clone(),
            self.notify.clone(),
        );
        tokio::spawn(async move {
            let value = providers.suggestions(arguments).await;
            let answered = value["items"]
                .as_array()
                .filter(|items| !items.is_empty())
                .map(|items| {
                    let suggestions = Suggestions {
                        items: items.iter().map(item).collect(),
                        prefix: value["prefix"].as_str().unwrap_or_default().to_owned(),
                    };
                    let applied = (0..items.len())
                        .map(|index| completion(&value["applied"][index]))
                        .collect();
                    (suggestions, applied)
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
    fn columns_convert_between_bytes_and_utf16() {
        let line = "é😀x";
        assert_eq!(utf16_col(line, 2), 1);
        assert_eq!(utf16_col(line, 6), 3);
        assert_eq!(byte_col(line, 3), 6);
        assert_eq!(byte_col(line, 9), line.len());
    }

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
