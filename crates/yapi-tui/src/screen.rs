//! Writing frames of styled lines to the terminal.
//!
//! [`MainScreen`] is pi's regular mode (`packages/tui/src/tui-main-screen.ts`
//! in pi `v1.0.0`): the document flows into the terminal's scrollback and only
//! changed lines are rewritten. [`AltScreen`] is the fullscreen mode
//! (`tui-alt-screen.ts`): a scrolling transcript above a dock pinned to the
//! bottom of the alternate screen.

use std::time::{Duration, Instant};

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::ansi::line_to_ansi;
use crate::lines::{StyledLine, composite, truncate, width as line_width};
pub use selection::{MouseAction, MouseEvent, MouseKind, WheelScroll};
/// pi's `fullscreenScrollbar` setting.
pub use yapi_types::settings::Scrollbar;

mod selection;

const SYNC_START: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

fn clip(line: &StyledLine, width: usize) -> String {
    if line_width(line) > width {
        line_to_ansi(&truncate(line, width, ""))
    } else {
        line_to_ansi(line)
    }
}

/// A frame cursor position: row in the rendered lines and column.
pub(crate) type Cursor = Option<(usize, usize)>;

/// A component drawn over the screen, as pi-tui's overlays are: its rows from
/// screen row `row` and column `col`, `width` columns wide.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    /// The first screen row.
    pub row: usize,
    /// The first column.
    pub col: usize,
    /// Its width in columns.
    pub width: usize,
    /// Its rows.
    pub lines: Vec<StyledLine>,
}

/// Draws `overlays` over `rows`, whose first row is screen row 0.
fn composite_overlays(rows: &mut [StyledLine], overlays: &[Overlay], width: usize) {
    for overlay in overlays {
        for (index, line) in overlay.lines.iter().enumerate() {
            if let Some(row) = rows.get_mut(overlay.row + index) {
                *row = composite(row, line, overlay.col, overlay.width, width);
            }
        }
    }
}

/// Regular mode: the document is written into the main screen and scrollback.
#[derive(Debug, Default)]
pub struct MainScreen {
    previous: Vec<String>,
    previous_width: usize,
    previous_height: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_viewport_top: usize,
    /// Redraw everything when the document shrinks (`terminal.clearOnShrink`).
    pub clear_on_shrink: bool,
    /// Show the terminal cursor at the frame cursor.
    pub show_hardware_cursor: bool,
    /// Components drawn over the visible rows.
    pub overlays: Vec<Overlay>,
}

/// Moves the cursor `diff` rows down, or up when negative.
fn move_rows(out: &mut String, diff: isize) {
    if diff > 0 {
        out.push_str(&format!("\x1b[{diff}B"));
    } else if diff < 0 {
        out.push_str(&format!("\x1b[{}A", -diff));
    }
}

impl MainScreen {
    /// A renderer that has drawn nothing yet.
    pub fn new() -> MainScreen {
        MainScreen::default()
    }

    /// Forgets what was drawn, so the next frame is drawn in full.
    pub fn reset(&mut self) {
        let keep = (self.clear_on_shrink, self.show_hardware_cursor);
        *self = MainScreen::default();
        (self.clear_on_shrink, self.show_hardware_cursor) = keep;
    }

    fn position_cursor(&mut self, out: &mut String, cursor: Cursor, total: usize) {
        let Some((row, col)) = cursor.filter(|_| total > 0) else {
            out.push_str("\x1b[?25l");
            return;
        };
        let target = row.min(total - 1);
        move_rows(out, target as isize - self.hardware_cursor_row as isize);
        out.push_str(&format!("\x1b[{}G", col + 1));
        self.hardware_cursor_row = target;
        out.push_str(if self.show_hardware_cursor {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        });
    }

