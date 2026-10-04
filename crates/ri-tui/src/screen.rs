//! Writing frames of styled lines to the terminal.
//!
//! [`MainScreen`] is pi's regular mode (`packages/tui/src/tui-main-screen.ts`
//! in pi `v1.0.0`): the document flows into the terminal's scrollback and only
//! changed lines are rewritten. [`AltScreen`] is the fullscreen mode
//! (`tui-alt-screen.ts`): a scrolling transcript above a dock pinned to the
//! bottom of the alternate screen.

use std::io::{self, Write};

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};

use crate::ansi::line_to_ansi;
use crate::lines::{StyledLine, composite, pad, truncate, width as line_width};

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
pub type Cursor = Option<(usize, usize)>;

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
        if target > self.hardware_cursor_row {
            out.push_str(&format!("\x1b[{}B", target - self.hardware_cursor_row));
        } else if target < self.hardware_cursor_row {
            out.push_str(&format!("\x1b[{}A", self.hardware_cursor_row - target));
        }
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

        if self.previous.is_empty() && !width_changed && !height_changed {
            self.full_render(&mut out, lines, false, cursor, width, height);
            return out;
        }
        if width_changed || height_changed {
            self.full_render(&mut out, lines, true, cursor, width, height);
            return out;
        }
        if self.clear_on_shrink && lines.len() < self.max_lines_rendered {
            self.full_render(&mut out, lines, true, cursor, width, height);
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
        let move_by = |out: &mut String, diff: isize| {
            if diff > 0 {
                out.push_str(&format!("\x1b[{diff}B"));
            } else if diff < 0 {
                out.push_str(&format!("\x1b[{}A", -diff));
            }
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
                move_by(&mut out, line_diff(target, hardware_row, prev_top, top));
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
        move_by(
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
        if target > self.hardware_cursor_row {
            out.push_str(&format!("\x1b[{}B", target - self.hardware_cursor_row));
        } else if target < self.hardware_cursor_row {
            out.push_str(&format!("\x1b[{}A", self.hardware_cursor_row - target));
        }
        out.push_str("\r\n");
        out
    }
}

/// Fullscreen mode: the transcript scrolls above a dock pinned to the bottom.
#[derive(Debug, Default)]
pub struct AltScreen {
    previous: Vec<String>,
    previous_size: (usize, usize),
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
}

/// Bytes that enter the alternate screen with wheel reporting.
pub const ALT_SCREEN_ENTER: &str =
    "\x1b[?1049h\x1b[?7l\x1b[?1000h\x1b[?1006h\x1b[2J\x1b[H\x1b[?25l";
/// Bytes that leave the alternate screen.
pub const ALT_SCREEN_LEAVE: &str = "\x1b[?1006l\x1b[?1000l\x1b[?7h\x1b[?1049l\x1b[?25h";

impl AltScreen {
    /// A renderer that has drawn nothing yet.
    pub fn new() -> AltScreen {
        AltScreen {
            bottom_key: "End".to_owned(),
            ..AltScreen::default()
        }
    }

    /// Forgets what was drawn, so the next frame repaints everything.
    pub fn invalidate(&mut self) {
        self.previous.clear();
        self.previous_size = (0, 0);
    }

    fn max_scroll(&self) -> usize {
        self.transcript_len.saturating_sub(self.viewport)
    }

    /// Scrolls by `rows` (negative is up). Reaching the end resumes following.
    pub fn scroll_by(&mut self, rows: isize) {
        let max = self.max_scroll();
        let current = self.scroll_top.unwrap_or(max) as isize;
        let next = (current + rows).clamp(0, max as isize) as usize;
        self.scroll_top = (next < max).then_some(next);
    }

    /// Scrolls by a page: the viewport less four rows.
    pub fn page(&mut self, direction: isize) {
        let rows = self.viewport.saturating_sub(4).max(1) as isize;
        self.scroll_by(direction * rows);
    }

    /// Scrolls to the top.
    pub fn top(&mut self) {
        self.scroll_top = (self.max_scroll() > 0).then_some(0);
    }

    /// Scrolls to the end and follows new output.
    pub fn bottom(&mut self) {
        self.scroll_top = None;
    }

    /// Whether the view follows new output.
    pub fn following(&self) -> bool {
        self.scroll_top.is_none()
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
        if self.scroll_top.is_some() && viewport > 0 {
            let label = format!(" ↓ Jump to latest message · {} ", self.bottom_key);
            let label = truncate(&Line::from(label), width, "");
            let label_width = line_width(&label);
            let left = width.saturating_sub(label_width) / 2;
            let mut spans = vec![Span::raw(" ".repeat(left))];
            spans.extend(label.spans.into_iter().map(|span| {
                let style = self.jump_label_style.patch(span.style);
                Span::styled(span.content, style)
            }));
            rows[viewport - 1] = pad(Line::from(spans), width);
        }
        rows.extend(dock.iter().skip(dock_skip).cloned());
        composite_overlays(&mut rows, &self.overlays, width);

        let lines: Vec<String> = rows.iter().map(|line| clip(line, width)).collect();
        out.push_str(SYNC_START);
        for (index, line) in lines.iter().enumerate() {
            if self.previous.get(index) != Some(line) {
                out.push_str(&format!("\x1b[{};1H\x1b[2K{line}", index + 1));
            }
        }
        // pi parks the cursor at the frame cursor even when it stays hidden,
        // so input methods place their candidate windows there.
        match cursor
            .and_then(|(row, col)| row.checked_sub(dock_skip).map(|row| (viewport + row, col)))
        {
            Some((row, col)) if row < height => {
                out.push_str(&format!("\x1b[{};{}H", row + 1, col.min(width) + 1));
                out.push_str(if self.show_hardware_cursor {
                    "\x1b[?25h"
                } else {
                    "\x1b[?25l"
                });
            }
            _ => out.push_str("\x1b[?25l"),
        }
        out.push_str(SYNC_END);
        self.previous = lines;
        self.previous_size = (width, height);
        out
    }
}

/// Writes `data` and flushes.
pub fn write_all(out: &mut impl Write, data: &str) -> io::Result<()> {
    out.write_all(data.as_bytes())?;
    out.flush()
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
        assert!(screen.following());
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
}
