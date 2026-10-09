//! Raw terminal input and editor completion: pi's `ctx.ui.onTerminalInput`
//! and `ctx.ui.addAutocompleteProvider`.
//!
//! Both run while yapi keeps drawing, as for Pi extensions in yapi:
//! listeners get the keys that arrive together in one call, and yapi asks
//! providers for suggestions in the background.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use serde_json::{Value, json};

use crate::task::LocalFuture;
use crate::{Context, op, request};

/// What a terminal input listener does with a key; pi's `{consume, data}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInput {
    /// Lets the key through unchanged, as pi's `undefined`.
    Pass,
    /// Stops the key, so later listeners and the editor never see it.
    Consume,
    /// Gives later listeners and the editor this input in place of the key.
    Replace(String),
}

type Listener = Rc<dyn Fn(&str) -> TerminalInput>;

#[derive(Default)]
struct Input {
    next: u64,
    listeners: Vec<(u64, Listener)>,
    /// Providers in the order they were added; each wraps those before it.
    providers: Vec<Rc<dyn Provider>>,
    triggers: Vec<String>,
}

thread_local! {
    static INPUT: RefCell<Input> = RefCell::default();
    /// Answer the `terminalInput` and `autocomplete` calls once a listener
    /// or a provider is added, so extensions without them leave out the
    /// code.
    pub(crate) static TERMINAL_INPUT: Cell<Option<Through>> = const { Cell::new(None) };
    pub(crate) static AUTOCOMPLETE: Cell<Option<Autocomplete>> = const { Cell::new(None) };
}

type Through = fn(&Value) -> Value;
type Autocomplete = fn(Value) -> LocalFuture<Result<Value, String>>;

/// The extension is about to load again for another session.
pub(crate) fn reset() {
    // Ids keep counting, so no earlier session's subscription stops a new
    // listener.
    let dropped = INPUT.with(|input| {
        let next = input.borrow().next;
        input.replace(Input {
            next,
            ..Input::default()
        })
    });
    drop(dropped);
}

/// A listener from [`Context::on_terminal_input`], which stops when this is
/// dropped; pi's unsubscribe function.
#[must_use = "dropping a Subscription stops its listener"]
pub struct Subscription(u64);

impl Subscription {
    /// Stops the listener.
    pub fn unsubscribe(self) {}
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let removed = INPUT.try_with(|input| {
            let listeners = &mut input.borrow_mut().listeners;
            let index = listeners.iter().position(|(id, _)| *id == self.0)?;
            Some((listeners.remove(index), listeners.is_empty()))
        });
        // The listener is dropped outside the borrow, so its `Drop` may use
        // the API.
        if let Ok(Some((_listener, true))) = removed {
            let _ = request("ui.setTerminalInput", &json!({"listening": false}));
        }
    }
}

/// The `terminalInput` call: each of `{"keys"}` through the listeners in the
/// order they were added, as pi-tui runs them; `null` for a consumed key.
fn terminal_input(payload: &Value) -> Value {
    let keys = payload["keys"].as_array().into_iter().flatten();
    let through = |key: &Value| {
        let mut data = key.as_str().unwrap_or_default().to_owned();
        // Read for each key, so a listener that stops stops at once.
        let listeners: Vec<Listener> = INPUT.with(|input| {
            let input = input.borrow();
            input.listeners.iter().map(|(_, l)| l.clone()).collect()
        });
        for listener in listeners {
            match listener(&data) {
                TerminalInput::Pass => {}
                TerminalInput::Consume => return Value::Null,
                TerminalInput::Replace(next) => data = next,
            }
        }
        Value::String(data)
    };
    Value::Array(keys.map(through).collect())
}

/// Editor lines and a cursor, as autocomplete providers get them. The
/// column is a byte offset into the cursor's line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorState {
    /// The editor's lines.
    pub lines: Vec<String>,
    /// The cursor's line.
    pub cursor_line: usize,
    /// The cursor's column in bytes.
    pub cursor_col: usize,
}

