//! Hooks into the keyboard and the editor's completion.
//!
//! A terminal input listener sees every key before the editor does. In an
//! empty prompt editor `?` shows help instead of being typed, while dialogs
//! and selectors still get it. `a` becomes `A`, and Ctrl+G reports how many
//! keys it saw and what the editor holds. `/quiet` stops it. Typing `$`
//! completes environment variable names, and Alt+K reports the count too.

use std::cell::{Cell, RefCell};

use yapi_extension_api::{
    Api, AutocompleteProvider, Current, EditorState, Subscription, Suggestions, TerminalInput,
    editor_focused, editor_text, json, notify, parse_key,
};

const VARIABLES: [&str; 3] = ["$HOME", "$PATH", "$PWD"];

thread_local! {
    static LISTENER: RefCell<Option<Subscription>> = const { RefCell::new(None) };
    static SEEN: Cell<usize> = const { Cell::new(0) };
}

fn listen(data: &str) -> TerminalInput {
    SEEN.set(SEEN.get() + 1);
    if data == "?" && editor_focused() && editor_text().is_empty() {
        notify("Type $ for variables, Alt+K to count keys", "info");
        return TerminalInput::Consume;
    }
    if data == "a" {
        return TerminalInput::Replace("A".into());
    }
    if parse_key(data).as_deref() == Some("ctrl+g") {
        let message = format!("Saw {} keys, editor: {}", SEEN.get(), editor_text());
        notify(&message, "info");
        return TerminalInput::Consume;
    }
    TerminalInput::Pass
}

/// Completes `$NAME` at the cursor, and leaves everything else to the
/// providers it wraps.
struct Variables;

impl AutocompleteProvider for Variables {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["$".into()]
    }

    async fn suggestions(
        &self,
        state: &EditorState,
        force: bool,
        current: &Current,
    ) -> Option<Suggestions> {
        let line = state
            .lines
            .get(state.cursor_line)
            .map_or("", String::as_str);
        let before = &line[..state.cursor_col];
        // A `$` that starts a word, and the name typed after it.
        let name = before.rfind('$').and_then(|at| {
            let name = &before[at + 1..];
            let word = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            let starts = before[..at]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace);
            (word && starts).then_some(name)
        });
        let Some(name) = name else {
            return current.suggestions(state, force).await;
        };
        let items: Vec<_> = VARIABLES
            .iter()
            .filter(|variable| variable[1..].starts_with(name))
            .map(|variable| json!({"value": variable, "label": variable, "description": "environment variable"}))
            .collect();
        (!items.is_empty()).then(|| Suggestions {
            items,
            prefix: format!("${name}"),
        })
    }
}

fn init(api: &mut Api) {
    api.on("session_start", |_event, ctx| async move {
        SEEN.set(0);
        LISTENER.set(Some(ctx.on_terminal_input(listen)));
        ctx.add_autocomplete_provider(Variables);
        Ok(None)
    });
    api.register_command("quiet", "Stop listening", |_args, _ctx| async move {
        // Dropping the subscription stops the listener.
        LISTENER.take();
        notify("Stopped listening", "info");
        Ok(())
    });
    api.register_shortcut("alt+k", "Count keys", |_ctx| async move {
        notify(&format!("Saw {} keys", SEEN.get()), "info");
        Ok(())
    });
}

yapi_extension_api::extension!(init);
