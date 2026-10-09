//! pi-tui's widgets for components, with the `widgets` feature: yapi's own
//! ports of them in [`tui`], such as [`tui::select_list::SelectList`],
//! [`tui::editor::Editor`], [`tui::text_input::TextInput`] and the `Text` and
//! `Box` layouts in [`tui::lines`]. They render [`Line`]s styled with
//! [`style`], which [`to_ansi`] turns into a component's lines.
//! [`CustomEditor`] is pi's `CustomEditor`, an editor for
//! [`Context::set_editor_component`](crate::Context::set_editor_component).

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Value, json};
pub use yapi_tui as tui;
use yapi_tui::ansi::{CURSOR_MARKER, line_to_ansi, parse_line};
use yapi_tui::autocomplete::{AutocompleteProvider, Completion, Suggestions};
use yapi_tui::editor::{Editor, EditorEvent, EditorTheme};
use yapi_tui::keybindings::{Keybindings, UserBindings, tui_definitions};
use yapi_tui::keys::Keys;
use yapi_tui::ratatui_core::style::Style;
use yapi_tui::ratatui_core::text::{Line, Span};
use yapi_tui::screen::MouseKind;
use yapi_tui::select_list::SelectItem;

use crate::input::{byte, utf16};
use crate::ui::{Component, EditorComponent};
use crate::{
    editor_action, editor_changed, editor_shortcut, editor_submit, op, request, request_render,
    spawn, theme,
};

/// The session's key bindings: pi-tui's, as the user bound them, and the
/// app actions an editor runs.
struct Bindings {
    tui: Rc<Keybindings>,
    all: UserBindings,
    actions: Vec<String>,
}

thread_local! {
    static BINDINGS: RefCell<Option<Rc<Bindings>>> = const { RefCell::new(None) };
}

/// A new session, whose key bindings [`keybindings`] asks for.
pub(crate) fn bind() {
    BINDINGS.with(|bindings| bindings.take());
}

fn bindings() -> Rc<Bindings> {
    BINDINGS.with(|bindings| {
        bindings
            .borrow_mut()
            .get_or_insert_with(|| {
                let spec = request("ui.keybindings", &json!({})).unwrap_or_default();
                let all: UserBindings =
                    serde_json::from_value(spec["bindings"].clone()).unwrap_or_default();
                let keys = Keys {
                    kitty: spec["kitty"] == true,
                    windows_terminal: false,
                };
                Rc::new(Bindings {
                    tui: Rc::new(Keybindings::new(keys, tui_definitions(), &all)),
                    actions: serde_json::from_value(spec["actions"].clone()).unwrap_or_default(),
                    all,
                })
            })
            .clone()
    })
}

/// The key bindings the widgets' `handle_input` takes: pi-tui's, as the user
/// bound them.
pub fn keybindings() -> Rc<Keybindings> {
    bindings().tui.clone()
}

/// Theme token `token`'s foreground as a style for the widgets' themes, such
/// as `SelectListTheme::from_theme(style)`. The default style without a
/// theme.
pub fn style(token: &str) -> Style {
    style_of(&theme().fg(token, " "))
}

/// Theme token `token`'s background as a style, such as a box's.
pub fn bg_style(token: &str) -> Style {
    style_of(&theme().bg(token, " "))
}

fn style_of(styled: &str) -> Style {
    parse(styled)
        .spans
        .first()
        .map(|span| span.style)
        .unwrap_or_default()
}

/// `text`, with escape sequences and newlines, as a styled line for the
/// widgets, which break it at the newlines.
pub fn parse(text: &str) -> Line<'static> {
    parse_line(text).0
}

/// pi-tui's `Text`: text wrapped to the width within `px` columns of
/// padding on each side, with `py` blank rows above and below.
pub struct Text {
    /// The text, with escape sequences and newlines.
    pub text: String,
    /// Columns of padding left and right.
    pub px: usize,
    /// Blank rows above and below.
    pub py: usize,
}

impl Text {
    /// pi-tui's `new Text(text, px, py)`.
    pub fn new(text: impl Into<String>, px: usize, py: usize) -> Text {
        Text {
            text: text.into(),
            px,
            py,
        }
    }
}

impl Component for Text {
    fn render(&mut self, width: usize) -> Vec<String> {
        to_ansi(
            &tui::lines::text(&[parse(&self.text)], width, self.px, self.py, None),
            None,
        )
    }
}

/// pi-tui's `wrapTextWithAnsi`: `text` wrapped to `width` columns, styles
/// carried across the breaks.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    to_ansi(&tui::lines::wrap(&parse(text), width), None)
}

