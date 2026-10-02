//! A single-line text input with horizontal scrolling.
//!
//! Port of `packages/tui/src/components/input.ts` in pi `v1.0.0`. The cursor is
//! a byte offset into the value.

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::keybindings::Keybindings;
use crate::keys::decode_printable;
use crate::kill_ring::KillRing;
use crate::segment::{find_word_backward, find_word_forward};
use crate::text::{grapheme_width, is_js_whitespace, truncate_to_width, visible_width};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// What a key did to the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// The value or cursor may have changed.
    None,
    /// Enter was pressed.
    Submit(String),
    /// The cancel key was pressed.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// A one-line input.
#[derive(Clone, Debug)]
pub struct TextInput {
    value: String,
    cursor: usize,
    prompt: String,
    /// The terminal cursor is placed here when rendering.
    pub focused: bool,
    paste: Option<String>,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo: Vec<(String, usize)>,
    rendered_cursor: Option<usize>,
}

impl Default for TextInput {
    fn default() -> TextInput {
        TextInput::new("> ")
    }
}

/// The graphemes of `text` that lie fully within columns `start..start + len`.
fn slice_columns(text: &str, start: usize, len: usize) -> &str {
    let mut column = 0;
    let mut from = None;
    let mut to = text.len();
    for (offset, grapheme) in text.grapheme_indices(true) {
        let width = grapheme_width(grapheme);
        if from.is_none() && column >= start {
            from = Some(offset);
        }
        if from.is_some() && column + width > start + len {
            to = offset;
            break;
        }
        column += width;
    }
    let from = from.unwrap_or(text.len());
    &text[from..to.max(from)]
}

impl TextInput {
    /// An empty input shown after `prompt`.
    pub fn new(prompt: &str) -> TextInput {
        TextInput {
            value: String::new(),
            cursor: 0,
            prompt: prompt.to_owned(),
            focused: false,
            paste: None,
            kill_ring: KillRing::default(),
            last_action: None,
            undo: Vec::new(),
            rendered_cursor: None,
        }
    }