/// UTF-16 units before byte `column` of `line`, and back.
pub(crate) fn utf16(line: &str, column: usize) -> usize {
    line.get(..column)
        .map_or(0, |before| before.encode_utf16().count())
}

pub(crate) fn byte(line: &str, units: usize) -> usize {
    let mut seen = 0;
    line.char_indices()
        .find(|(_, char)| {
            seen += char.len_utf16();
            seen > units
        })
        .map_or(line.len(), |(index, _)| index)
}

impl EditorState {
    fn line(&self) -> &str {
        self.lines.get(self.cursor_line).map_or("", String::as_str)
    }

    /// From pi's `{"lines", "cursorLine", "cursorCol"}`, in UTF-16 units.
    fn from_js(value: &Value) -> EditorState {
        let mut state = EditorState {
            lines: serde_json::from_value(value["lines"].clone()).unwrap_or_default(),
            cursor_line: value["cursorLine"].as_u64().unwrap_or_default() as usize,
            cursor_col: 0,
        };
        state.cursor_col = byte(
            state.line(),
            value["cursorCol"].as_u64().unwrap_or_default() as usize,
        );
        state
    }

    fn to_js(&self) -> Value {
        json!({"lines": self.lines, "cursorLine": self.cursor_line, "cursorCol": utf16(self.line(), self.cursor_col)})
    }
}

/// pi-tui's `AutocompleteSuggestions`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Suggestions {
    /// pi's `AutocompleteItem`s, `{"value", "label", "description"}`, best
    /// first.
    pub items: Vec<Value>,
    /// The text before the cursor that they complete.
    pub prefix: String,
}

/// pi-tui's `AutocompleteProvider`, which [`Context::add_autocomplete_provider`]
/// adds over the providers before it.
pub trait AutocompleteProvider: 'static {
    /// Characters that open completion where a word starts, as `@` does;
    /// pi's `triggerCharacters`.
    fn trigger_characters(&self) -> Vec<String> {
        Vec::new()
    }

    /// pi's `getSuggestions`: the suggestions for `state`, or `None`.
    /// `force` when the user asked for completion with Tab. `current`
    /// gives the suggestions of the providers this one wraps, as pi's
    /// `current.getSuggestions` does.
    fn suggestions(
        &self,
        state: &EditorState,
        force: bool,
        current: &Current,
    ) -> impl Future<Output = Option<Suggestions>>;

    /// pi's `applyCompletion`: the editor after choosing `item`, which
    /// completes `prefix`. By default, as the providers this one wraps
    /// apply it.
    fn apply(
        &self,
        state: &EditorState,
        item: &Value,
        prefix: &str,
        current: &Current,
    ) -> EditorState {
        current.apply(state, item, prefix)
    }
}

/// [`AutocompleteProvider`] behind `dyn`.
trait Provider {
    fn suggestions(
        self: Rc<Self>,
        state: EditorState,
        force: bool,
        current: Current,
    ) -> LocalFuture<Option<Suggestions>>;

    fn apply(
        &self,
        state: &EditorState,
        item: &Value,
        prefix: &str,
        current: &Current,
    ) -> EditorState;
}

impl<P: AutocompleteProvider> Provider for P {
    fn suggestions(
        self: Rc<Self>,
        state: EditorState,
        force: bool,
        current: Current,
    ) -> LocalFuture<Option<Suggestions>> {
        Box::pin(
            async move { AutocompleteProvider::suggestions(&*self, &state, force, &current).await },
        )
    }

    fn apply(
        &self,
        state: &EditorState,
        item: &Value,
        prefix: &str,
        current: &Current,
    ) -> EditorState {
        AutocompleteProvider::apply(self, state, item, prefix, current)
    }
}

/// pi's `current`: the providers added before one, over yapi's built-in
/// completion of slash commands and paths.
#[derive(Clone, Debug)]
pub struct Current(usize);

impl Current {
    /// The provider this one wraps, `None` for the built-in completion.
    fn below(&self) -> Option<(Rc<dyn Provider>, Current)> {
        let index = self.0.checked_sub(1)?;
        let provider = INPUT.with(|input| input.borrow().providers.get(index).cloned())?;
        Some((provider, Current(index)))
    }

