//! Mouse input in fullscreen mode: wheel scrolling, the scrollbar, clicks
//! and text selection.
//!
//! Port of the mouse handling of `packages/tui/src/tui-alt-screen.ts` in pi
//! `v1.0.0`. A press starts a selection in the transcript, in content rows
//! so it scrolls with the text, or on the screen elsewhere; a drag extends
//! it and scrolls the transcript while held at its edge; a release finishes
//! it. Double and triple clicks select words and lines. Components under the
//! pointer get presses, clicks and the wheel first.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui_core::style::{Modifier, Style};

use super::AltScreen;
use crate::lines::{
    StyledLine, cell_range, link_at, plain, restyle_columns, text_in_columns, width,
};
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

/// The selection and pointer state of the fullscreen viewport.
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
    /// The pointer moved since the press.
    dragged: bool,
    /// The link under the press, opened if it ends as a click; pi's
    /// `pressedUrl`.
    url: Option<Arc<str>>,
    /// A press a component took: where, and whether the pointer moved since.
    press: Option<(usize, usize, bool)>,
    /// The scrollbar's thumb is being dragged, held this many rows below its
    /// top.
    scrollbar_drag: Option<usize>,
}

/// What a component under the pointer gets: the kinds of pi-tui's
/// `TuiMouseEvent` that the layout's components handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    /// The left button went down.
    Press,
    /// The left button went down and up on one cell without moving.
    Click,
    /// The wheel turned up (-1) or down (1).
    Wheel(isize),
}

/// A [`MouseKind`] at a column and row of the screen, from zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    /// What happened.
    pub kind: MouseKind,
    /// The column.
    pub x: usize,
    /// The row.
    pub y: usize,
    /// The report's button code, whose bits 4, 8 and 16 are Shift, Alt and
    /// Ctrl.
    pub button: usize,
    /// For the wheel, the rows it scrolls, negative upward: pi's
    /// `wheelDelta`.
    pub delta: isize,
    /// The topmost of [`AltScreen::overlays`] under the pointer, which gets
    /// the event in place of the components below it.
    pub overlay: Option<usize>,
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
    /// A link was clicked: its URL, to open.
    Open(String),
}

/// pi's `WheelScrollAccelerator` (`packages/tui/src/wheel-scroll.ts`): turns
/// wheel events into rows to scroll.
#[derive(Debug, Default)]
pub struct WheelScroll {
    /// Speed up fast spins in `auto` mode. Off by default on a local macOS
    /// terminal, which accelerates the wheel itself.
    pub accelerate: bool,
    /// The last event's time and direction.
    last: Option<(Instant, isize)>,
    average_gap: Option<f64>,
    carry: f64,
}

/// Events closer than this, in milliseconds, are one notch or a
/// high-resolution wheel, and scroll a row each.
const BURST_GAP_MS: f64 = 5.0;
/// A longer pause ends a spin.
const GESTURE_GAP_MS: f64 = 200.0;
/// The average gap that scrolls one row per event; faster spins scale up.
const REFERENCE_GAP_MS: f64 = 100.0;
const MAX_AUTO_LINES: f64 = 6.0;