    /// The text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replaces the text; the cursor stays where it was, within the text.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_owned();
        self.cursor = self.cursor.min(self.value.len());
        while !self.value.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    /// Handles one key or paste.
    pub fn handle_input(&mut self, data: &str, keybindings: &Keybindings) -> InputEvent {
        let mut data = data.to_owned();
        if let Some(index) = data.find(PASTE_START) {
            data.replace_range(index..index + PASTE_START.len(), "");
            self.paste = Some(String::new());
        }
        if let Some(buffer) = &mut self.paste {
            buffer.push_str(&data);
            if let Some(end) = buffer.find(PASTE_END) {
                let content = buffer[..end].to_owned();
                let remaining = buffer[end + PASTE_END.len()..].to_owned();
                self.paste = None;
                self.handle_paste(&content);
                if !remaining.is_empty() {
                    return self.handle_input(&remaining, keybindings);
                }
            }
            return InputEvent::None;
        }
        let data = data.as_str();
        let kb = keybindings;
        if kb.matches(data, "tui.select.cancel") {
            return InputEvent::Cancel;
        }
        if kb.matches(data, "tui.editor.undo") {
            if let Some((value, cursor)) = self.undo.pop() {
                self.value = value;
                self.cursor = cursor;
                self.last_action = None;
            }
            return InputEvent::None;
        }
        if kb.matches(data, "tui.input.submit") || data == "\n" {
            return InputEvent::Submit(self.value.clone());
        }
        if kb.matches(data, "tui.editor.deleteCharBackward") {
            self.last_action = None;
            if self.cursor > 0 {
                self.push_undo();
                let len = self.previous_grapheme_len();
                self.value.replace_range(self.cursor - len..self.cursor, "");
                self.cursor -= len;
            }
        } else if kb.matches(data, "tui.editor.deleteCharForward") {
            self.last_action = None;
            if self.cursor < self.value.len() {
                self.push_undo();
                let len = self.next_grapheme_len();
                self.value.replace_range(self.cursor..self.cursor + len, "");
            }
        } else if kb.matches(data, "tui.editor.deleteWordBackward") {
            if self.cursor > 0 {
                let accumulate = self.last_action == Some(LastAction::Kill);
                self.push_undo();
                let from = find_word_backward(&self.value, self.cursor, &[]);
                let killed = self.value[from..self.cursor].to_owned();
                self.kill_ring.push(&killed, true, accumulate);
                self.last_action = Some(LastAction::Kill);
                self.value.replace_range(from..self.cursor, "");
                self.cursor = from;
            }
        } else if kb.matches(data, "tui.editor.deleteWordForward") {
            if self.cursor < self.value.len() {
                let accumulate = self.last_action == Some(LastAction::Kill);
                self.push_undo();
                let to = find_word_forward(&self.value, self.cursor, &[]);
                let killed = self.value[self.cursor..to].to_owned();
                self.kill_ring.push(&killed, false, accumulate);
                self.last_action = Some(LastAction::Kill);
                self.value.replace_range(self.cursor..to, "");
            }
        } else if kb.matches(data, "tui.editor.deleteToLineStart") {
            if self.cursor > 0 {
                self.push_undo();
                let killed = self.value[..self.cursor].to_owned();
                self.kill_ring
                    .push(&killed, true, self.last_action == Some(LastAction::Kill));
                self.last_action = Some(LastAction::Kill);
                self.value.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
        } else if kb.matches(data, "tui.editor.deleteToLineEnd") {
            if self.cursor < self.value.len() {
                self.push_undo();
                let killed = self.value[self.cursor..].to_owned();
                self.kill_ring
                    .push(&killed, false, self.last_action == Some(LastAction::Kill));
                self.last_action = Some(LastAction::Kill);
                self.value.truncate(self.cursor);
            }
        } else if kb.matches(data, "tui.editor.yank") {
            if let Some(text) = self.kill_ring.peek().map(str::to_owned) {
                self.push_undo();
                self.value.insert_str(self.cursor, &text);
                self.cursor += text.len();
                self.last_action = Some(LastAction::Yank);
            }
        } else if kb.matches(data, "tui.editor.yankPop") {
            if self.last_action == Some(LastAction::Yank) && self.kill_ring.len() > 1 {
                self.push_undo();
                let previous = self.kill_ring.peek().unwrap_or_default().len();
                let start = self.cursor.saturating_sub(previous);
                self.value.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.kill_ring.rotate();
                let text = self.kill_ring.peek().unwrap_or_default().to_owned();
                self.value.insert_str(self.cursor, &text);
                self.cursor += text.len();
                self.last_action = Some(LastAction::Yank);
            }
        } else if kb.matches(data, "tui.editor.cursorLeft") {
            self.last_action = None;
            if self.cursor > 0 {
                self.cursor -= self.previous_grapheme_len();
            }
        } else if kb.matches(data, "tui.editor.cursorRight") {
            self.last_action = None;
            if self.cursor < self.value.len() {
                self.cursor += self.next_grapheme_len();
            }
        } else if kb.matches(data, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.cursor = 0;
        } else if kb.matches(data, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            self.cursor = self.value.len();
        } else if kb.matches(data, "tui.editor.cursorWordLeft") {
            if self.cursor > 0 {
                self.last_action = None;
                self.cursor = find_word_backward(&self.value, self.cursor, &[]);
            }
        } else if kb.matches(data, "tui.editor.cursorWordRight") {
            if self.cursor < self.value.len() {
                self.last_action = None;
                self.cursor = find_word_forward(&self.value, self.cursor, &[]);
            }
        } else if let Some(printable) = decode_printable(data) {
            self.insert(&printable.to_string());
        } else if !data.chars().any(|c| {
            let code = c as u32;
            code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
        }) {
            self.insert(data);
        }
        InputEvent::None
    }

    fn previous_grapheme_len(&self) -> usize {
        self.value[..self.cursor]
            .graphemes(true)
            .next_back()
            .map_or(1, str::len)
    }

    fn next_grapheme_len(&self) -> usize {
        self.value[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(1, str::len)
    }

    fn push_undo(&mut self) {
        self.undo.push((self.value.clone(), self.cursor));
    }

    fn insert(&mut self, text: &str) {
        if text.chars().any(is_js_whitespace) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn handle_paste(&mut self, text: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = text
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// The terminal cursor column in the last render, when focused.
    pub fn cursor_column(&self) -> Option<usize> {
        self.rendered_cursor
    }

    /// The input's single row at `width`, with a reverse-video cursor.
    pub fn render(&mut self, width: usize) -> Line<'static> {
        let prompt_width = visible_width(&self.prompt);
        self.rendered_cursor = None;
        let Some(available) = width.checked_sub(prompt_width).filter(|w| *w > 0) else {
            return Line::from(truncate_to_width(&self.prompt, width, "", false));
        };
        let total = visible_width(&self.value);
        let (visible, cursor) = if total < available {
            (self.value.as_str(), self.cursor)
        } else {
            let scroll = if self.cursor == self.value.len() {
                available - 1
            } else {
                available
            };
            let cursor_col = visible_width(&self.value[..self.cursor]);
            if scroll > 0 {
                let half = scroll / 2;
                let start = if cursor_col < half {
                    0
                } else if cursor_col > total.saturating_sub(half) {
                    total.saturating_sub(scroll)
                } else {
                    cursor_col - half
                };
                let visible = slice_columns(&self.value, start, scroll);
                let before = slice_columns(&self.value, start, cursor_col.saturating_sub(start));
                (visible, before.len().min(visible.len()))
            } else {
                ("", 0)
            }
        };
        let before = &visible[..cursor];
        let rest = &visible[cursor..];
        let at = rest.graphemes(true).next().unwrap_or(" ");
        let after = rest.get(at.len()..).unwrap_or("");
        if self.focused {
            self.rendered_cursor = Some(prompt_width + visible_width(before));
        }
        let used = visible_width(before) + visible_width(at) + visible_width(after);
        Line::from(vec![
            Span::raw(self.prompt.clone()),
            Span::raw(before.to_owned()),
            Span::styled(at.to_owned(), Style::new().add_modifier(Modifier::REVERSED)),
            Span::raw(after.to_owned()),
            Span::raw(" ".repeat(available.saturating_sub(used))),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{Keybindings, UserBindings, tui_definitions};
    use crate::keys::Keys;

    fn kb() -> Keybindings {
        Keybindings::new(
            Keys::default(),
            tui_definitions(),
            &UserBindings::new(),
            &[],
        )
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn edits_and_submits() {
        let kb = kb();
        let mut input = TextInput::default();
        for key in ["h", "e", "y", " ", "x"] {
            input.handle_input(key, &kb);
        }
        input.handle_input("\x7f", &kb);
        assert_eq!(input.value(), "hey ");
        input.handle_input("\x17", &kb);
        assert_eq!(input.value(), "");
        input.handle_input("\x19", &kb);
        assert_eq!(input.value(), "hey ");
        assert_eq!(
            input.handle_input("\r", &kb),
            InputEvent::Submit("hey ".into())
        );
        assert_eq!(input.handle_input("\x1b", &kb), InputEvent::Cancel);
    }

    #[test]
    fn pastes_one_line() {
        let kb = kb();
        let mut input = TextInput::default();
        input.handle_input("\x1b[200~a\nb\tc\x1b[201~", &kb);
        assert_eq!(input.value(), "ab    c");
    }

    #[test]
    fn scrolls_to_the_cursor() {
        let kb = kb();
        let mut input = TextInput::new("> ");
        for c in "abcdefghijklmnop".chars() {
            input.handle_input(&c.to_string(), &kb);
        }
        let line = input.render(10);
        assert_eq!(text(&line), "> jklmnop ");
        assert_eq!(line.width(), 10);
    }
}