    fn full_render(
        &mut self,
        out: &mut String,
        lines: Vec<String>,
        clear: bool,
        cursor: Cursor,
        width: usize,
        height: usize,
    ) {
        out.push_str(SYNC_START);
        if clear {
            out.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        out.push_str(&lines.join("\r\n"));
        out.push_str(SYNC_END);
        self.hardware_cursor_row = lines.len().saturating_sub(1);
        self.max_lines_rendered = if clear {
            lines.len()
        } else {
            self.max_lines_rendered.max(lines.len())
        };
        self.previous_viewport_top = height.max(lines.len()) - height;
        self.position_cursor(out, cursor, lines.len());
        self.previous = lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    /// Draws a frame of `width` by `height` and returns the bytes to write.
    pub fn frame(
        &mut self,
        document: &[StyledLine],
        cursor: Cursor,
        width: usize,
        height: usize,
    ) -> String {
        let mut out = String::new();
        let lines: Vec<String> = if self.overlays.is_empty() {
            document.iter().map(|line| clip(line, width)).collect()
        } else {
            // As pi-tui, the document grows to the screen's height so
            // overlays sit at screen positions.
            let needed = self
                .overlays
                .iter()
                .map(|overlay| overlay.row + overlay.lines.len())
                .fold(document.len().max(height), usize::max);
            let mut rows = document.to_vec();
            rows.resize(needed, Line::default());
            let start = needed - height;
            composite_overlays(&mut rows[start..], &self.overlays, width);
            rows.iter().map(|line| clip(line, width)).collect()
        };
        // Only a cursor within the visible viewport counts.
        let viewport_start = lines.len().saturating_sub(height);
        let cursor = cursor.filter(|(row, _)| *row >= viewport_start && *row < lines.len());
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        let previous_buffer = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height
        } else {
            height
        };
        let mut prev_top = if height_changed {
            previous_buffer.saturating_sub(height)
        } else {
            self.previous_viewport_top
        };
        let mut top = prev_top;
        let mut hardware_row = self.hardware_cursor_row;

        // The first frame draws without clearing; a resize, or a shrink with
        // `clearOnShrink`, redraws everything.
        let first_frame = self.previous.is_empty() && !width_changed && !height_changed;
        if first_frame
            || width_changed
            || height_changed
            || (self.clear_on_shrink && lines.len() < self.max_lines_rendered)
        {
            self.full_render(&mut out, lines, !first_frame, cursor, width, height);
            return out;
        }

        let longest = lines.len().max(self.previous.len());
        let mut first = None;
        let mut last = 0;
        for index in 0..longest {
            let old = self.previous.get(index).map_or("", String::as_str);
            let new = lines.get(index).map_or("", String::as_str);
            if old != new {
                first.get_or_insert(index);
                last = index;
            }
        }
        let appended = lines.len() > self.previous.len();
        if appended {
            first.get_or_insert(self.previous.len());
            last = lines.len() - 1;
        }
        let Some(first) = first else {
            self.position_cursor(&mut out, cursor, lines.len());
            self.previous_viewport_top = prev_top;
            self.previous_height = height;
            return out;
        };
        let append_start = appended && first == self.previous.len() && first > 0;
        let line_diff =
            |target: usize, hardware_row: usize, prev_top: usize, top: usize| -> isize {
                (target as isize - top as isize) - (hardware_row as isize - prev_top as isize)
            };

        if first >= lines.len() {
            // Only deletions at the end.
            if self.previous.len() > lines.len() {
                let target = lines.len().saturating_sub(1);
                if target < prev_top {
                    self.full_render(&mut out, lines, true, cursor, width, height);
                    return out;
                }
                let extra = self.previous.len() - lines.len();
                if extra > height {
                    self.full_render(&mut out, lines, true, cursor, width, height);
                    return out;
                }
                out.push_str(SYNC_START);
                move_rows(&mut out, line_diff(target, hardware_row, prev_top, top));
                out.push('\r');
                let start_offset = usize::from(!lines.is_empty());
                if start_offset > 0 {
                    out.push_str(&format!("\x1b[{start_offset}B"));
                }
                for index in 0..extra {
                    out.push_str("\r\x1b[2K");
                    if index + 1 < extra {
                        out.push_str("\x1b[1B");
                    }
                }
                let back = extra - 1 + start_offset;
                if back > 0 {
                    out.push_str(&format!("\x1b[{back}A"));
                }
                out.push_str(SYNC_END);
                self.hardware_cursor_row = target;
            }
            self.position_cursor(&mut out, cursor, lines.len());
            self.previous = lines;
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_top;
            return out;
        }

        if first < prev_top {
            // The change scrolled out of view: redraw the whole document.
            self.full_render(&mut out, lines, true, cursor, width, height);
            return out;
        }

        out.push_str(SYNC_START);
        let prev_bottom = prev_top + height - 1;
        let move_target = if append_start { first - 1 } else { first };
        if move_target > prev_bottom {
            let screen_row = hardware_row.saturating_sub(prev_top).min(height - 1);
            let to_bottom = height - 1 - screen_row;
            if to_bottom > 0 {
                out.push_str(&format!("\x1b[{to_bottom}B"));
            }
            let scroll = move_target - prev_bottom;
            out.push_str(&"\r\n".repeat(scroll));
            prev_top += scroll;
            top += scroll;
            hardware_row = move_target;
        }
        move_rows(
            &mut out,
            line_diff(move_target, hardware_row, prev_top, top),
        );
        out.push_str(if append_start { "\r\n" } else { "\r" });
        let render_end = last.min(lines.len() - 1);
        for (offset, line) in lines[first..=render_end].iter().enumerate() {
            if offset > 0 {
                out.push_str("\r\n");
            }
            out.push_str("\x1b[2K");
            out.push_str(line);
        }
        let mut final_row = render_end;
        if self.previous.len() > lines.len() {
            if render_end < lines.len() - 1 {
                out.push_str(&format!("\x1b[{}B", lines.len() - 1 - render_end));
                final_row = lines.len() - 1;
            }
            let extra = self.previous.len() - lines.len();
            for _ in 0..extra {
                out.push_str("\r\n\x1b[2K");
            }
            out.push_str(&format!("\x1b[{extra}A"));
        }
        out.push_str(SYNC_END);
        self.hardware_cursor_row = final_row;
        self.max_lines_rendered = self.max_lines_rendered.max(lines.len());
        self.previous_viewport_top = prev_top.max((final_row + 1).saturating_sub(height));
        self.position_cursor(&mut out, cursor, lines.len());
        self.previous = lines;
        self.previous_width = width;
        self.previous_height = height;
        out
    }

    /// Moves below the document before the program exits or hands over the
    /// terminal.
    pub fn stop(&mut self) -> String {
        if self.previous.is_empty() {
            return String::new();
        }
        let mut out = String::from(" ");
        let target = self.previous.len();
        move_rows(
            &mut out,
            target as isize - self.hardware_cursor_row as isize,
        );
        out.push_str("\r\n");
        out
    }
}

/// pi's `replaceScrollbarCell` for the last column: the line up to it, a
/// wide character crossing it given up for spaces, then `glyph`, over the
/// cell's own background when `keep_background`.
fn replace_last_cell(
    line: &StyledLine,
    width: usize,
    glyph: &str,
    style: Style,
    keep_background: bool,
) -> StyledLine {
    let column = width - 1;
    let mut background = None;
    let mut at = 0;
    for span in &line.spans {
        let span_width = line_width(&Line::from(span.clone()));
        if at + span_width > column {
            background = span.style.bg;
            break;
        }
        at += span_width;
    }
    let mut out = truncate(line, column, "");
    let short = column.saturating_sub(line_width(&out));
    if short > 0 {
        out.spans.push(Span::raw(" ".repeat(short)));
    }
    let mut style = style;
    if keep_background && let Some(background) = background {
        style = style.bg(background);
    }
    out.spans.push(Span::styled(glyph.to_owned(), style));
    out
}

/// pi-tui's `allocateStackSizes` for a stack that only shrinks: the heights
/// of parts with natural heights `sizes` and minimums `minimums` in
/// `available` rows. Each pass takes from every part above its minimum in
/// proportion to its height, at least one row each, until the stack fits.
pub(crate) fn shrink_stack(sizes: &[usize], minimums: &[usize], available: usize) -> Vec<usize> {
    let mut sizes: Vec<usize> = sizes
        .iter()
        .zip(minimums)
        .map(|(size, min)| (*size).max(*min))
        .collect();
    let total: usize = sizes.iter().sum();
    let mut remaining = total.saturating_sub(available);
    while remaining > 0 {
        let candidates: Vec<usize> = (0..sizes.len())
            .filter(|index| sizes[*index] > minimums[*index])
            .collect();
        if candidates.is_empty() {
            break;
        }
        let total_weight: usize = candidates.iter().map(|index| sizes[*index].max(1)).sum();
        let mut distributed = 0;
        for index in candidates {
            if remaining == 0 {
                break;
            }
            let weight = sizes[index].max(1);
            let proposed = (remaining * weight / total_weight).max(1);
            let delta = remaining.min(proposed).min(sizes[index] - minimums[index]);
            if delta == 0 {
                continue;
            }
            sizes[index] -= delta;
            remaining -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            break;
        }
    }
    sizes
}

/// One part of a stack: its rows and its minimum height.
pub type StackPart = (Vec<StyledLine>, usize);

/// Where a stack part landed: its first row in the stack, its first row
/// shown and how many of its rows show.
pub type Placement = (usize, usize, usize);

/// Lays out a vertical stack of `parts`, each its rows and minimum height,
/// in `available` rows, as pi-tui's layout does: a part shorter than its
/// minimum gets blank rows below it, parts shrink by [`shrink_stack`], and
/// a cut part keeps its top rows unless the cursor,
/// given as part, row and column, would fall below them. Returns the rows,
/// the cursor within them and where each part landed.
pub fn fit_stack(
    parts: Vec<StackPart>,
    cursor: Option<(usize, usize, usize)>,
    available: usize,
) -> (Vec<StyledLine>, Option<(usize, usize)>, Vec<Placement>) {
    let natural: Vec<usize> = parts
        .iter()
        .map(|(rows, min)| rows.len().max(*min))
        .collect();
    let minimums: Vec<usize> = parts.iter().map(|(_, min)| *min).collect();
    let sizes = if natural.iter().sum::<usize>() > available {
        shrink_stack(&natural, &minimums, available)
    } else {
        natural
    };
    let mut out = Vec::new();
    let mut frame_cursor = None;
    let mut placements = Vec::new();
    for (index, ((mut rows, _), size)) in parts.into_iter().zip(sizes).enumerate() {
        let cursor_row = cursor.filter(|(part, _, _)| *part == index);
        let offset = match cursor_row {
            Some((_, row, _)) if rows.len() > size && size > 0 && row >= size => row + 1 - size,
            _ => 0,
        };
        if let Some((_, row, col)) = cursor_row
            && row >= offset
            && row < offset + size
        {
            frame_cursor = Some((out.len() + row - offset, col));
        }
        rows.resize(rows.len().max(offset + size), Line::default());
        placements.push((out.len(), offset, size));
        out.extend(rows.drain(offset..offset + size));
    }
    // Minimums can exceed the room; the stack's rectangle clips its bottom.
    out.truncate(available);
    (
        out,
        frame_cursor.filter(|(row, _)| *row < available),
        placements,
    )
}

/// Fullscreen mode: the transcript scrolls above a dock pinned to the bottom.
#[derive(Debug, Default)]
pub struct AltScreen {
    previous: Vec<String>,
    previous_size: (usize, usize),
    /// The cursor bytes of the last frame.
    previous_cursor: String,
    /// First transcript row shown; `None` follows the end.
    scroll_top: Option<usize>,
    viewport: usize,
    transcript_len: usize,
    /// Show the terminal cursor at the frame cursor.
    pub show_hardware_cursor: bool,
    /// Style of the "jump to latest" label.
    pub jump_label_style: Style,
    /// Key shown in the "jump to latest" label.
    pub bottom_key: String,
    /// Components drawn over the screen.
    pub overlays: Vec<Overlay>,
    /// When the transcript's scrollbar shows; pi's `fullscreenScrollbar`.
    pub scrollbar: Scrollbar,
    /// Style of the scrollbar's track.
    pub scrollbar_track: Style,
    /// Style of the scrollbar's thumb.
    pub scrollbar_thumb: Style,
    /// Until when an `auto` scrollbar shows after scrolling.
    scrollbar_until: Option<Instant>,
    /// Copy a selection when the mouse button is released; pi's
    /// `fullscreenCopyOnSelect`.
    pub copy_on_select: bool,
    /// Rows a wheel event scrolls, or `None` for `auto`; pi's
    /// `fullscreenWheelScrollLines`.
    pub wheel_lines: Option<usize>,
    /// The wheel's acceleration in `auto` mode.
    pub wheel: WheelScroll,
    /// The pointer is over the scrollbar, which shows its thumb solid.
    scrollbar_hover: bool,
    /// The "jump to latest" label's row, first column and width.
    jump_label: Option<(usize, usize, usize)>,
    /// The rows of the last frame.
    screen: Vec<StyledLine>,
    /// The width and height of the last frame.
    size: (usize, usize),
    selection: selection::Selection,
    /// Messages at the top right, and when each goes.
    flashes: Vec<(String, Instant)>,
}

/// How long pi's `auto` scrollbar stays after the view scrolls.
const SCROLLBAR_HIDE_DELAY: Duration = Duration::from_millis(1000);

/// How long pi's flash messages show.
pub const FLASH_DURATION: Duration = Duration::from_millis(1000);
/// How long pi shows a failed copy's message.
pub const COPY_ERROR_FLASH_DURATION: Duration = Duration::from_millis(5000);

/// Bytes that enter the alternate screen with mouse and focus reports. As in
/// pi, the terminal also reports pointer moves without a button held, for
/// the scrollbar's hover, except in terminal multiplexers, which can lag
/// forwarding them.
pub fn alt_screen_enter() -> String {
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let multiplexer = ["TMUX", "ZELLIJ", "STY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
        || term.starts_with("tmux")
        || term.starts_with("screen");
    let moves = if multiplexer { "" } else { "\x1b[?1003h" };
    format!(
        "\x1b[?1049h\x1b[?7l\x1b[?1000h\x1b[?1002h{moves}\x1b[?1004h\x1b[?1006h\x1b[2J\x1b[H\x1b[?25l"
    )
}
/// Bytes that leave the alternate screen.
pub const ALT_SCREEN_LEAVE: &str =
    "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?7h\x1b[?1049l\x1b[?25h";

impl AltScreen {
    /// A renderer that has drawn nothing yet.
    pub fn new() -> AltScreen {
        AltScreen {
            bottom_key: "End".to_owned(),
            copy_on_select: true,
            wheel_lines: Some(1),
            wheel: WheelScroll::new(),
            ..AltScreen::default()
        }
    }

    /// Forgets what was drawn, the selection and the flashes, as pi does on
    /// entering the screen, so the next frame repaints everything.
    pub fn invalidate(&mut self) {
        self.previous.clear();
        self.previous_size = (0, 0);
        self.previous_cursor.clear();
        self.selection = selection::Selection::default();
        self.flashes.clear();
    }

    /// pi's `getScreenLines`: the rows of the last frame.
    pub fn screen_lines(&self) -> &[StyledLine] {
        &self.screen
    }

    /// pi's `flash`: shows `message` at the top right of the screen for
    /// `duration`, below the messages already there.
    pub fn flash(&mut self, message: impl Into<String>, duration: Duration) {
        self.flashes
            .push((message.into(), Instant::now() + duration));
    }

    /// When the screen changes on its own: an `auto` scrollbar hides, a
    /// flash ends or a drag held at the transcript's edge scrolls it. A frame
    /// drawn then shows the change.
    pub fn deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        self.flashes
            .iter()
            .map(|(_, until)| *until)
            .chain(self.scrollbar_until)
            .chain(self.autoscroll_deadline())
            .filter(|at| *at > now)
            .min()
    }

    fn max_scroll(&self) -> usize {
        self.transcript_len.saturating_sub(self.viewport)
    }

    /// Scrolls by `rows` (negative is up). Reaching the end resumes following.
    pub fn scroll_by(&mut self, rows: isize) {
        let max = self.max_scroll();
        let current = self.scroll_top.unwrap_or(max) as isize;
        let next = (current + rows).clamp(0, max as isize) as usize;
        if next as isize != current {
            self.scrolled();
        }
        self.scroll_top = (next < max).then_some(next);
    }

    /// pi's `markScrollbarActivity`: an `auto` scrollbar shows for a moment.
    fn scrolled(&mut self) {
        if self.scrollbar == Scrollbar::Auto && self.transcript_len > self.viewport {
            self.scrollbar_until = Some(Instant::now() + SCROLLBAR_HIDE_DELAY);
        }
    }

    /// When a shown `auto` scrollbar hides.
    fn scrollbar_deadline(&self) -> Option<Instant> {
        self.scrollbar_until.filter(|until| *until > Instant::now())
    }

    /// The transcript's width on a screen `width` wide: an `always`
    /// scrollbar keeps the last column, as pi's `ScrollView` does.
    pub fn content_width(&self, width: usize) -> usize {
        if self.scrollbar == Scrollbar::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    /// Scrolls by a page: the viewport less four rows.
    pub fn page(&mut self, direction: isize) {
        let rows = self.viewport.saturating_sub(4).max(1) as isize;
        self.scroll_by(direction * rows);
    }

    /// Scrolls by half the viewport.
    pub fn half_page(&mut self, direction: isize) {
        let rows = (self.viewport / 2).max(1) as isize;
        self.scroll_by(direction * rows);
    }

    /// The first transcript row shown.
    pub fn scroll_position(&self) -> usize {
        self.scroll_top.unwrap_or(self.max_scroll())
    }

    /// Shows transcript row `row` at the top, as far as the transcript
    /// allows; the end resumes following.
    pub fn scroll_to(&mut self, row: usize) {
        let max = self.max_scroll();
        let next = row.min(max);
        if next != self.scroll_position() {
            self.scrolled();
        }
        self.scroll_top = (next < max).then_some(next);
    }

    /// Scrolls to the top.
    pub fn top(&mut self) {
        let top = (self.max_scroll() > 0).then_some(0);
        if top != self.scroll_top {
            self.scrolled();
        }
        self.scroll_top = top;
    }

    /// Scrolls to the end and follows new output.
    pub fn bottom(&mut self) {
        if self.scroll_top.is_some() {
            self.scrolled();
        }
        self.scroll_top = None;
    }

    /// Draws a frame and returns the bytes to write. `cursor` is in dock
    /// coordinates.
    pub fn frame(
        &mut self,
        transcript: &[StyledLine],
        dock: &[StyledLine],
        cursor: Cursor,
        width: usize,
        height: usize,
    ) -> String {
        let mut out = String::new();
        if self.previous_size != (width, height) {
            out.push_str("\x1b[2J");
            self.previous.clear();
        }
        let height = height.max(1);
        let dock_rows = dock.len().min(height.saturating_sub(1));
        let dock_skip = dock.len() - dock_rows;
        let viewport = height - dock_rows;
        self.viewport = viewport;
        self.transcript_len = transcript.len();
        self.size = (width, height);
        self.step_autoscroll(transcript);
        let max = self.max_scroll();
        let top = self.scroll_top.map_or(max, |top| top.min(max));
        if top >= max {
            self.scroll_top = None;
        }
        let mut rows: Vec<StyledLine> = transcript
            .iter()
            .skip(top)
            .take(viewport)
            .cloned()
            .collect();
        rows.resize(viewport, Line::default());
        let scrollbar = self.paint_scrollbar(&mut rows, width);
        // pi's `compositeScrollToEndIndicator`: the label centered over the
        // last transcript row, stopping at the scrollbar.
        self.jump_label = None;
        if self.scroll_top.is_some() && viewport > 0 {
            let label = format!(" ↓ Jump to latest message · {} ", self.bottom_key);
            let label = truncate(&Line::from(label), width, "");
            let left = width.saturating_sub(line_width(&label)) / 2;
            let right_edge = if scrollbar { width - 1 } else { width };
            let label = truncate(&label, right_edge.saturating_sub(left), "");
            let label_width = line_width(&label);
            if label_width > 0 {
                let label = Line::from(crate::lines::under(label.spans, self.jump_label_style));
                rows[viewport - 1] =
                    composite(&rows[viewport - 1], &label, left, label_width, width);
                self.jump_label = Some((viewport - 1, left, label_width));
            }
        }
        rows.extend(dock.iter().skip(dock_skip).cloned());
        composite_overlays(&mut rows, &self.overlays, width);
        self.highlight(&mut rows, top, width);
        self.composite_flashes(&mut rows, width);
        for row in &mut rows {
            if line_width(row) > width {
                *row = truncate(row, width, "");
            }
        }

        let lines: Vec<String> = rows.iter().map(line_to_ansi).collect();
        self.screen = rows;
        let mut changes = String::new();
        for (index, line) in lines.iter().enumerate() {
            if self.previous.get(index) != Some(line) {
                changes.push_str(&format!("\x1b[{};1H\x1b[2K{line}", index + 1));
            }
        }
        // pi parks the cursor at the frame cursor even when it stays hidden,
        // so input methods place their candidate windows there.
        let cursor = match cursor
            .and_then(|(row, col)| row.checked_sub(dock_skip).map(|row| (viewport + row, col)))
        {
            Some((row, col)) if row < height => format!(
                "\x1b[{};{}H{}",
                row + 1,
                col.min(width) + 1,
                if self.show_hardware_cursor {
                    "\x1b[?25h"
                } else {
                    "\x1b[?25l"
                }
            ),
            _ => "\x1b[?25l".to_owned(),
        };
        // An unchanged frame writes nothing.
        if out.is_empty() && changes.is_empty() && cursor == self.previous_cursor {
            return out;
        }
        out.push_str(SYNC_START);
        out.push_str(&changes);
        out.push_str(&cursor);
        out.push_str(SYNC_END);
        self.previous = lines;
        self.previous_size = (width, height);
        self.previous_cursor = cursor;
        out
    }

    /// pi's `compositeFlashes`: each message reversed at the right end of a
    /// row, from the top.
    fn composite_flashes(&mut self, rows: &mut [StyledLine], width: usize) {
        let now = Instant::now();
        self.flashes.retain(|(_, until)| *until > now);
        let skip = self.flashes.len().saturating_sub(rows.len());
        let reversed = Style::new().add_modifier(Modifier::REVERSED);
        for (row, (message, _)) in rows.iter_mut().zip(self.flashes.iter().skip(skip)) {
            let flash = Line::from(Span::styled(format!(" {message} "), reversed));
            let flash = truncate(&flash, width, "");
            let flash_width = line_width(&flash);
            if flash_width > 0 {
                *row = composite(row, &flash, width - flash_width, flash_width, width);
            }
        }
    }

    /// pi's `getScrollbarGeometry` for the transcript: the thumb's first row
    /// and height while the scrollbar shows, or when `revealable`, while an
    /// `auto` one would show for the pointer over it.
    fn thumb(&self, revealable: bool) -> Option<(usize, usize)> {
        let (track, content) = (self.viewport, self.transcript_len);
        let visible = match self.scrollbar {
            Scrollbar::Always => track > 0,
            Scrollbar::Auto => {
                content > track
                    && (revealable || self.scrollbar_hover || self.scrollbar_deadline().is_some())
            }
            Scrollbar::Hidden => false,
        };
        if !visible || self.size.0 == 0 || content == 0 {
            return None;
        }
        let round = |value: f64| value.round() as usize;
        let thumb = round((track * track) as f64 / content as f64)
            .min(track)
            .max(2.min(track));
        let max_top = content.saturating_sub(track);
        let offset = if max_top == 0 {
            0
        } else {
            let top = self.scroll_position().min(max_top);
            round(top as f64 / max_top as f64 * (track - thumb) as f64)
        };
        Some((offset, thumb))
    }

    /// pi's `paintScrollbar`: the track and thumb in the transcript rows'
    /// last column while the scrollbar shows.
    fn paint_scrollbar(&mut self, rows: &mut [StyledLine], width: usize) -> bool {
        let Some((offset, thumb)) = self.thumb(false) else {
            if self.scrollbar_deadline().is_none() {
                self.scrollbar_until = None;
            }
            return false;
        };
        let keep_background = self.scrollbar != Scrollbar::Always;
        let glyph = if self.scrollbar_hover { "█" } else { "┃" };
        for (row, line) in rows.iter_mut().take(self.viewport).enumerate() {
            let (glyph, style) = if row >= offset && row < offset + thumb {
                (glyph, self.scrollbar_thumb)
            } else {
                ("│", self.scrollbar_track)
            };
            *line = replace_last_cell(line, width, glyph, style, keep_background);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(rows: &[&str]) -> Vec<StyledLine> {
        rows.iter().map(|row| Line::from(row.to_string())).collect()
    }

    #[test]
    fn main_screen_rewrites_only_changes() {
        let mut screen = MainScreen::new();
        let first = screen.frame(&doc(&["a", "b", "c"]), None, 10, 5);
        assert!(first.contains("a\x1b[0m\r\nb\x1b[0m\r\nc\x1b[0m"));
        let second = screen.frame(&doc(&["a", "B", "c"]), None, 10, 5);
        assert!(second.contains("\x1b[1A\r\x1b[2KB\x1b[0m"), "{second:?}");
        assert!(!second.contains("a\x1b[0m"));
        let appended = screen.frame(&doc(&["a", "B", "c", "d"]), None, 10, 5);
        assert!(appended.contains("\r\n\x1b[2Kd\x1b[0m"), "{appended:?}");
        let resized = screen.frame(&doc(&["a"]), None, 8, 5);
        assert!(resized.contains("\x1b[2J\x1b[H\x1b[3J"));
    }

    #[test]
    fn alt_screen_scrolls_and_follows() {
        let mut screen = AltScreen::new();
        let transcript = doc(&["1", "2", "3", "4", "5", "6"]);
        let dock = doc(&["editor"]);
        let frame = screen.frame(&transcript, &dock, None, 40, 4);
        assert!(frame.contains("\x1b[1;1H\x1b[2K4"));
        assert!(frame.contains("\x1b[4;1H\x1b[2Keditor"));
        screen.scroll_by(-2);
        let frame = screen.frame(&transcript, &dock, None, 40, 4);
        assert!(frame.contains("\x1b[1;1H\x1b[2K2"));
        assert!(frame.contains("Jump to latest"));
        screen.bottom();
        assert!(screen.scroll_top.is_none());
    }

    #[test]
    fn alt_screen_parks_hidden_cursor_at_frame_cursor() {
        let mut screen = AltScreen::new();
        let transcript = doc(&["1"]);
        let dock = doc(&["> hi", "footer"]);
        let frame = screen.frame(&transcript, &dock, Some((0, 4)), 40, 4);
        assert!(
            frame.ends_with("\x1b[3;5H\x1b[?25l\x1b[?2026l"),
            "{frame:?}"
        );
        screen.show_hardware_cursor = true;
        let frame = screen.frame(&transcript, &dock, Some((0, 99)), 40, 4);
        assert!(
            frame.ends_with("\x1b[3;41H\x1b[?25h\x1b[?2026l"),
            "{frame:?}"
        );
        let frame = screen.frame(&transcript, &dock, None, 40, 4);
        assert!(frame.ends_with("\x1b[?25l\x1b[?2026l"), "{frame:?}");
    }

    #[test]
    fn shrinks_stacks_as_pi_tui() {
        // Values computed by pi-tui's `allocateStackSizes`: every part above
        // its minimum gives at least a row per pass.
        assert_eq!(
            shrink_stack(&[0, 0, 1, 40, 0, 2], &[0, 0, 0, 3, 0, 0], 23),
            [0, 0, 0, 22, 0, 1]
        );
        assert_eq!(
            shrink_stack(&[0, 0, 1, 44, 0, 2], &[0, 0, 0, 3, 0, 0], 29),
            [0, 0, 0, 28, 0, 1]
        );
        assert_eq!(shrink_stack(&[2, 5], &[0, 3], 4), [0, 4]);
        assert_eq!(shrink_stack(&[2, 5], &[2, 5], 4), [2, 5]);
        assert_eq!(shrink_stack(&[4, 1], &[0, 0], 3), [3, 0]);
    }

    #[test]
    fn fit_stack_keeps_top_rows_or_the_cursor() {
        let parts = vec![(doc(&["a", "b", "c", "d"]), 0), (doc(&["f"]), 0)];
        let (rows, _, _) = fit_stack(parts.clone(), None, 3);
        let text: Vec<String> = rows.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["a", "b", "c"]);
        let (rows, cursor, _) = fit_stack(parts.clone(), Some((0, 3, 1)), 3);
        let text: Vec<String> = rows.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["b", "c", "d"]);
        assert_eq!(cursor, Some((2, 1)));
        let (rows, cursor, _) = fit_stack(parts, Some((0, 1, 1)), 9);
        assert_eq!(rows.len(), 5);
        assert_eq!(cursor, Some((1, 1)));
        // A minimum taller than the room is clipped at the bottom.
        let editor = vec![(doc(&["─", "text", "─"]), 3), (doc(&["footer"]), 0)];
        let (rows, cursor, _) = fit_stack(editor, Some((0, 1, 4)), 2);
        let text: Vec<String> = rows.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["─", "text"]);
        assert_eq!(cursor, Some((1, 4)));
        // A part shorter than its minimum keeps its rows at the top.
        let component = vec![(doc(&["one line"]), 3), (doc(&["footer"]), 0)];
        let (rows, _, _) = fit_stack(component, None, 9);
        let text: Vec<String> = rows.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["one line", "", "", "footer"]);
    }