    /// What the wrapped providers suggest for `state`.
    pub async fn suggestions(&self, state: &EditorState, force: bool) -> Option<Suggestions> {
        if let Some((provider, current)) = self.below() {
            return provider.suggestions(state.clone(), force, current).await;
        }
        let mut payload = state.to_js();
        payload["force"] = json!(force);
        let answer = op("ui.suggestions", &payload).await.ok()?;
        Some(Suggestions {
            items: answer["items"].as_array()?.clone(),
            prefix: answer["prefix"].as_str()?.to_owned(),
        })
    }

    /// The editor after the wrapped providers apply `item`.
    pub fn apply(&self, state: &EditorState, item: &Value, prefix: &str) -> EditorState {
        if let Some((provider, current)) = self.below() {
            return provider.apply(state, item, prefix, &current);
        }
        let mut payload = state.to_js();
        payload["item"] = item.clone();
        payload["prefix"] = json!(prefix);
        EditorState::from_js(&request("ui.applyCompletion", &payload).unwrap_or_default())
    }
}

/// The `autocomplete` call: the providers' suggestions for the editor at
/// `{"lines", "cursorLine", "cursorCol", "force"}`, with what applying each
/// item gives, since yapi's editor applies one at once.
async fn autocomplete(payload: Value) -> Value {
    let state = EditorState::from_js(&payload);
    let top = Current(INPUT.with(|input| input.borrow().providers.len()));
    let Some(suggestions) = top
        .suggestions(&state, payload["force"] == true)
        .await
        .filter(|suggestions| !suggestions.items.is_empty())
    else {
        return Value::Null;
    };
    let applied: Vec<Value> = suggestions
        .items
        .iter()
        .map(|item| top.apply(&state, item, &suggestions.prefix).to_js())
        .collect();
    json!({"prefix": suggestions.prefix, "items": suggestions.items, "applied": applied})
}

impl Context {
    /// pi's `ctx.ui.onTerminalInput`: runs `listener` on raw terminal input
    /// before the editor and everything else sees it, until the returned
    /// [`Subscription`] is dropped. Listeners run in the order they were
    /// added. Modes that show no components never call it.
    pub fn on_terminal_input(
        &self,
        listener: impl Fn(&str) -> TerminalInput + 'static,
    ) -> Subscription {
        if !self.shows_components() {
            return Subscription(0);
        }
        TERMINAL_INPUT.set(Some(terminal_input));
        let (id, first) = INPUT.with(|input| {
            let mut input = input.borrow_mut();
            input.next += 1;
            let id = input.next;
            input.listeners.push((id, Rc::new(listener)));
            (id, input.listeners.len() == 1)
        });
        if first {
            let _ = request("ui.setTerminalInput", &json!({"listening": true}));
        }
        Subscription(id)
    }

    /// pi's `ctx.ui.addAutocompleteProvider`: completes in the editor
    /// through `provider`, which wraps the providers added before it. Modes
    /// that show no components leave it out.
    pub fn add_autocomplete_provider(&self, provider: impl AutocompleteProvider) {
        if !self.shows_components() {
            return;
        }
        AUTOCOMPLETE.set(Some(|payload| {
            Box::pin(async move { Ok(autocomplete(payload).await) })
        }));
        let triggers = INPUT.with(|input| {
            let mut input = input.borrow_mut();
            for trigger in provider.trigger_characters() {
                if !input.triggers.contains(&trigger) {
                    input.triggers.push(trigger);
                }
            }
            input.providers.push(Rc::new(provider));
            input.triggers.clone()
        });
        let _ = request(
            "ui.setAutocomplete",
            &json!({"triggerCharacters": triggers}),
        );
    }
}

/// The text in the editor, paste markers expanded; pi's
/// `ctx.ui.getEditorText`.
pub fn editor_text() -> String {
    match request("ui.getEditorText", &json!({})) {
        Ok(Value::String(text)) => text,
        _ => String::new(),
    }
}
