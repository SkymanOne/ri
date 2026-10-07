//! Mouse input in fullscreen mode: wheel scrolling and text selection.
//!
//! Port of the mouse handling of `packages/tui/src/tui-alt-screen.ts` in pi
//! `v1.0.0`. A press starts a selection in the transcript, in content rows
//! so it scrolls with the text, or on the screen elsewhere; a drag extends
//! it and scrolls the transcript while held at its edge; a release finishes
//! it. Double and triple clicks select words and lines.

use std::time::{Duration, Instant};

use ratatui_core::style::{Modifier, Style};

use super::AltScreen;
use crate::lines::{StyledLine, cell_range, plain, restyle_columns, text_in_columns, width};
use crate::segment::{Granularity, segment};
use crate::text::visible_width;

/// pi's `DOUBLE_CLICK_INTERVAL_MS`.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// How often a drag held at the transcript's edge scrolls it.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);
/// Alt+wheel scrolls this many times as far.
const ALT_WHEEL_MULTIPLIER: usize = 5;
const FOCUS_IN: &str = "\x1b[I";
const FOCUS_OUT: &str = "\x1b[O";

/// One end of a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    row: usize,
    col: usize,
    /// A transcript row rather than a screen row.
    transcript: bool,
    /// Between cells rather than on one.
    boundary: bool,
}

impl Point {
    fn at(&self) -> (usize, usize) {
        (self.row, self.col)
    }
}

/// What a double or triple click snaps the selection to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Word,
    Line,
}

#[derive(Debug)]
struct Click {
    at: Instant,
    count: u8,
    row: usize,
    transcript: bool,
    word: (usize, usize),
}

/// The selection state of the fullscreen viewport.
#[derive(Debug, Default)]
pub(super) struct Selection {
    anchor: Option<Point>,
    focus: Option<Point>,
    /// The word or line a double or triple click selected first.
    initial: Option<(Unit, Point, Point)>,
    last_click: Option<Click>,
    pressed: bool,
    /// A drag held at the transcript's edge: direction, pointer column and
    /// row, and when it scrolls next.
    autoscroll: Option<(isize, usize, usize, Instant)>,
}

/// What [`AltScreen::mouse`] did with its input.
#[derive(Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// The input is no mouse report or focus change.
    Unhandled,
    /// The input was handled.
    Handled,
    /// A selection finished while `copy_on_select` is on: its text, to copy.
    Copy(String),
}

/// An SGR mouse report: button code, column and row from zero, and whether
/// it is a release.
fn parse(data: &str) -> Option<(usize, usize, usize, bool)> {
    let body = data.strip_prefix("\x1b[<")?;
    let release = body.ends_with('m');
    let mut fields = body.strip_suffix(['M', 'm'])?.split(';').map(|field| {
        (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| field.parse::<usize>().ok())
            .flatten()
    });
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(Some(button)), Some(Some(x)), Some(Some(y)), None) => {
            Some((button, x.saturating_sub(1), y.saturating_sub(1), release))
        }
        _ => None,
    }
}

impl AltScreen {
    /// pi's `handleViewportInput` for mouse reports and focus changes: the
    /// wheel scrolls [`AltScreen::wheel_lines`] rows, the left button selects
    /// text and losing focus mid-press drops the selection. `transcript` is
    /// the one last drawn.
    pub fn mouse(&mut self, data: &str, transcript: &[StyledLine]) -> MouseAction {
        if data == FOCUS_OUT {
            let selection = &mut self.selection;
            if selection.pressed {
                selection.anchor = None;
                selection.focus = None;
                selection.initial = None;
            }
            selection.pressed = false;
            selection.autoscroll = None;
            selection.last_click = None;
            return MouseAction::Handled;
        }
        if data == FOCUS_IN {
            return MouseAction::Handled;
        }
        let Some((button, x, y, release)) = parse(data) else {
            return MouseAction::Unhandled;
        };
        if button & 64 != 0 {
            // Wheel up or down; horizontal wheels do nothing.
            let lines = if button & 8 != 0 {
                self.wheel_lines * ALT_WHEEL_MULTIPLIER
            } else {
                self.wheel_lines
            } as isize;
            match button & 3 {
                0 => self.scroll_by(-lines),
                1 => self.scroll_by(lines),
                _ => {}
            }
            return MouseAction::Handled;
        }
        // The left button; some terminals release with button 3.
        if button & 3 != 0 && !(release && button & 3 == 3) {
            return MouseAction::Handled;
        }
        let in_transcript = self
            .selection
            .anchor
            .is_some_and(|anchor| anchor.transcript);
        let point = self.point(x, y, in_transcript);
        if release {
            if !self.selection.pressed {
                return MouseAction::Handled;
            }
            self.selection.pressed = false;
            self.selection.autoscroll = None;
            if self.selection.anchor.is_none() {
                return MouseAction::Handled;
            }
            self.extend(point, transcript);
            return match self.selected_text(transcript) {
                Some(text) if self.copy_on_select => MouseAction::Copy(text),
                _ => MouseAction::Handled,
            };
        }
        if button & 32 != 0 {
            if !self.selection.pressed || self.selection.anchor.is_none() {
                return MouseAction::Handled;
            }
            self.selection.last_click = None;
            self.extend(point, transcript);
            self.update_autoscroll(x, y);
            return MouseAction::Handled;
        }
        // A press starts a selection, in the transcript when no overlay shows.
        self.selection.autoscroll = None;
        self.selection.pressed = true;
        let anchor = self.point(x, y, self.overlays.is_empty() && y < self.viewport);
        let word = self.word_at(anchor, transcript);
        let range = match self.click_count(anchor, word) {
            2 => word.map(|(start, end)| (Unit::Word, start, end)),
            3 => {
                let (start, end) = self.line_at(anchor, transcript);
                Some((Unit::Line, start, end))
            }
            _ => None,
        };
        self.selection.initial = range;
        self.selection.anchor = Some(range.map_or(anchor, |(_, start, _)| start));
        self.selection.focus = Some(range.map_or(anchor, |(_, _, end)| end));
        MouseAction::Handled
    }

