//! The multi-line prompt editor.
//!
//! Port of `packages/tui/src/components/editor.ts` in pi `v1.0.0`: word-aware
//! wrapping, sticky-column vertical motion, history, an Emacs kill ring,
//! fish-style undo grouping, large-paste markers, character jumps and
//! autocomplete. Cursor columns are byte offsets into the current line.

use std::collections::BTreeMap;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::autocomplete::{AutocompleteProvider, Completion};
use crate::keybindings::Keybindings;
use crate::keys::decode_printable;
use crate::kill_ring::KillRing;
use crate::segment::{
    Granularity, find_word_backward, find_word_forward, is_paste_marker, parse_paste_marker,
    paste_marker_spans, segment,
};
use crate::select_list::{SelectEvent, SelectItem, SelectList, SelectListLayout, SelectListTheme};
use crate::text::{
    has_cjk, has_whitespace, is_autocomplete_separator, take_width, utf16_len, visible_width,
};

const HISTORY_LIMIT: usize = 100;
const LARGE_PASTE_LINES: usize = 10;
const LARGE_PASTE_CHARS: usize = 1000;
const SLASH_COMMAND_LAYOUT: SelectListLayout = SelectListLayout {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
};
const DEFAULT_TRIGGERS: [char; 2] = ['@', '#'];

/// Styles for the editor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorTheme {
    /// The horizontal borders.
    pub border: Style,
    /// The autocomplete list.
    pub select_list: SelectListTheme,
}

/// What a key did to the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// The key was handled, or ignored.
    None,
    /// The user submitted this text, with paste markers expanded and trimmed.
    Submit(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

#[derive(Clone, Debug)]
struct Snapshot {
    state: State,
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutocompleteMode {
    Regular,
    Force,
}

#[derive(Clone, Copy, Debug)]
struct VisualLine {
    logical: usize,
    start: usize,
    len: usize,
}

struct LayoutLine {
    text: String,
    cursor: Option<usize>,
}

/// A chunk of a wrapped line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    /// The chunk's text.
    pub text: String,
    /// Byte offset of the chunk in the line.
    pub start: usize,
    /// Byte offset just past the chunk.
    pub end: usize,
}

/// Wraps `line` into chunks of at most `max_width` columns, breaking after
/// whitespace or beside CJK characters where possible. Paste markers with an
/// id in `valid_ids` stay whole unless wider than a line.
pub fn word_wrap_line(line: &str, max_width: usize, valid_ids: &[u32]) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start: 0,
            end: 0,
        }];
    }
    if visible_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_owned(),
            start: 0,
            end: line.len(),
        }];
    }
    let segments = segment(line, Granularity::Grapheme, valid_ids);
    let mut chunks = Vec::new();
    let mut current_width = 0;
    let mut chunk_start = 0;
    let mut wrap_at: Option<(usize, usize)> = None;
    for (position, seg) in segments.iter().enumerate() {
        let width = visible_width(seg.text);
        let marker = is_paste_marker(seg.text);
        let is_space = !marker && has_whitespace(seg.text);
        if current_width + width > max_width {
            match wrap_at {
                Some((index, wrap_width)) if current_width - wrap_width + width <= max_width => {
                    chunks.push(TextChunk {
                        text: line[chunk_start..index].to_owned(),
                        start: chunk_start,
                        end: index,
                    });
                    chunk_start = index;
                    current_width -= wrap_width;
                }
                _ if chunk_start < seg.index => {
                    chunks.push(TextChunk {
                        text: line[chunk_start..seg.index].to_owned(),
                        start: chunk_start,
                        end: seg.index,
                    });
                    chunk_start = seg.index;
                    current_width = 0;
                }
                _ => {}
            }
            wrap_at = None;
        }
        if width > max_width {
            let sub = word_wrap_line(seg.text, max_width, &[]);
            for chunk in &sub[..sub.len() - 1] {
                chunks.push(TextChunk {
                    text: chunk.text.clone(),
                    start: seg.index + chunk.start,
                    end: seg.index + chunk.end,
                });
            }
            let last = &sub[sub.len() - 1];
            chunk_start = seg.index + last.start;
            current_width = visible_width(&last.text);
            wrap_at = None;
            continue;
        }
        current_width += width;
        if let Some(next) = segments.get(position + 1) {
            let next_marker = is_paste_marker(next.text);
            let next_space = has_whitespace(next.text);
            // Break after whitespace, or beside CJK characters.
            let after_space = is_space && (next_marker || !next_space);
            let beside_cjk = !is_space
                && !next_space
                && ((!marker && has_cjk(seg.text)) || (!next_marker && has_cjk(next.text)));
            if after_space || beside_cjk {
                wrap_at = Some((next.index, current_width));
            }
        }
    }
    chunks.push(TextChunk {
        text: line[chunk_start..].to_owned(),
        start: chunk_start,
        end: line.len(),
    });
    chunks
}

