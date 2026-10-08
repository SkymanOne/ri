//! A vim-like modal editor in place of the built-in one. Escape switches
//! from insert to normal mode, where `hjkl` move the cursor, `0` and `$` go
//! to the line's start and end, `x` deletes, and `i` and `a` switch back.
//! Escape in normal mode and keys such as Ctrl+C act as in the built-in
//! editor.
//!
//! A port of pi's `modal-editor.ts`. pi's `ModalEditor` extends
//! `CustomEditor`, and this one wraps the SDK's `CustomEditor`.

use yapi_extension_api::widgets::{CustomEditor, truncate_to_width, visible_width};
use yapi_extension_api::{Api, Component, EditorComponent, Value, parse_key};

/// Normal mode's keys: the key the editor gets instead, or `None` for a mode
/// switch.
const NORMAL_KEYS: [(&str, Option<&str>); 9] = [
    ("h", Some("\x1b[D")),
    ("j", Some("\x1b[B")),
    ("k", Some("\x1b[A")),
    ("l", Some("\x1b[C")),
    ("0", Some("\x01")),
    ("$", Some("\x05")),
    ("x", Some("\x1b[3~")),
    ("i", None),
    ("a", None),
];

#[derive(Default)]
struct ModalEditor {
    editor: CustomEditor,
    normal: bool,
}

impl Component for ModalEditor {
    fn handle_input(&mut self, data: &str) {
        // Escape switches to normal mode, or acts as in the built-in editor.
        if parse_key(data).as_deref() == Some("escape") {
            if self.normal {
                self.editor.handle_input(data);
            } else {
                self.normal = true;
            }
            return;
        }
        if !self.normal {
            return self.editor.handle_input(data);
        }
        if let Some((key, sequence)) = NORMAL_KEYS.iter().find(|(key, _)| *key == data) {
            match (*key, sequence) {
                ("i", _) => self.normal = false,
                ("a", _) => {
                    self.normal = false;
                    self.editor.handle_input("\x1b[C");
                }
                (_, Some(sequence)) => self.editor.handle_input(sequence),
                _ => {}
            }
            return;
        }
        // Control keys act as in the built-in editor. Text is ignored.
        let mut chars = data.chars();
        if let (Some(char), None) = (chars.next(), chars.next())
            && char >= ' '
            && char.len_utf16() == 1
        {
            return;
        }
        self.editor.handle_input(data);
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.editor.render(width);
        // The mode on the bottom border.
        let label = if self.normal { " NORMAL " } else { " INSERT " };
        if let Some(last) = lines.last_mut()
            && visible_width(last) >= label.len()
        {
            *last = truncate_to_width(last, width - label.len(), "") + label;
        }
        lines
    }

    fn handle_mouse(&mut self, event: &Value) -> bool {
        self.editor.handle_mouse(event)
    }
}

impl EditorComponent for ModalEditor {
    fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
    }

    fn add_to_history(&mut self, text: &str) {
        self.editor.add_to_history(text);
    }

    fn insert_text_at_cursor(&mut self, text: &str, apart: bool) {
        self.editor.insert_text_at_cursor(text, apart);
    }

    fn configure(&mut self, config: &Value) {
        self.editor.configure(config);
    }
}

fn init(api: &mut Api) {
    api.on("session_start", |_event, ctx| async move {
        ctx.set_editor_component(Some(Box::new(ModalEditor::default())));
        Ok(None)
    });
}

yapi_extension_api::extension!(init);