/// `lines` as a component's lines, with pi-tui's cursor marker at `cursor`,
/// a row and column such as [`Editor::cursor_position`] gives.
pub fn to_ansi(lines: &[Line<'_>], cursor: Option<(usize, usize)>) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .map(|(row, line)| match cursor {
            Some((cursor_row, column)) if cursor_row == row => {
                let (before, after) = tui::lines::split_at(line, column);
                let mut spans = before.spans;
                spans.push(Span::raw(CURSOR_MARKER));
                spans.extend(after.spans);
                line_to_ansi(&Line::from(spans))
            }
            _ => line_to_ansi(line),
        })
        .collect()
}

/// pi-tui's `visibleWidth`: the columns `text` takes, escape sequences
/// aside.
pub fn visible_width(text: &str) -> usize {
    tui::lines::width(&parse(text))
}

/// pi-tui's `truncateToWidth`: `text`, escape sequences kept, cut to `width`
/// columns with `ellipsis` in place of what was cut.
pub fn truncate_to_width(text: &str, width: usize, ellipsis: &str) -> String {
    line_to_ansi(&tui::lines::truncate(&parse(text), width, ellipsis))
}

/// A mouse event as the widgets' `mouse` methods take it: the kind, the
/// column and the row. `None` for buttons other than the left one.
pub fn mouse_event(event: &Value) -> Option<(MouseKind, usize, usize)> {
    let kind = match event["type"].as_str()? {
        "press" if event["button"] == "left" => MouseKind::Press,
        "click" if event["button"] == "left" => MouseKind::Click,
        "wheel" => MouseKind::Wheel(event["wheelDelta"].as_i64()?.signum() as isize),
        _ => return None,
    };
    let at = |key: &str| event[key].as_u64().map(|value| value as usize);
    Some((kind, at("x")?, at("y")?))
}

/// The built-in editor's completions, such as slash commands and paths,
/// which yapi computes in the background: `ui.suggestions` and
/// `ui.applyCompletion` with pi's UTF-16 columns.
#[derive(Clone, Default)]
struct HostCompletions(Rc<RefCell<Asked>>);

#[derive(Default)]
struct Asked {
    request: Value,
    answer: Option<Value>,
    waiting: bool,
    /// An answer arrived that the editor has not asked for again.
    arrived: bool,
}

impl AutocompleteProvider for HostCompletions {
    fn suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<Suggestions> {
        let current = lines.get(line).map_or("", String::as_str);
        let request = json!({"lines": lines, "cursorLine": line, "cursorCol": utf16(current, col), "force": force});
        let mut asked = self.0.borrow_mut();
        if asked.request == request && !asked.waiting {
            let answer = asked.answer.clone()?;
            return Some(Suggestions {
                items: answer["items"]
                    .as_array()?
                    .iter()
                    .map(|item| SelectItem {
                        value: item["value"].as_str().unwrap_or_default().to_owned(),
                        label: item["label"].as_str().unwrap_or_default().to_owned(),
                        description: item["description"].as_str().map(str::to_owned),
                    })
                    .collect(),
                prefix: answer["prefix"].as_str()?.to_owned(),
            });
        }
        *asked = Asked {
            request: request.clone(),
            waiting: true,
            ..Asked::default()
        };
        let shared = self.0.clone();
        spawn(async move {
            let answer = op("ui.suggestions", &request).await.ok();
            let mut asked = shared.borrow_mut();
            if asked.request == request {
                *asked = Asked {
                    request,
                    answer,
                    waiting: false,
                    arrived: true,
                };
                drop(asked);
                request_render();
            }
        });
        None
    }

    fn apply(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> Completion {
        let current = lines.get(line).map_or("", String::as_str);
        let item =
            json!({"value": item.value, "label": item.label, "description": item.description});
        let payload = json!({"lines": lines, "cursorLine": line, "cursorCol": utf16(current, col), "item": item, "prefix": prefix});
        let answer = request("ui.applyCompletion", &payload).unwrap_or_default();
        let lines: Vec<String> =
            serde_json::from_value(answer["lines"].clone()).unwrap_or_default();
        let cursor_line = answer["cursorLine"].as_u64().unwrap_or_default() as usize;
        let units = answer["cursorCol"].as_u64().unwrap_or_default() as usize;
        let cursor_col = byte(lines.get(cursor_line).map_or("", String::as_str), units);
        Completion {
            lines,
            cursor_line,
            cursor_col,
        }
    }

    fn pending(&self) -> bool {
        self.0.borrow().waiting
    }
}

/// pi's `CustomEditor`: the built-in editor's behavior, app keys included,
/// for [`Context::set_editor_component`](crate::Context::set_editor_component).
/// Wrap it to change how it handles keys or renders, as pi's examples extend
/// it.
pub struct CustomEditor {
    /// The editor itself.
    pub editor: Editor,
    bindings: Rc<Bindings>,
    completions: HostCompletions,
}

impl Default for CustomEditor {
    fn default() -> Self {
        CustomEditor::new()
    }
}

impl CustomEditor {
    /// An empty editor in the session's theme and key bindings.
    pub fn new() -> CustomEditor {
        let completions = HostCompletions::default();
        let mut editor = Editor::new(EditorTheme::from_theme(style), 0, 5);
        editor.set_autocomplete(Box::new(completions.clone()));
        CustomEditor {
            editor,
            bindings: bindings(),
            completions,
        }
    }