fn scroll_border(arrow: char, hidden: usize, width: usize) -> String {
    let label = format!(" {arrow} {hidden} more ");
    let label_width = visible_width(&label);
    if label_width + 2 <= width {
        let left = (width - label_width) / 2;
        return format!(
            "{}{label}{}",
            "─".repeat(left),
            "─".repeat(width - left - label_width)
        );
    }
    let indicator = format!("─── {arrow} {hidden} more ");
    let indicator_width = visible_width(&indicator);
    if indicator_width <= width {
        return format!("{indicator}{}", "─".repeat(width - indicator_width));
    }
    let ellipsis = &"..."[..width.min(3)];
    format!(
        "{}{ellipsis}",
        take_width(&indicator, width - ellipsis.len())
    )
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

/// Undoes control characters that tmux re-encodes as `ESC [ code ; 5 u` inside
/// a paste.
fn decode_pasted_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let decoded = (digits > 0 && after[digits..].starts_with(";5u"))
            .then(|| after[..digits].parse::<u32>().ok())
            .flatten()
            .and_then(|code| match code {
                97..=122 => char::from_u32(code - 96),
                65..=90 => char::from_u32(code - 64),
                _ => None,
            });
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[digits + 3..];
            }
            None => {
                out.push_str("\x1b[");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The largest char boundary of `text` at or below `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn to_utf16(text: &str, byte: usize) -> usize {
    utf16_len(&text[..floor_boundary(text, byte)])
}

fn from_utf16(text: &str, units: usize) -> usize {
    let mut count = 0;
    for (index, c) in text.char_indices() {
        if count >= units {
            return index;
        }
        count += c.len_utf16();
    }
    text.len()
}

/// The prompt editor.
pub struct Editor {
    state: State,
    padding_x: usize,
    last_width: usize,
    terminal_rows: usize,
    scroll_offset: usize,
    theme: EditorTheme,
    /// Border style, which callers change with the thinking level or bash mode.
    pub border: Style,
    autocomplete: Option<Box<dyn AutocompleteProvider>>,
    triggers: Vec<char>,
    autocomplete_mode: Option<AutocompleteMode>,
    autocomplete_list: Option<SelectList>,
    autocomplete_prefix: String,
    autocomplete_max_visible: usize,
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
    in_paste: bool,
    paste_buffer: String,
    history: Vec<String>,
    history_index: isize,
    history_draft: Option<State>,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    jump: Option<Jump>,
    preferred_visual_col: Option<usize>,
    snapped_from_cursor_col: Option<usize>,
    undo_stack: Vec<Snapshot>,
    cursor: Option<(usize, usize)>,
    /// Enter does nothing while set.
    pub disable_submit: bool,
    /// Whether the editor has focus, which shows the terminal cursor position.
    pub focused: bool,
}

impl Editor {
    /// An empty editor.
    pub fn new(theme: EditorTheme, padding_x: usize, autocomplete_max_visible: usize) -> Editor {
        Editor {
            state: State {
                lines: vec![String::new()],
                cursor_line: 0,
                cursor_col: 0,
            },
            padding_x,
            last_width: 80,
            terminal_rows: 24,
            scroll_offset: 0,
            border: theme.border,
            theme,
            autocomplete: None,
            triggers: DEFAULT_TRIGGERS.to_vec(),
            autocomplete_mode: None,
            autocomplete_list: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible: autocomplete_max_visible.clamp(3, 20),
            pastes: BTreeMap::new(),
            paste_counter: 0,
            in_paste: false,
            paste_buffer: String::new(),
            history: Vec::new(),
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::default(),
            last_action: None,
            jump: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: Vec::new(),
            cursor: None,
            disable_submit: false,
            focused: true,
        }
    }

    /// Sets the terminal height, which bounds the editor to 30% of it.
    pub fn set_terminal_rows(&mut self, rows: usize) {
        self.terminal_rows = rows;
    }

    /// Sets the horizontal padding.
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
    }

    /// Sets how many autocomplete rows show at once (3 to 20).
    pub fn set_autocomplete_max_visible(&mut self, rows: usize) {
        self.autocomplete_max_visible = rows.clamp(3, 20);
    }

    /// Installs the autocomplete provider and its extra trigger characters.
    pub fn set_autocomplete(&mut self, provider: Box<dyn AutocompleteProvider>) {
        self.cancel_autocomplete();
        let mut triggers = DEFAULT_TRIGGERS.to_vec();
        for c in provider.trigger_characters() {
            if c != '/' && !crate::text::is_js_whitespace(c) && !triggers.contains(&c) {
                triggers.push(c);
            }
        }
        self.triggers = triggers;
        self.autocomplete = Some(provider);
    }

    /// The text, with paste markers unexpanded.
    pub fn text(&self) -> String {
        self.state.lines.join("\n")
    }

    /// The text with paste markers replaced by their content.
    pub fn expanded_text(&self) -> String {
        self.expand_paste_markers(&self.text())
    }

    /// The logical lines.
    pub fn lines(&self) -> &[String] {
        &self.state.lines
    }

    /// The cursor as (line, byte column).
    pub fn cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    /// Whether the autocomplete list is open.
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_mode.is_some()
    }

    /// Where the terminal cursor belongs in the last render, as (row, column).
    pub fn cursor_position(&self) -> Option<(usize, usize)> {
        self.cursor
    }

    /// Adds a submitted prompt to the history, skipping blanks and repeats.
    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() || self.history.first().is_some_and(|first| first == trimmed) {
            return;
        }
        self.history.insert(0, trimmed.to_owned());
        self.history.truncate(HISTORY_LIMIT);
    }

    /// Replaces the text, as an undoable change, and clears paste markers.
    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history();
        let normalized = normalize(text);
        if self.text() != normalized {
            self.push_undo();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, false);
    }

    /// Inserts `text` at the cursor as one undoable change.
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo();
        self.last_action = None;
        self.exit_history();
        self.insert_text(text);
    }

    fn valid_ids(&self) -> Vec<u32> {
        self.pastes.keys().copied().collect()
    }

    fn current_line(&self) -> &str {
        self.state
            .lines
            .get(self.state.cursor_line)
            .map_or("", String::as_str)
    }

    fn before_cursor(&self) -> &str {
        let line = self.current_line();
        &line[..floor_boundary(line, self.state.cursor_col)]
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = floor_boundary(self.current_line(), col);
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    fn is_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    fn set_text_internal(&mut self, text: &str, cursor_at_start: bool) {
        self.state.lines = text.split('\n').map(str::to_owned).collect();
        self.state.cursor_line = if cursor_at_start {
            0
        } else {
            self.state.lines.len() - 1
        };
        let col = if cursor_at_start {
            0
        } else {
            self.current_line().len()
        };
        self.set_cursor_col(col);
        self.scroll_offset = 0;
    }

    fn exit_history(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }

    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let index = self.history_index - direction;
        if index < -1 || index >= self.history.len() as isize {
            return;
        }
        if self.history_index == -1 && index >= 0 {
            self.push_undo();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = index;
        if index == -1 {
            match self.history_draft.take() {
                Some(draft) => {
                    self.state = draft;
                    self.preferred_visual_col = None;
                    self.snapped_from_cursor_col = None;
                    self.scroll_offset = 0;
                }
                None => self.set_text_internal("", false),
            }
        } else {
            let entry = self.history[index as usize].clone();
            self.set_text_internal(&entry, direction == -1);
        }
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(Snapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.exit_history();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_owned();
        for (id, content) in &self.pastes {
            let mut out = String::with_capacity(result.len());
            let mut copied = 0;
            for (marker_id, start, end) in paste_marker_spans(&result) {
                if marker_id == *id {
                    out.push_str(&result[copied..start]);
                    out.push_str(content);
                    copied = end;
                }
            }
            out.push_str(&result[copied..]);
            result = out;
        }
        result
    }

    // Rendering

    fn layout(&self, width: usize) -> Vec<LayoutLine> {
        if self.is_empty() {
            return vec![LayoutLine {
                text: String::new(),
                cursor: Some(0),
            }];
        }
        let ids = self.valid_ids();
        let mut out = Vec::new();
        for (index, line) in self.state.lines.iter().enumerate() {
            let current = index == self.state.cursor_line;
            if visible_width(line) <= width {
                out.push(LayoutLine {
                    text: line.clone(),
                    cursor: current.then_some(self.state.cursor_col),
                });
                continue;
            }
            let chunks = word_wrap_line(line, width, &ids);
            let last = chunks.len() - 1;
            for (position, chunk) in chunks.into_iter().enumerate() {
                let col = self.state.cursor_col;
                let cursor = if !current {
                    None
                } else if position == last {
                    (col >= chunk.start).then(|| col - chunk.start)
                } else {
                    (col >= chunk.start && col < chunk.end)
                        .then(|| (col - chunk.start).min(chunk.text.len()))
                };
                out.push(LayoutLine {
                    text: chunk.text,
                    cursor,
                });
            }
        }
        out
    }

    /// The editor's rows for `width` columns: a border, the visible text, a
    /// border, then the autocomplete list when open.
    pub fn render(&mut self, width: usize) -> Vec<Line<'static>> {
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = content_width
            .saturating_sub(if padding_x > 0 { 0 } else { 1 })
            .max(1);
        self.last_width = layout_width;
        let layout = self.layout(layout_width);
        let max_visible = (self.terminal_rows * 3 / 10).max(5);
        let cursor_index = layout
            .iter()
            .position(|line| line.cursor.is_some())
            .unwrap_or(0);
        if cursor_index < self.scroll_offset {
            self.scroll_offset = cursor_index;
        } else if cursor_index >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_index + 1 - max_visible;
        }
        self.scroll_offset = self
            .scroll_offset
            .min(layout.len().saturating_sub(max_visible));
        let visible =
            &layout[self.scroll_offset..(self.scroll_offset + max_visible).min(layout.len())];

        let mut out = Vec::with_capacity(visible.len() + 2);
        let top = if self.scroll_offset > 0 {
            scroll_border('↑', self.scroll_offset, width)
        } else {
            "─".repeat(width)
        };
        out.push(Line::from(Span::styled(top, self.border)));
        let padding = " ".repeat(padding_x);
        let ids = self.valid_ids();
        self.cursor = None;
        for line in visible {
            let mut spans = vec![Span::raw(padding.clone())];
            let mut line_width = visible_width(&line.text);
            let mut cursor_in_padding = false;
            match line.cursor {
                Some(position) => {
                    let position = floor_boundary(&line.text, position);
                    let (before, after) = line.text.split_at(position);
                    if self.focused {
                        self.cursor = Some((out.len(), padding_x + visible_width(before)));
                    }
                    spans.push(Span::raw(before.to_owned()));
                    let reversed = Style::new().add_modifier(Modifier::REVERSED);
                    if after.is_empty() {
                        spans.push(Span::styled(" ", reversed));
                        line_width += 1;
                        cursor_in_padding = line_width > content_width && padding_x > 0;
                    } else {
                        let first = segment(after, Granularity::Grapheme, &ids)
                            .first()
                            .map_or("", |seg| seg.text);
                        spans.push(Span::styled(first.to_owned(), reversed));
                        spans.push(Span::raw(after[first.len()..].to_owned()));
                    }
                }
                None => spans.push(Span::raw(line.text.clone())),
            }
            spans.push(Span::raw(
                " ".repeat(content_width.saturating_sub(line_width)),
            ));
            let right = if cursor_in_padding {
                padding_x - 1
            } else {
                padding_x
            };
            spans.push(Span::raw(" ".repeat(right)));
            out.push(Line::from(spans));
        }
        let below = layout.len() - (self.scroll_offset + visible.len());
        let bottom = if below > 0 {
            scroll_border('↓', below, width)
        } else {
            "─".repeat(width)
        };
        out.push(Line::from(Span::styled(bottom, self.border)));
        if self.autocomplete_mode.is_some()
            && let Some(list) = &self.autocomplete_list
        {
            for line in list.render(content_width) {
                let line_width = line.width();
                let mut spans = vec![Span::raw(padding.clone())];
                spans.extend(line.spans);
                spans.push(Span::raw(
                    " ".repeat(content_width.saturating_sub(line_width)),
                ));
                spans.push(Span::raw(padding.clone()));
                out.push(Line::from(spans));
            }
        }
        out
    }

    // Input

    /// Handles one key sequence, or a paste wrapped in bracketed-paste markers.
    pub fn handle_input(&mut self, data: &str, keybindings: &Keybindings) -> EditorEvent {
        let keys = keybindings.decoder();
        if let Some(direction) = self.jump {
            if keybindings.matches(data, "tui.editor.jumpForward")
                || keybindings.matches(data, "tui.editor.jumpBackward")
            {
                self.jump = None;
                return EditorEvent::None;
            }
            let printable = decode_printable(data).or_else(|| {
                data.chars()
                    .next()
                    .filter(|c| *c >= ' ')
                    .map(|_| data.to_owned())
            });
            self.jump = None;
            if let Some(target) = printable {
                self.jump_to(&target, direction);
                return EditorEvent::None;
            }
        }

        let mut data = data.to_owned();
        if data.contains("\x1b[200~") {
            self.in_paste = true;
            self.paste_buffer.clear();
            data = data.replacen("\x1b[200~", "", 1);
        }
        if self.in_paste {
            self.paste_buffer.push_str(&data);
            if let Some(end) = self.paste_buffer.find("\x1b[201~") {
                let content = self.paste_buffer[..end].to_owned();
                let rest = self.paste_buffer[end + 6..].to_owned();
                self.in_paste = false;
                self.paste_buffer.clear();
                if !content.is_empty() {
                    self.handle_paste(&content);
                }
                if !rest.is_empty() {
                    return self.handle_input(&rest, keybindings);
                }
            }
            return EditorEvent::None;
        }
        let data = data.as_str();

        if keybindings.matches(data, "tui.input.copy") {
            return EditorEvent::None;
        }
        if keybindings.matches(data, "tui.editor.undo") {
            self.undo();
            return EditorEvent::None;
        }

        if self.autocomplete_mode.is_some() && self.autocomplete_list.is_some() {
            if keybindings.matches(data, "tui.select.cancel") {
                self.cancel_autocomplete();
                return EditorEvent::None;
            }
            if keybindings.matches(data, "tui.select.up")
                || keybindings.matches(data, "tui.select.down")
            {
                if let Some(list) = &mut self.autocomplete_list {
                    list.handle_input(data, keybindings);
                }
                return EditorEvent::None;
            }
            if keybindings.matches(data, "tui.input.tab") {
                if let Some(item) = self.selected_completion() {
                    self.apply_completion(&item);
                    self.cancel_autocomplete();
                }
                return EditorEvent::None;
            }
            if keybindings.matches(data, "tui.select.confirm")
                && let Some(item) = self.selected_completion()
            {
                let slash = self.autocomplete_prefix.starts_with('/');
                self.apply_completion(&item);
                self.cancel_autocomplete();
                if !slash {
                    return EditorEvent::None;
                }
                // A chosen slash command is submitted at once.
            }
        }

        if keybindings.matches(data, "tui.input.tab") && self.autocomplete_mode.is_none() {
            self.handle_tab();
            return EditorEvent::None;
        }

        if keybindings.matches(data, "tui.editor.deleteToLineEnd") {
            self.delete_to_line_end();
        } else if keybindings.matches(data, "tui.editor.deleteToLineStart") {
            self.delete_to_line_start();
        } else if keybindings.matches(data, "tui.editor.deleteWordBackward") {
            self.delete_word_backward();
        } else if keybindings.matches(data, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
        } else if keybindings.matches(data, "tui.editor.deleteCharBackward")
            || keys.matches(data, "shift+backspace")
        {
            self.backspace();
        } else if keybindings.matches(data, "tui.editor.deleteCharForward")
            || keys.matches(data, "shift+delete")
        {
            self.forward_delete();
        } else if keybindings.matches(data, "tui.editor.yank") {
            self.yank();
        } else if keybindings.matches(data, "tui.editor.yankPop") {
            self.yank_pop();
        } else if keybindings.matches(data, "tui.editor.historyPrevious") {
            self.cancel_autocomplete();
            self.navigate_history(-1);
        } else if keybindings.matches(data, "tui.editor.historyNext") {
            self.cancel_autocomplete();
            self.navigate_history(1);
        } else if keybindings.matches(data, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.set_cursor_col(0);
        } else if keybindings.matches(data, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            let end = self.current_line().len();
            self.set_cursor_col(end);
        } else if keybindings.matches(data, "tui.editor.cursorWordLeft") {
            self.word_left();
        } else if keybindings.matches(data, "tui.editor.cursorWordRight") {
            self.word_right();
        } else if keybindings.matches(data, "tui.input.newLine")
            || (data.starts_with('\n') && data.len() > 1)
            || data == "\x1b\r"
            || data == "\x1b[13;2~"
            || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'))
            || data == "\n"
        {
            if self.submit_on_backslash_enter(data, keybindings) {
                self.backspace();
                return self.submit();
            }
            self.new_line();
        } else if keybindings.matches(data, "tui.input.submit") {
            if self.disable_submit {
                return EditorEvent::None;
            }
            if self.before_cursor().ends_with('\\') {
                self.backspace();
                self.new_line();
                return EditorEvent::None;
            }
            return self.submit();
        } else if keybindings.matches(data, "tui.editor.cursorUp") {
            let first = self.on_first_visual_line();
            if first && (self.is_empty() || self.history_index > -1 || self.state.cursor_col == 0) {
                self.navigate_history(-1);
            } else if first {
                self.last_action = None;
                self.set_cursor_col(0);
            } else {
                self.move_cursor(-1, 0);
            }
        } else if keybindings.matches(data, "tui.editor.cursorDown") {
            let last = self.on_last_visual_line();
            if self.history_index > -1 && last {
                self.navigate_history(1);
            } else if last {
                self.last_action = None;
                let end = self.current_line().len();
                self.set_cursor_col(end);
            } else {
                self.move_cursor(1, 0);
            }
        } else if keybindings.matches(data, "tui.editor.cursorRight") {
            self.move_cursor(0, 1);
        } else if keybindings.matches(data, "tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
        } else if keybindings.matches(data, "tui.editor.pageUp") {
            self.page_scroll(-1);
        } else if keybindings.matches(data, "tui.editor.pageDown") {
            self.page_scroll(1);
        } else if keybindings.matches(data, "tui.editor.jumpForward") {
            self.jump = Some(Jump::Forward);
        } else if keybindings.matches(data, "tui.editor.jumpBackward") {
            self.jump = Some(Jump::Backward);
        } else if keys.matches(data, "shift+space") {
            self.insert_character(" ", false);
        } else if let Some(printable) = decode_printable(data) {
            self.insert_character(&printable, false);
        } else if data.chars().next().is_some_and(|c| c >= ' ') {
            self.insert_character(data, false);
        }
        EditorEvent::None
    }

    fn submit_on_backslash_enter(&self, data: &str, keybindings: &Keybindings) -> bool {
        if self.disable_submit || !keybindings.decoder().matches(data, "enter") {
            return false;
        }
        let submit = keybindings.keys("tui.input.submit");
        submit
            .iter()
            .any(|key| key == "shift+enter" || key == "shift+return")
            && self.before_cursor().ends_with('\\')
    }

    fn submit(&mut self) -> EditorEvent {
        self.cancel_autocomplete();
        let result = self.expanded_text().trim().to_owned();
        self.state = State {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        };
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        EditorEvent::Submit(result)
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize(text);
        let inserted: Vec<&str> = normalized.split('\n').collect();
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        let (before, after) = line.split_at(col);
        if inserted.len() == 1 {
            self.state.lines[self.state.cursor_line] = format!("{before}{normalized}{after}");
            self.set_cursor_col(col + normalized.len());
            return;
        }
        let last = inserted[inserted.len() - 1];
        let mut replacement = vec![format!("{before}{}", inserted[0])];
        replacement.extend(
            inserted[1..inserted.len() - 1]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        replacement.push(format!("{last}{after}"));
        let line_index = self.state.cursor_line;
        self.state
            .lines
            .splice(line_index..=line_index, replacement);
        self.state.cursor_line += inserted.len() - 1;
        self.set_cursor_col(last.len());
    }

    fn insert_character(&mut self, text: &str, skip_undo_coalescing: bool) {
        self.exit_history();
        if !skip_undo_coalescing {
            if has_whitespace(text) || self.last_action != Some(LastAction::TypeWord) {
                self.push_undo();
            }
            self.last_action = Some(LastAction::TypeWord);
        }
        let col = floor_boundary(self.current_line(), self.state.cursor_col);
        let line = self.state.cursor_line;
        self.state.lines[line].insert_str(col, text);
        self.set_cursor_col(col + text.len());

        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
            return;
        }
        let before = self.before_cursor().to_owned();
        let c = text.chars().next().unwrap_or_default();
        if text == "/" && self.at_start_of_message() {
            self.request_autocomplete(false, false);
        } else if text.chars().count() == 1 && self.triggers.contains(&c) {
            if self.matches_trigger(&before) {
                self.request_autocomplete(false, false);
            }
        } else if ((text.chars().count() == 1
            && (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
            || has_cjk(text))
            && (self.in_slash_command(&before) || self.matches_trigger(&before))
        {
            self.request_autocomplete(false, false);
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.exit_history();
        self.last_action = None;
        self.push_undo();
        let cleaned = normalize(&decode_pasted_controls(pasted));
        let mut filtered: String = cleaned
            .chars()
            .filter(|c| *c == '\n' || *c >= ' ')
            .collect();
        if filtered.starts_with(['/', '~', '.'])
            && self
                .before_cursor()
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            filtered.insert(0, ' ');
        }
        let line_count = filtered.split('\n').count();
        let chars = utf16_len(&filtered);
        if line_count > LARGE_PASTE_LINES || chars > LARGE_PASTE_CHARS {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.insert(id, filtered);
            let marker = if line_count > LARGE_PASTE_LINES {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {chars} chars]")
            };
            self.insert_text(&marker);
            return;
        }
        self.insert_text(&filtered);
    }

    fn new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history();
        self.last_action = None;
        self.push_undo();
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        let (before, after) = line.split_at(col);
        let index = self.state.cursor_line;
        self.state.lines[index] = before.to_owned();
        self.state.lines.insert(index + 1, after.to_owned());
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
    }

    fn merge_with_previous_line(&mut self) {
        let current = self.state.lines.remove(self.state.cursor_line);
        self.state.cursor_line -= 1;
        let previous_len = self.current_line().len();
        self.state.lines[self.state.cursor_line].push_str(&current);
        self.set_cursor_col(previous_len);
    }

    fn merge_with_next_line(&mut self) {
        let next = self.state.lines.remove(self.state.cursor_line + 1);
        self.state.lines[self.state.cursor_line].push_str(&next);
    }

    fn backspace(&mut self) {
        self.exit_history();
        self.last_action = None;
        if self.state.cursor_col > 0 {
            self.push_undo();
            let ids = self.valid_ids();
            let before = self.before_cursor().to_owned();
            let last = segment(&before, Granularity::Grapheme, &ids)
                .last()
                .map(|seg| seg.text.to_owned())
                .unwrap_or_default();
            let length = if last.is_empty() { 1 } else { last.len() };
            if let Some((target, len)) = parse_paste_marker(&last)
                && len == last.len()
            {
                self.remove_paste(target);
            }
            let line = self.current_line().to_owned();
            let col = floor_boundary(&line, self.state.cursor_col);
            let start = floor_boundary(&line, col.saturating_sub(length));
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &line[..start], &line[col..]);
            self.set_cursor_col(start);
        } else if self.state.cursor_line > 0 {
            self.push_undo();
            self.merge_with_previous_line();
        }
        self.refresh_autocomplete_after_delete();
    }

    /// Drops paste `target` and renumbers the markers above it.
    fn remove_paste(&mut self, target: u32) {
        self.pastes.remove(&target);
        self.paste_counter = self.paste_counter.saturating_sub(1);
        let higher: Vec<u32> = self.pastes.range(target + 1..).map(|(id, _)| *id).collect();
        for id in higher {
            if let Some(content) = self.pastes.remove(&id) {
                self.pastes.insert(id - 1, content);
            }
        }
        for line in &mut self.state.lines {
            let spans = paste_marker_spans(line);
            if spans.iter().all(|(id, _, _)| *id <= target) {
                continue;
            }
            let mut out = String::with_capacity(line.len());
            let mut copied = 0;
            for (id, start, end) in spans {
                if id <= target {
                    continue;
                }
                out.push_str(&line[copied..start]);
                let marker = &line[start..end];
                let digits = id.to_string();
                let suffix = &marker["[paste #".len() + digits.len()..];
                out.push_str(&format!("[paste #{}{suffix}", id - 1));
                copied = end;
            }
            out.push_str(&line[copied..]);
            *line = out;
        }
    }

    fn forward_delete(&mut self) {
        self.exit_history();
        self.last_action = None;
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        if col < line.len() {
            self.push_undo();
            let ids = self.valid_ids();
            let first = segment(&line[col..], Granularity::Grapheme, &ids)
                .first()
                .map_or(1, |seg| seg.text.len());
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &line[..col], &line[col + first..]);
        } else if self.state.cursor_line + 1 < self.state.lines.len() {
            self.push_undo();
            self.merge_with_next_line();
        }
        self.refresh_autocomplete_after_delete();
    }

    fn refresh_autocomplete_after_delete(&mut self) {
        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
            return;
        }
        let before = self.before_cursor().to_owned();
        if self.in_slash_command(&before) || self.matches_trigger(&before) {
            self.request_autocomplete(false, false);
        }
    }

    fn delete_to_line_start(&mut self) {
        self.exit_history();
        let accumulate = self.last_action == Some(LastAction::Kill);
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        if col > 0 {
            self.push_undo();
            self.kill_ring.push(&line[..col], true, accumulate);
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] = line[col..].to_owned();
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo();
            self.kill_ring.push("\n", true, accumulate);
            self.last_action = Some(LastAction::Kill);
            self.merge_with_previous_line();
        }
    }

    fn delete_to_line_end(&mut self) {
        self.exit_history();
        let accumulate = self.last_action == Some(LastAction::Kill);
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        if col < line.len() {
            self.push_undo();
            self.kill_ring.push(&line[col..], false, accumulate);
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] = line[..col].to_owned();
        } else if self.state.cursor_line + 1 < self.state.lines.len() {
            self.push_undo();
            self.kill_ring.push("\n", false, accumulate);
            self.last_action = Some(LastAction::Kill);
            self.merge_with_next_line();
        }
    }

    fn delete_word_backward(&mut self) {
        self.exit_history();
        let accumulate = self.last_action == Some(LastAction::Kill);
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo();
                self.kill_ring.push("\n", true, accumulate);
                self.last_action = Some(LastAction::Kill);
                self.merge_with_previous_line();
            }
            return;
        }
        self.push_undo();
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        let from = find_word_backward(&line, col, &self.valid_ids());
        self.kill_ring.push(&line[from..col], true, accumulate);
        self.last_action = Some(LastAction::Kill);
        self.state.lines[self.state.cursor_line] = format!("{}{}", &line[..from], &line[col..]);
        self.set_cursor_col(from);
    }

    fn delete_word_forward(&mut self) {
        self.exit_history();
        let accumulate = self.last_action == Some(LastAction::Kill);
        let line = self.current_line().to_owned();
        let col = floor_boundary(&line, self.state.cursor_col);
        if col >= line.len() {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.push_undo();
                self.kill_ring.push("\n", false, accumulate);
                self.last_action = Some(LastAction::Kill);
                self.merge_with_next_line();
            }
            return;
        }
        self.push_undo();
        let to = find_word_forward(&line, col, &self.valid_ids());
        self.kill_ring.push(&line[col..to], false, accumulate);
        self.last_action = Some(LastAction::Kill);
        self.state.lines[self.state.cursor_line] = format!("{}{}", &line[..col], &line[to..]);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        self.push_undo();
        self.exit_history();
        self.insert_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        self.delete_yanked();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_owned();
        self.exit_history();
        self.insert_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn delete_yanked(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        let parts: Vec<&str> = yanked.split('\n').collect();
        if parts.len() == 1 {
            let line = self.current_line().to_owned();
            let col = floor_boundary(&line, self.state.cursor_col);
            let start = floor_boundary(&line, col.saturating_sub(yanked.len()));
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &line[..start], &line[col..]);
            self.set_cursor_col(start);
            return;
        }
        let Some(start_line) = self.state.cursor_line.checked_sub(parts.len() - 1) else {
            return;
        };
        let first = &self.state.lines[start_line];
        let start_col = floor_boundary(first, first.len().saturating_sub(parts[0].len()));
        let before = first[..start_col].to_owned();
        let after = self.current_line()
            [floor_boundary(self.current_line(), self.state.cursor_col)..]
            .to_owned();
        self.state.lines.splice(
            start_line..=self.state.cursor_line,
            [format!("{before}{after}")],
        );
        self.state.cursor_line = start_line;
        self.set_cursor_col(start_col);
    }

    fn jump_to(&mut self, target: &str, direction: Jump) {
        self.last_action = None;
        let lines = &self.state.lines;
        let count = lines.len();
        let mut index = self.state.cursor_line;
        loop {
            let line = &lines[index];
            let current = index == self.state.cursor_line;
            let found = match direction {
                Jump::Forward => {
                    let from = if current {
                        let col = self.state.cursor_col;
                        // One past the cursor's character.
                        line[col.min(line.len())..]
                            .chars()
                            .next()
                            .map_or(line.len(), |c| col + c.len_utf8())
                    } else {
                        0
                    };
                    line.get(from..)
                        .and_then(|rest| rest.find(target))
                        .map(|at| from + at)
                }
                Jump::Backward => {
                    let to = if current {
                        // The cursor's character is skipped; a match may start just before it.
                        let col = floor_boundary(line, self.state.cursor_col);
                        let start = line[..col].chars().last().map_or(0, |c| col - c.len_utf8());
                        (start + target.len()).min(line.len())
                    } else {
                        line.len()
                    };
                    if current && self.state.cursor_col == 0 {
                        // pi's lastIndexOf from -1 still checks index 0.
                        line.starts_with(target).then_some(0)
                    } else {
                        line[..floor_boundary(line, to)].rfind(target)
                    }
                }
            };
            if let Some(col) = found {
                self.state.cursor_line = index;
                self.set_cursor_col(col);
                return;
            }
            match direction {
                Jump::Forward if index + 1 < count => index += 1,
                Jump::Backward if index > 0 => index -= 1,
                _ => return,
            }
        }
    }

    // Cursor motion

    fn visual_lines(&self, width: usize) -> Vec<VisualLine> {
        let ids = self.valid_ids();
        let mut out = Vec::new();
        for (logical, line) in self.state.lines.iter().enumerate() {
            if line.is_empty() || visible_width(line) <= width {
                out.push(VisualLine {
                    logical,
                    start: 0,
                    len: line.len(),
                });
                continue;
            }
            for chunk in word_wrap_line(line, width, &ids) {
                out.push(VisualLine {
                    logical,
                    start: chunk.start,
                    len: chunk.end - chunk.start,
                });
            }
        }
        out
    }

    fn is_last_segment(visual: &[VisualLine], index: usize) -> bool {
        visual
            .get(index + 1)
            .is_none_or(|next| next.logical != visual[index].logical)
    }

    fn visual_line_at(visual: &[VisualLine], line: usize, col: usize) -> usize {
        for (index, vl) in visual.iter().enumerate() {
            if vl.logical != line || col < vl.start {
                continue;
            }
            let offset = col - vl.start;
            if offset < vl.len || (Self::is_last_segment(visual, index) && offset == vl.len) {
                return index;
            }
        }
        visual.len().saturating_sub(1)
    }

    fn current_visual_line(&self, visual: &[VisualLine]) -> usize {
        Self::visual_line_at(visual, self.state.cursor_line, self.state.cursor_col)
    }

    fn on_first_visual_line(&self) -> bool {
        let visual = self.visual_lines(self.last_width);
        self.current_visual_line(&visual) == 0
    }

    fn on_last_visual_line(&self) -> bool {
        let visual = self.visual_lines(self.last_width);
        self.current_visual_line(&visual) + 1 == visual.len()
    }

    /// Offset of `col` from a visual line's start in UTF-16 units, as pi counts.
    fn units_from(&self, vl: VisualLine, col: usize) -> usize {
        let line = &self.state.lines[vl.logical];
        to_utf16(line, col).saturating_sub(to_utf16(line, vl.start))
    }

    fn visual_len(&self, vl: VisualLine) -> usize {
        self.units_from(vl, vl.start + vl.len)
    }

    fn move_to_visual_line(&mut self, visual: &[VisualLine], from: usize, to: usize) {
        let (current, target) = (visual[from], visual[to]);
        let current_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let index = Self::visual_line_at(visual, current.logical, snapped);
                self.units_from(visual[index], snapped)
            }
            None => self.units_from(current, self.state.cursor_col),
        };
        let source_max = if Self::is_last_segment(visual, from) {
            self.visual_len(current)
        } else {
            self.visual_len(current).saturating_sub(1)
        };
        let target_max = if Self::is_last_segment(visual, to) {
            self.visual_len(target)
        } else {
            self.visual_len(target).saturating_sub(1)
        };
        let column = self.vertical_move_column(current_col, source_max, target_max);
        self.state.cursor_line = target.logical;
        let line = self.state.lines[target.logical].clone();
        let start_units = to_utf16(&line, target.start);
        self.state.cursor_col = from_utf16(&line, start_units + column).min(line.len());

        let ids = self.valid_ids();
        for seg in segment(&line, Granularity::Grapheme, &ids) {
            if seg.index > self.state.cursor_col {
                break;
            }
            if seg.text.chars().count() <= 1 {
                continue;
            }
            if self.state.cursor_col < seg.index + seg.text.len() {
                let continuation = seg.index < target.start;
                if continuation && to > from {
                    let end = seg.index + seg.text.len();
                    let mut next = to + 1;
                    while next < visual.len()
                        && visual[next].logical == target.logical
                        && visual[next].start < end
                    {
                        next += 1;
                    }
                    if next < visual.len() {
                        self.move_to_visual_line(visual, from, next);
                        return;
                    }
                }
                self.snapped_from_cursor_col = Some(self.state.cursor_col);
                self.state.cursor_col = seg.index;
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    /// pi's sticky-column decision table for vertical motion.
    fn vertical_move_column(
        &mut self,
        current: usize,
        source_max: usize,
        target_max: usize,
    ) -> usize {
        let in_middle = current < source_max;
        let too_short = target_max < current;
        match self.preferred_visual_col {
            Some(preferred) if !in_middle => {
                if too_short || target_max < preferred {
                    target_max
                } else {
                    self.preferred_visual_col = None;
                    preferred
                }
            }
            _ => {
                if too_short {
                    self.preferred_visual_col = Some(current);
                    target_max
                } else {
                    self.preferred_visual_col = None;
                    current
                }
            }
        }
    }

    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual = self.visual_lines(self.last_width);
        let current = self.current_visual_line(&visual);
        if delta_line != 0 {
            let target = current as isize + delta_line;
            if target >= 0 && (target as usize) < visual.len() {
                self.move_to_visual_line(&visual, current, target as usize);
            }
        }
        if delta_col != 0 {
            let line = self.current_line().to_owned();
            let col = floor_boundary(&line, self.state.cursor_col);
            let ids = self.valid_ids();
            if delta_col > 0 {
                if col < line.len() {
                    let step = segment(&line[col..], Granularity::Grapheme, &ids)
                        .first()
                        .map_or(1, |seg| seg.text.len());
                    self.set_cursor_col(col + step);
                } else if self.state.cursor_line + 1 < self.state.lines.len() {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else if let Some(vl) = visual.get(current) {
                    self.preferred_visual_col = Some(self.units_from(*vl, col));
                }
            } else if col > 0 {
                let step = segment(&line[..col], Granularity::Grapheme, &ids)
                    .last()
                    .map_or(1, |seg| seg.text.len());
                self.set_cursor_col(col - step);
            } else if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let end = self.current_line().len();
                self.set_cursor_col(end);
            }
        }
        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
        }
    }

    fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page = (self.terminal_rows * 3 / 10).max(5) as isize;
        let visual = self.visual_lines(self.last_width);
        let current = self.current_visual_line(&visual);
        let target = (current as isize + direction * page).clamp(0, visual.len() as isize - 1);
        self.move_to_visual_line(&visual, current, target as usize);
    }

    fn word_left(&mut self) {
        self.last_action = None;
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let end = self.current_line().len();
                self.set_cursor_col(end);
            }
            return;
        }
        let line = self.current_line().to_owned();
        let target = find_word_backward(
            &line,
            floor_boundary(&line, self.state.cursor_col),
            &self.valid_ids(),
        );
        self.set_cursor_col(target);
    }

    fn word_right(&mut self) {
        self.last_action = None;
        let line = self.current_line().to_owned();
        if self.state.cursor_col >= line.len() {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let target = find_word_forward(&line, self.state.cursor_col, &self.valid_ids());
        self.set_cursor_col(target);
    }

    // Autocomplete

    fn at_start_of_message(&self) -> bool {
        if self.state.cursor_line != 0 {
            return false;
        }
        let before = self.before_cursor().trim();
        before.is_empty() || before == "/"
    }

    fn in_slash_command(&self, before: &str) -> bool {
        self.state.cursor_line == 0 && before.trim_start().starts_with('/')
    }

    /// pi's trigger pattern: a trigger character at a token boundary, optionally
    /// after opening brackets or a backtick, followed by a token to the cursor.
    fn matches_trigger(&self, before: &str) -> bool {
        for (index, c) in before.char_indices().rev() {
            if !self.triggers.contains(&c) {
                continue;
            }
            let rest = &before[index + c.len_utf8()..];
            let quoted = c == '@' && rest.starts_with('"') && !rest[1..].contains('"');
            if !quoted && rest.chars().any(is_autocomplete_separator) {
                continue;
            }
            let head = before[..index].trim_end_matches(['(', '[', '{', '<', '`']);
            if head.chars().last().is_none_or(is_autocomplete_separator) {
                return true;
            }
        }
        false
    }

    fn selected_completion(&self) -> Option<SelectItem> {
        self.autocomplete_list
            .as_ref()
            .and_then(SelectList::selected_item)
            .cloned()
    }

    fn apply(&mut self, completion: Completion) {
        self.state.lines = completion.lines;
        self.state.cursor_line = completion
            .cursor_line
            .min(self.state.lines.len().saturating_sub(1));
        self.set_cursor_col(completion.cursor_col);
    }

    fn apply_completion(&mut self, item: &SelectItem) {
        let Some(provider) = &self.autocomplete else {
            return;
        };
        let completion = provider.apply(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            item,
            &self.autocomplete_prefix,
        );
        self.push_undo();
        self.last_action = None;
        self.apply(completion);
    }

    fn handle_tab(&mut self) {
        if self.autocomplete.is_none() {
            return;
        }
        let before = self.before_cursor().to_owned();
        if self.in_slash_command(&before) && !before.trim_start().contains(' ') {
            self.request_autocomplete(false, true);
        } else {
            self.request_autocomplete(true, true);
        }
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = &self.autocomplete else {
            return;
        };
        let (line, col) = (self.state.cursor_line, self.state.cursor_col);
        if force && !provider.should_trigger_file_completion(&self.state.lines, line, col) {
            return;
        }
        let suggestions = provider.suggestions(&self.state.lines, line, col, force);
        let Some(suggestions) = suggestions.filter(|s| !s.items.is_empty()) else {
            self.cancel_autocomplete();
            return;
        };
        if force && explicit_tab && suggestions.items.len() == 1 {
            let completion = provider.apply(
                &self.state.lines,
                line,
                col,
                &suggestions.items[0],
                &suggestions.prefix,
            );
            self.push_undo();
            self.last_action = None;
            self.apply(completion);
            return;
        }
        let layout = if suggestions.prefix.starts_with('/') {
            SLASH_COMMAND_LAYOUT
        } else {
            SelectListLayout::default()
        };
        let best = suggestions
            .items
            .iter()
            .position(|item| item.value == suggestions.prefix)
            .or_else(|| {
                suggestions
                    .items
                    .iter()
                    .position(|item| item.value.starts_with(&suggestions.prefix))
            })
            .filter(|_| !suggestions.prefix.is_empty());
        let mut list = SelectList::new(
            suggestions.items,
            self.autocomplete_max_visible,
            self.theme.select_list,
            layout,
        );
        if let Some(best) = best {
            list.set_selected_index(best);
        }
        self.autocomplete_prefix = suggestions.prefix;
        self.autocomplete_list = Some(list);
        self.autocomplete_mode = Some(if force {
            AutocompleteMode::Force
        } else {
            AutocompleteMode::Regular
        });
    }

    fn cancel_autocomplete(&mut self) {
        self.autocomplete_mode = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
    }

    fn update_autocomplete(&mut self) {
        if let Some(mode) = self.autocomplete_mode {
            self.request_autocomplete(mode == AutocompleteMode::Force, false);
        }
    }

    /// Handles a key while the autocomplete list is open and returns whether
    /// the list consumed it; used by hosts that route list keys themselves.
    pub fn autocomplete_event(&mut self, data: &str, keybindings: &Keybindings) -> SelectEvent {
        match &mut self.autocomplete_list {
            Some(list) => list.handle_input(data, keybindings),
            None => SelectEvent::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{UserBindings, tui_definitions};
    use crate::keys::Keys;

    fn bindings() -> Keybindings {
        Keybindings::new(
            Keys::default(),
            tui_definitions(),
            &UserBindings::new(),
            &[],
        )
    }

    fn typed(editor: &mut Editor, keys: &[&str]) -> Vec<EditorEvent> {
        let kb = bindings();
        keys.iter()
            .map(|key| editor.handle_input(key, &kb))
            .collect()
    }

    fn editor() -> Editor {
        Editor::new(EditorTheme::default(), 0, 5)
    }

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn types_wraps_and_submits() {
        let mut e = editor();
        for c in "hello world".chars() {
            typed(&mut e, &[&c.to_string()]);
        }
        assert_eq!(e.text(), "hello world");
        assert_eq!(
            plain(&e.render(10)),
            ["──────────", "hello     ", "world     ", "──────────"]
        );
        assert_eq!(e.cursor_position(), Some((2, 5)));
        assert_eq!(
            typed(&mut e, &["\r"]),
            [EditorEvent::Submit("hello world".into())]
        );
        assert_eq!(e.text(), "");
    }

    #[test]
    fn backslash_enter_inserts_newline() {
        let mut e = editor();
        typed(&mut e, &["a", "\\", "\r", "b"]);
        assert_eq!(e.text(), "a\nb");
        typed(&mut e, &["\x1b[13;2u", "c"]);
        assert_eq!(e.text(), "a\nb\nc");
    }

    #[test]
    fn kill_ring_accumulates_and_yanks() {
        let mut e = editor();
        e.set_text("one two three");
        typed(&mut e, &["\x17", "\x17"]);
        assert_eq!(e.text(), "one ");
        typed(&mut e, &["\x19"]);
        assert_eq!(e.text(), "one two three");
        typed(&mut e, &["\x01", "\x0b", "x", "\x19"]);
        assert_eq!(e.text(), "xone two three");
    }

    #[test]
    fn undo_groups_words() {
        let mut e = editor();
        typed(&mut e, &["a", "b", " ", "c", "d"]);
        typed(&mut e, &["\x1f"]);
        assert_eq!(e.text(), "ab");
        typed(&mut e, &["\x1f"]);
        assert_eq!(e.text(), "");
    }

    #[test]
    fn history_navigation() {
        let mut e = editor();
        e.add_to_history("first");
        e.add_to_history("second");
        e.add_to_history("second");
        typed(&mut e, &["\x1b[A"]);
        assert_eq!(e.text(), "second");
        typed(&mut e, &["\x1b[A"]);
        assert_eq!(e.text(), "first");
        typed(&mut e, &["\x1b[B", "\x1b[B"]);
        assert_eq!(e.text(), "");
    }

    #[test]
    fn large_pastes_become_markers() {
        let mut e = editor();
        let pasted: String = (0..12).map(|i| format!("line {i}\n")).collect();
        typed(&mut e, &["x", &format!("\x1b[200~{pasted}\x1b[201~")]);
        assert_eq!(e.text(), "x[paste #1 +13 lines]");
        typed(&mut e, &["\x1b[D"]);
        assert_eq!(e.cursor(), (0, 1), "the marker is one unit");
        typed(&mut e, &["\x1b[C", "\x7f"]);
        assert_eq!(e.text(), "x");
        typed(&mut e, &[&format!("\x1b[200~{pasted}\x1b[201~")]);
        assert_eq!(
            typed(&mut e, &["\r"]),
            [EditorEvent::Submit(format!("x{pasted}").trim().to_owned())]
        );
    }

    #[test]
    fn sticky_column_and_scroll_indicator() {
        let mut e = editor();
        e.set_text("long line here\nab\nanother long line");
        e.set_terminal_rows(10);
        e.render(40);
        typed(&mut e, &["\x1b[A", "\x1b[A"]);
        assert_eq!(e.cursor(), (0, 14));
        typed(&mut e, &["\x1b[D", "\x1b[D", "\x1b[B"]);
        assert_eq!(e.cursor(), (1, 2));
        typed(&mut e, &["\x1b[B"]);
        assert_eq!(e.cursor(), (2, 12));
        let text: String = (0..8).map(|i| format!("{i}\n")).collect();
        e.set_text(&text);
        let lines = plain(&e.render(20));
        assert_eq!(lines[0], "───── ↑ 4 more ─────");
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn jumps_to_characters() {
        let mut e = editor();
        e.set_text("abcabc\nxbz");
        typed(&mut e, &["\x01"]);
        assert_eq!(e.cursor(), (1, 0));
        typed(&mut e, &["\x1d", "z"]);
        assert_eq!(e.cursor(), (1, 2));
        typed(&mut e, &["\x1b\x1d", "c"]);
        assert_eq!(e.cursor(), (0, 5));
        typed(&mut e, &["\x1b\x1d", "c"]);
        assert_eq!(e.cursor(), (0, 2));
    }
}