impl WheelScroll {
    /// State for this terminal: accelerating unless it is a local macOS one.
    pub fn new() -> WheelScroll {
        let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some());
        WheelScroll {
            accelerate: !cfg!(target_os = "macos") || ssh,
            ..WheelScroll::default()
        }
    }

    /// pi's `next`: the rows an event in `direction` (-1 or 1) at `now`
    /// scrolls, `lines` each, or in `auto` mode when `None`: one for an
    /// isolated notch and up to six for a fast spin.
    pub fn next(&mut self, lines: Option<usize>, direction: isize, now: Instant) -> usize {
        if let Some(lines) = lines {
            return lines.max(1);
        }
        if !self.accelerate {
            return 1;
        }
        let gap = self
            .last
            .filter(|(_, last)| *last == direction)
            .map(|(at, _)| now.duration_since(at).as_secs_f64() * 1000.0)
            .filter(|gap| *gap <= GESTURE_GAP_MS);
        self.last = Some((now, direction));
        let Some(gap) = gap else {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        };
        if gap < BURST_GAP_MS {
            return 1;
        }
        let average = self
            .average_gap
            .map_or(gap, |average| (average + gap) / 2.0);
        self.average_gap = Some(average);
        let lines = (REFERENCE_GAP_MS / average).clamp(1.0, MAX_AUTO_LINES) + self.carry;
        self.carry = lines.fract();
        lines.floor() as usize
    }
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
    /// wheel scrolls [`AltScreen::wheel_lines`] rows, the left button drags
    /// the scrollbar, jumps to the latest message from its label or selects
    /// text, and losing focus mid-press drops the selection. `components`
    /// gets the events pi offers the overlay or the components under the
    /// pointer, and says whether one took the event. A wheel event nothing
    /// takes is unhandled while an overlay shows, as pi gives it to the
    /// focused overlay. `transcript` is the one last drawn.
    pub fn mouse(
        &mut self,
        data: &str,
        transcript: &[StyledLine],
        mut components: impl FnMut(MouseEvent) -> bool,
    ) -> MouseAction {
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
            selection.press = None;
            selection.scrollbar_drag = None;
            selection.url = None;
            self.set_scrollbar_hover(false);
            return MouseAction::Handled;
        }
        if data == FOCUS_IN || self.pointer_moved(data).is_some() {
            return MouseAction::Handled;
        }
        let Some((button, x, y, release)) = parse(data) else {
            return MouseAction::Unhandled;
        };
        let overlay = self.overlay_at(x, y);
        let covered = overlay.is_some();
        let event = |kind, delta| MouseEvent {
            kind,
            x,
            y,
            button,
            delta,
            overlay,
        };
        if button & 64 != 0 {
            // Wheel up or down; horizontal wheels do nothing.
            let direction = match button & 3 {
                0 => -1,
                1 => 1,
                _ => return MouseAction::Handled,
            };
            let mut lines = self.wheel.next(self.wheel_lines, direction, Instant::now());
            if button & 8 != 0 {
                lines *= ALT_WHEEL_MULTIPLIER;
            }
            // A component under the pointer may take it instead.
            let delta = direction * lines as isize;
            if components(event(MouseKind::Wheel(direction), delta)) {
                return MouseAction::Handled;
            }
            if !self.overlays.is_empty() {
                return MouseAction::Unhandled;
            }
            self.scroll_by(delta);
            self.update_scrollbar_hover(x, y);
            return MouseAction::Handled;
        }
        if let Some((at_x, at_y, moved)) = &mut self.selection.press {
            *moved |= (x, y) != (*at_x, *at_y);
            if release {
                let click = !*moved;
                self.selection.press = None;
                if click {
                    components(event(MouseKind::Click, 0));
                }
            }
            return MouseAction::Handled;
        }
        let left_press = button & (32 | 3) == 0 && !release;
        if let Some((row, col, width)) = self.jump_label
            && left_press
            && !covered
            && y == row
            && (col..col + width).contains(&x)
        {
            self.bottom();
            return MouseAction::Handled;
        }
        let scrollbar = !covered && self.scrollbar_mouse(button, x, y, release);
        if self.selection.scrollbar_drag.is_none() {
            self.update_scrollbar_hover(x, y);
        }
        if scrollbar {
            return MouseAction::Handled;
        }
        if left_press && components(event(MouseKind::Press, 0)) {
            self.clear_selection();
            self.selection.press = Some((x, y, false));
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
            // A press and release on one cell is a click, which a component
            // may take instead.
            let click = !self.selection.dragged
                && self.selection.anchor.is_some_and(|anchor| {
                    anchor.transcript == point.transcript && anchor.at() == point.at()
                });
            if let Some(url) = self.selection.url.take().filter(|_| click) {
                self.selection.anchor = None;
                self.selection.focus = None;
                return MouseAction::Open(url.to_string());
            }
            if click && components(event(MouseKind::Click, 0)) {
                self.clear_selection();
                return MouseAction::Handled;
            }
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
            self.selection.dragged = true;
            self.selection.url = None;
            self.extend(point, transcript);
            self.update_autoscroll(x, y);
            return MouseAction::Handled;
        }
        // A press starts a selection, in the transcript when no overlay shows.
        self.selection.autoscroll = None;
        self.selection.pressed = true;
        self.selection.dragged = false;
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
        self.selection.url = match range {
            Some(_) => None,
            None => self
                .screen
                .get(y)
                .and_then(|line| link_at(line, anchor.col)),
        };
        self.selection.anchor = Some(range.map_or(anchor, |(_, start, _)| start));
        self.selection.focus = Some(range.map_or(anchor, |(_, _, end)| end));
        MouseAction::Handled
    }

    /// pi's handling of a pointer move with no button held, which changes
    /// only the scrollbar's hover: for such a report, whether that changed,
    /// so the screen needs drawing; `None` for any other input.
    pub fn pointer_moved(&mut self, data: &str) -> Option<bool> {
        let (button, x, y, release) = parse(data)?;
        if release
            || button & (64 | 32 | 3) != 32 | 3
            || self.selection.press.is_some()
            || self.selection.scrollbar_drag.is_some()
        {
            return None;
        }
        let hover = self.scrollbar_hover;
        self.update_scrollbar_hover(x, y);
        Some(self.scrollbar_hover != hover)
    }

    /// A component took the press that started the selection being made,
    /// at column `x` and row `y`, as pi learns before the selection starts:
    /// it ends the selection, and a release there is a click on the
    /// component. Does nothing once the button is up or the pointer moved.
    pub fn take_press(&mut self, x: usize, y: usize) {
        if self.selection.pressed && !self.selection.dragged {
            self.clear_selection();
            self.selection.press = Some((x, y, false));
        }
    }

    /// pi's `clearTextSelection`.
    pub fn clear_selection(&mut self) {
        let selection = &mut self.selection;
        selection.anchor = None;
        selection.focus = None;
        selection.initial = None;
        selection.pressed = false;
        selection.autoscroll = None;
        selection.dragged = false;
        selection.url = None;
    }

    fn overlay_at(&self, x: usize, y: usize) -> Option<usize> {
        self.overlays.iter().rposition(|overlay| {
            (overlay.col..overlay.col + overlay.width).contains(&x)
                && (overlay.row..overlay.row + overlay.lines.len()).contains(&y)
        })
    }

    /// pi's `updateScrollbarHover`: whether the pointer is over the
    /// transcript's scrollbar, or where an `auto` one would show, while no
    /// overlay shows.
    fn update_scrollbar_hover(&mut self, x: usize, y: usize) {
        let over = self.overlays.is_empty()
            && x + 1 == self.size.0
            && y < self.viewport
            && self.thumb(true).is_some();
        self.set_scrollbar_hover(over);
    }

    /// pi's `setScrollbarActive`: an `auto` scrollbar shows while hovered,
    /// and for a moment after.
    fn set_scrollbar_hover(&mut self, hover: bool) {
        if hover != self.scrollbar_hover {
            self.scrollbar_hover = hover;
            self.scrolled();
        }
    }

    /// pi's `handleScrollbarMouseEvent`: a left press on the shown scrollbar
    /// drags its thumb until the release, after moving the thumb's middle
    /// there when the press is off it.
    fn scrollbar_mouse(&mut self, button: usize, x: usize, y: usize, release: bool) -> bool {
        if let Some(grab) = self.selection.scrollbar_drag {
            if release {
                self.selection.scrollbar_drag = None;
            } else {
                self.drag_scrollbar(y, grab);
            }
            return true;
        }
        if release || button & (32 | 3) != 0 || !self.overlays.is_empty() {
            return false;
        }
        let Some((offset, thumb)) = self
            .thumb(false)
            .filter(|_| x + 1 == self.size.0 && y < self.viewport)
        else {
            return false;
        };
        self.clear_selection();
        self.selection.last_click = None;
        self.set_scrollbar_hover(true);
        let on_thumb = (offset..offset + thumb).contains(&y);
        let grab = if on_thumb { y - offset } else { thumb / 2 };
        if !on_thumb {
            self.drag_scrollbar(y, grab);
        }
        self.selection.scrollbar_drag = Some(grab);
        true
    }

    /// pi's `scrollScrollbarToPointer`: scrolls so the thumb's row `grab`
    /// is at row `y`.
    fn drag_scrollbar(&mut self, y: usize, grab: usize) {
        let Some((_, thumb)) = self.thumb(false) else {
            return;
        };
        let max_offset = self.viewport - thumb;
        let offset = y.saturating_sub(grab).min(max_offset);
        let top = if max_offset == 0 {
            0
        } else {
            (offset as f64 / max_offset as f64 * self.max_scroll() as f64).round() as usize
        };
        self.scroll_to(top);
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

    /// Feeds `inputs` to `screen`, whose components take nothing: the clicks
    /// offered to them and the last action.
    fn feed(screen: &mut AltScreen, inputs: &[&str]) -> (Vec<(usize, usize)>, MouseAction) {
        let (mut clicks, mut action) = (Vec::new(), MouseAction::Unhandled);
        for input in inputs {
            action = screen.mouse(input, &[], |event| {
                if event.kind == MouseKind::Click {
                    clicks.push((event.x, event.y));
                }
                false
            });
        }
        (clicks, action)
    }

    #[test]
    fn a_press_taken_late_selects_nothing() {
        let mut screen = AltScreen::new();
        screen.copy_on_select = true;
        screen.frame(&[], &[StyledLine::from("pick me")], None, 20, 5);
        let drag = ["\x1b[<32;5;5M", "\x1b[<0;5;5m"];
        feed(&mut screen, &["\x1b[<0;1;5M"]);
        assert_eq!(feed(&mut screen, &drag).1, MouseAction::Copy("pick".into()));
        // Taken before the pointer moves, a press selects nothing, and a
        // release where it went down is a click.
        feed(&mut screen, &["\x1b[<0;1;5M"]);
        screen.take_press(0, 4);
        assert_eq!(feed(&mut screen, &drag), (vec![], MouseAction::Handled));
        feed(&mut screen, &["\x1b[<0;1;5M"]);
        screen.take_press(0, 4);
        let release = feed(&mut screen, &["\x1b[<0;1;5m"]);
        assert_eq!(release, (vec![(0, 4)], MouseAction::Handled));
    }

    #[test]
    fn the_wheel_over_an_overlay_goes_to_it() {
        let mut screen = AltScreen::new();
        screen.frame(&[], &[], None, 20, 5);
        screen.overlays = vec![crate::screen::Overlay {
            row: 1,
            col: 2,
            width: 5,
            lines: vec![StyledLine::from("menu")],
        }];
        let mut overlay = None;
        let action = screen.mouse("\x1b[<65;4;2M", &[], |event| {
            overlay = event.overlay;
            false
        });
        // Nothing took it, so it is the focused overlay's input.
        assert_eq!((action, overlay), (MouseAction::Unhandled, Some(0)));
    }
}