    fn matches(&self, data: &str, action: &str) -> bool {
        let keys = self.bindings.tui.decoder();
        let bound = self
            .bindings
            .all
            .get(action)
            .map(Vec::as_slice)
            .unwrap_or_default();
        bound.iter().any(|key| keys.matches(data, key))
    }

    /// Hands the key to the editor, reporting its new text or submission.
    fn edit(&mut self, data: &str) {
        let before = self.editor.expanded_text();
        if let EditorEvent::Submit(text) = self.editor.handle_input(data, &self.bindings.tui) {
            editor_submit(&text);
        }
        let after = self.editor.expanded_text();
        if after != before {
            editor_changed(&after);
        }
    }
}

impl Component for CustomEditor {
    fn render(&mut self, width: usize) -> Vec<String> {
        if std::mem::take(&mut self.completions.0.borrow_mut().arrived) {
            self.editor.refresh_autocomplete();
        }
        let lines = self.editor.render(width);
        to_ansi(&lines, self.editor.cursor_position())
    }

    /// pi's `CustomEditor.handleInput`: extension shortcuts, then app keys,
    /// then the editor.
    fn handle_input(&mut self, data: &str) {
        if editor_shortcut(data) {
            return;
        }
        if self.matches(data, "app.clipboard.pasteImage") {
            return editor_action("app.clipboard.pasteImage");
        }
        if self.matches(data, "app.interrupt") {
            if !self.editor.is_showing_autocomplete() {
                return editor_action("app.interrupt");
            }
            return self.edit(data);
        }
        if self.matches(data, "app.exit") && self.editor.text().is_empty() {
            return editor_action("app.exit");
        }
        let tui = &self.bindings.tui;
        if tui.matches(data, "tui.editor.historyPrevious")
            || tui.matches(data, "tui.editor.historyNext")
        {
            return self.edit(data);
        }
        let bindings = self.bindings.clone();
        let action = bindings.actions.iter().find(|action| {
            !matches!(action.as_str(), "app.interrupt" | "app.exit") && self.matches(data, action)
        });
        match action {
            Some(action) => editor_action(action),
            None => self.edit(data),
        }
    }

    fn handle_mouse(&mut self, event: &Value) -> bool {
        mouse_event(event).is_some_and(|(kind, x, y)| self.editor.mouse(kind, x, y))
    }
}

impl EditorComponent for CustomEditor {
    fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
        editor_changed(&self.editor.expanded_text());
    }

    fn add_to_history(&mut self, text: &str) {
        self.editor.add_to_history(text);
    }

    /// Sets the text apart at the editor's own cursor, as pi's
    /// `handleClipboardPaste` does.
    fn insert_text_at_cursor(&mut self, text: &str, apart: bool) {
        let space = |char: Option<char>| match char {
            Some(char) if apart && !char.is_whitespace() => " ",
            _ => "",
        };
        let (line, column) = self.editor.cursor();
        let line = self.editor.lines().get(line).cloned().unwrap_or_default();
        let (before, after) = line.split_at(column.min(line.len()));
        let text = format!(
            "{}{text}{}",
            space(before.chars().next_back()),
            space(after.chars().next())
        );
        self.editor.insert_text_at_cursor(&text);
        editor_changed(&self.editor.expanded_text());
    }

    fn configure(&mut self, config: &Value) {
        // `bashMode`, or a thinking level's color, such as `thinkingHigh`.
        let level = config["border"].as_str().filter(|level| !level.is_empty());
        let level = level.unwrap_or("off");
        let token = if level == "bashMode" {
            level.to_owned()
        } else {
            let (first, rest) = level.split_at(level.chars().next().map_or(0, char::len_utf8));
            format!("thinking{}{rest}", first.to_uppercase())
        };
        self.editor.border = style(&token);
        let number = |key: &str| config[key].as_u64().map(|value| value as usize);
        if let Some(padding) = number("paddingX") {
            self.editor.set_padding_x(padding);
        }
        if let Some(rows) = number("autocompleteMaxVisible") {
            self.editor.set_autocomplete_max_visible(rows);
        }
        if let Some(rows) = number("rows") {
            self.editor.set_terminal_rows(rows);
        }
        self.editor.focused = config["focused"] == true;
    }
}
