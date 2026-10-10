//! An editor in place of the built-in one, and a `/fragile` dialog, that
//! panic when their text is `panic`. A panic stops the extension's runtime:
//! yapi reports it and restarts the runtime, the dialog closes, and the
//! built-in editor takes the keys again. Components shown from
//! `session_start` stay gone until the next session.

use yapi_extension_api::{
    Api, Component, CustomOptions, Done, EditorComponent, editor_changed, editor_submit, parse_key,
};

/// One line of typed text, which panics as it renders `panic`.
#[derive(Default)]
struct Fragile {
    text: String,
    /// Ends the dialog it is; `None` for the editor.
    done: Option<Done<()>>,
}

impl Component for Fragile {
    fn render(&mut self, _width: usize) -> Vec<String> {
        assert!(self.text != "panic", "asked to panic");
        vec![format!("> {}", self.text)]
    }

    fn handle_input(&mut self, data: &str) {
        match (parse_key(data).as_deref(), &self.done) {
            (Some("enter" | "escape"), Some(done)) => return done.finish(()),
            (Some("enter"), None) => editor_submit(&std::mem::take(&mut self.text)),
            (Some("backspace"), _) => {
                self.text.pop();
            }
            (None, _) if !data.chars().any(char::is_control) => self.text.push_str(data),
            _ => return,
        }
        if self.done.is_none() {
            editor_changed(&self.text);
        }
    }
}

impl EditorComponent for Fragile {
    fn set_text(&mut self, text: &str) {
        text.clone_into(&mut self.text);
    }

    fn insert_text_at_cursor(&mut self, text: &str, _apart: bool) {
        self.text.push_str(text);
    }
}

fn init(api: &mut Api) {
    api.on("session_start", |_event, ctx| async move {
        ctx.set_editor_component(Some(Box::new(Fragile::default())));
        Ok(None)
    });
    api.register_command(
        "fragile",
        "Open a dialog that panics on `panic`",
        |_args, ctx| async move {
            let build = |done| Fragile {
                text: String::new(),
                done: Some(done),
            };
            ctx.custom(build, CustomOptions::default()).await;
            Ok(())
        },
    );
}

yapi_extension_api::extension!(init);