    #[test]
    fn alt_screen_draws_pi_scrollbars() {
        let transcript = doc(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]);
        let dock = doc(&["editor"]);
        // The text a frame writes to `row`, up to its next escape.
        let last_column = |frame: &str, row: usize| {
            let prefix = format!("\x1b[{row};1H\x1b[2K");
            let start = frame
                .find(&prefix)
                .unwrap_or_else(|| panic!("row {row} in {frame:?}"))
                + prefix.len();
            let rest = &frame[start..];
            rest[..rest.find("\x1b[").unwrap_or(rest.len())].to_owned()
        };
        // `always`: four rows over ten; thumb of round(16 / 10) = 2 rows at
        // the end, since the view follows the end.
        let mut screen = AltScreen::new();
        screen.scrollbar = Scrollbar::Always;
        let frame = screen.frame(&transcript, &dock, None, 10, 5);
        assert!(last_column(&frame, 1).contains('│'), "{frame:?}");
        assert!(last_column(&frame, 3).contains('┃'), "{frame:?}");
        assert!(last_column(&frame, 4).contains('┃'), "{frame:?}");
        // `auto` shows only after scrolling, and `hidden` never.
        let mut screen = AltScreen::new();
        let frame = screen.frame(&transcript, &dock, None, 10, 5);
        assert!(!frame.contains('│') && !frame.contains('┃'), "{frame:?}");
        screen.top();
        let frame = screen.frame(&transcript, &dock, None, 10, 5);
        assert!(last_column(&frame, 1).contains('┃'), "{frame:?}");
        assert!(screen.scrollbar_deadline().is_some());
        let mut screen = AltScreen::new();
        screen.scrollbar = Scrollbar::Hidden;
        screen.frame(&transcript, &dock, None, 10, 5);
        screen.top();
        let frame = screen.frame(&transcript, &dock, None, 10, 5);
        assert!(!frame.contains('┃'), "{frame:?}");
    }

    #[test]
    fn alt_screen_writes_nothing_for_an_unchanged_frame() {
        let mut screen = AltScreen::new();
        let transcript = doc(&["1"]);
        let dock = doc(&["> hi", "footer"]);
        assert!(
            !screen
                .frame(&transcript, &dock, Some((0, 4)), 40, 4)
                .is_empty()
        );
        assert_eq!(screen.frame(&transcript, &dock, Some((0, 4)), 40, 4), "");
        let moved = screen.frame(&transcript, &dock, Some((0, 3)), 40, 4);
        assert!(moved.contains("\x1b[3;4H"), "{moved:?}");
        screen.invalidate();
        assert!(
            !screen
                .frame(&transcript, &dock, Some((0, 3)), 40, 4)
                .is_empty()
        );
    }
}