    /// The selected text, one line per row without trailing whitespace;
    /// `None` when nothing is selected. `transcript` is the one last drawn.
    pub fn selected_text(&self, transcript: &[StyledLine]) -> Option<String> {
        let (start, end) = self.bounds()?;
        let source = if start.transcript {
            transcript
        } else {
            &self.screen
        };
        let empty = StyledLine::default();
        let rows: Vec<String> = (start.row..=end.row)
            .map(|row| {
                let line = source.get(row).unwrap_or(&empty);
                let (from, to) = columns(line, row, (start, end), width(line));
                text_in_columns(line, from, to).trim_end().to_owned()
            })
            .collect();
        let text = rows.join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// The point under column `x` and row `y`: in the transcript's content
    /// rows, the pointer held within its visible rows, or on the screen.
    fn point(&self, x: usize, y: usize, transcript: bool) -> Point {
        let (columns, rows) = self.size;
        let col = x.min(columns.saturating_sub(1));
        if transcript && self.viewport > 0 {
            let row = self.scroll_position() + y.min(self.viewport - 1);
            return Point {
                row: row.min(self.transcript_len.saturating_sub(1)),
                col,
                transcript,
                boundary: false,
            };
        }
        Point {
            row: y.min(rows.saturating_sub(1)),
            col,
            transcript: false,
            boundary: false,
        }
    }

    fn source<'a>(&'a self, point: Point, transcript: &'a [StyledLine]) -> Option<&'a StyledLine> {
        if point.transcript {
            transcript.get(point.row)
        } else {
            self.screen.get(point.row)
        }
    }

    /// pi's `getWordSelection`: the word under `point`, joined across `/`
    /// and `-` so paths and kebab-case names stay whole, or the run of
    /// spaces or punctuation there.
    fn word_at(&self, point: Point, transcript: &[StyledLine]) -> Option<(Point, Point)> {
        let text = self
            .source(point, transcript)
            .map(plain)
            .unwrap_or_default();
        // Each segment's columns, whether a word can join, and whether it joins words.
        let mut segments = Vec::new();
        let mut start = 0;
        for word in segment(&text, Granularity::Word, &[]) {
            let end = start + visible_width(word.text);
            let joiner = matches!(word.text, "/" | "-");
            segments.push((start, end, word.word_like || joiner, joiner));
            start = end;
        }
        let clicked = segments
            .iter()
            .position(|(start, end, _, _)| point.col >= *start && point.col < *end)?;
        let joins = |left: usize, right: usize| {
            let (left, right) = (segments[left], segments[right]);
            left.2 && right.2 && (left.3 || right.3)
        };
        let mut first = clicked;
        while first > 0 && joins(first - 1, first) {
            first -= 1;
        }
        let mut last = clicked;
        while last + 1 < segments.len() && joins(last, last + 1) {
            last += 1;
        }
        Some((
            Point {
                col: segments[first].0,
                ..point
            },
            Point {
                col: segments[last].1,
                boundary: true,
                ..point
            },
        ))
    }

    fn line_at(&self, point: Point, transcript: &[StyledLine]) -> (Point, Point) {
        let end = self.source(point, transcript).map_or(0, width);
        (
            Point { col: 0, ..point },
            Point {
                col: end,
                boundary: true,
                ..point
            },
        )
    }

    /// pi's `getClickCount`: presses on the same word within the
    /// double-click interval count up to three, then start over.
    fn click_count(&mut self, point: Point, word: Option<(Point, Point)>) -> u8 {
        let now = Instant::now();
        let count = match (&self.selection.last_click, word) {
            (Some(last), Some((start, end)))
                if now.duration_since(last.at) <= DOUBLE_CLICK
                    && last.row == point.row
                    && last.transcript == point.transcript
                    && last.word == (start.col, end.col) =>
            {
                last.count % 3 + 1
            }
            _ => 1,
        };
        self.selection.last_click = word.map(|(start, end)| Click {
            at: now,
            count,
            row: point.row,
            transcript: point.transcript,
            word: (start.col, end.col),
        });
        count
    }

    /// pi's `updateSelectionFocus`: moves the selection's free end to
    /// `point`, by whole words or lines after a double or triple click.
    fn extend(&mut self, point: Point, transcript: &[StyledLine]) {
        let Some((unit, first, last)) = self.selection.initial else {
            self.selection.focus = Some(point);
            return;
        };
        let range = match unit {
            Unit::Word => self.word_at(point, transcript),
            Unit::Line => Some(self.line_at(point, transcript)),
        };
        let Some((start, end)) = range else {
            return;
        };
        let (anchor, focus) = if start.at() < first.at() {
            (last, start)
        } else {
            (first, end)
        };
        self.selection.anchor = Some(anchor);
        self.selection.focus = Some(focus);
    }

    /// pi's `updateSelectionAutoScroll`: a drag of a transcript selection at
    /// the transcript's top or bottom row, or below it, scrolls that way.
    fn update_autoscroll(&mut self, x: usize, y: usize) {
        let in_transcript = self
            .selection
            .anchor
            .is_some_and(|anchor| anchor.transcript);
        let bottom = self.viewport.min(self.size.1).saturating_sub(1);
        let direction = match y {
            _ if !in_transcript || self.viewport == 0 => 0,
            0 => -1,
            y if y >= bottom => 1,
            _ => 0,
        };
        self.selection.autoscroll = (direction != 0).then(|| {
            let next = self.selection.autoscroll.map_or_else(
                || Instant::now() + AUTOSCROLL_INTERVAL,
                |(_, _, _, next)| next,
            );
            (direction, x, y, next)
        });
    }

    /// pi's `autoScrollSelection`, when due: scrolls a row and extends the
    /// selection to the pointer, or stops at the transcript's end.
    pub(super) fn step_autoscroll(&mut self, transcript: &[StyledLine]) {
        let Some((direction, x, y, next)) = self.selection.autoscroll else {
            return;
        };
        let now = Instant::now();
        if now < next {
            return;
        }
        let before = self.scroll_position();
        self.scroll_by(direction);
        if self.scroll_position() == before {
            self.selection.autoscroll = None;
            return;
        }
        self.selection.autoscroll = Some((direction, x, y, now + AUTOSCROLL_INTERVAL));
        let point = self.point(x, y, true);
        self.extend(point, transcript);
    }

    /// When a held drag scrolls next.
    pub(super) fn autoscroll_deadline(&self) -> Option<Instant> {
        self.selection.autoscroll.map(|(_, _, _, next)| next)
    }

    /// pi's `getSelectionBounds`: the selection's ends in order, unless it
    /// is empty.
    fn bounds(&self) -> Option<(Point, Point)> {
        let (anchor, focus) = (self.selection.anchor?, self.selection.focus?);
        if anchor.transcript != focus.transcript || anchor.at() == focus.at() {
            return None;
        }
        Some(if anchor.at() < focus.at() {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    /// pi's `applySelection`: reverses the selected cells of `rows`, the
    /// screen whose transcript starts at row `top`.
    pub(super) fn highlight(&self, rows: &mut [StyledLine], top: usize, max_column: usize) {
        let Some((start, end)) = self.bounds() else {
            return;
        };
        let (offset, visible) = if start.transcript {
            (top, self.viewport.min(rows.len()))
        } else {
            (0, rows.len())
        };
        let reversed = Style::new().add_modifier(Modifier::REVERSED);
        for (index, line) in rows.iter_mut().enumerate().take(visible) {
            let row = offset + index;
            if row < start.row || row > end.row {
                continue;
            }
            let (from, to) = columns(line, row, (start, end), max_column);
            if to > from {
                *line = restyle_columns(line, from, to, reversed);
            }
        }
    }
}

/// pi's `getSelectionColumns`: the columns of `line`, row `row` of the
/// selection from `start` to `end`, that the selection covers, up to `max`.
/// Its ends snap to the graphemes they fall on.
fn columns(
    line: &StyledLine,
    row: usize,
    (start, end): (Point, Point),
    max: usize,
) -> (usize, usize) {
    let line_width = width(line);
    let mut from = 0;
    let mut to = line_width.min(max);
    if row == start.row {
        from = cell_range(line, start.col).map_or(start.col.min(line_width), |(from, _)| from);
    }
    if row == end.row {
        to = if end.boundary {
            end.col.min(line_width)
        } else {
            cell_range(line, end.col).map_or((end.col + 1).min(line_width), |(_, to)| to)
        };
    }
    (from, to.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sgr_mouse_reports() {
        assert_eq!(parse("\x1b[<0;1;1M"), Some((0, 0, 0, false)));
        assert_eq!(parse("\x1b[<35;20;5m"), Some((35, 19, 4, true)));
        assert_eq!(parse("\x1b[<0;1M"), None);
        assert_eq!(parse("\x1b[<0;+1;1M"), None);
        assert_eq!(parse("\x1b[A"), None);
    }
}
