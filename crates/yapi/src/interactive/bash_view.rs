//! A user `!` command in the transcript, with its streaming output.
//!
//! Port of `components/bash-execution.ts` in
//! `packages/coding-agent/src/modes/interactive` in pi `v1.0.0`.

use std::time::Instant;

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use yapi_core::bash_executor::{BashResult, strip_ansi};
use yapi_core::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};
use yapi_tui::lines::{self, StyledLine, styled};

use super::chat::RenderContext;
use super::{SPINNER, SPINNER_INTERVAL};

const PREVIEW_LINES: usize = 20;

/// A `!` command and its output.
pub struct BashView {
    /// Identifies the command's output events.
    pub id: u64,
    command: String,
    exclude_from_context: bool,
    lines: Vec<String>,
    result: Option<BashResult>,
    started: Instant,
    updated: bool,
}

impl BashView {
    /// A running command; `exclude_from_context` is the `!!` form.
    pub fn new(id: u64, command: &str, exclude_from_context: bool) -> BashView {
        BashView {
            id,
            command: command.to_owned(),
            exclude_from_context,
            lines: Vec::new(),
            result: None,
            started: Instant::now(),
            updated: false,
        }
    }

    /// Whether the command still runs.
    pub fn running(&self) -> bool {
        self.result.is_none()
    }

    /// Appends streamed output.
    pub fn append(&mut self, chunk: &str) {
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = clean.split('\n');
        match (self.lines.last_mut(), parts.next()) {
            (Some(last), Some(first)) => last.push_str(first),
            (None, Some(first)) => self.lines.push(first.to_owned()),
            _ => {}
        }
        self.lines.extend(parts.map(str::to_owned));
        self.updated = true;
    }

    /// A finished command from the session, as history shows it.
    pub fn from_message(message: &yapi_types::message::BashExecutionMessage) -> BashView {
        let mut view = BashView::new(
            0,
            &message.command,
            message.exclude_from_context == Some(true),
        );
        view.append(&message.output);
        view.finish(BashResult {
            output: message.output.clone(),
            exit_code: message.exit_code.map(|code| code as i32),
            cancelled: message.cancelled,
            truncated: message.truncated,
            full_output_path: message.full_output_path.clone(),
        });
        view
    }

    /// Records how the command ended.
    pub fn finish(&mut self, result: BashResult) {
        self.result = Some(result);
        self.updated = true;
    }

    /// The rows at `width`.
    pub fn render(&self, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let color = if self.exclude_from_context {
            "dim"
        } else {
            "bashMode"
        };
        let border = lines::border(width, theme.fg(color));
        let mut out = lines::spacer(1);
        out.push(border.clone());
        // pi recolors the header with the bash color once output or the result
        // arrives, even for `!!` commands.
        let header_color = if self.updated { "bashMode" } else { color };
        out.extend(lines::text_row(
            styled(
                format!("$ {}", self.command),
                theme.fg(header_color).add_modifier(Modifier::BOLD),
            ),
            width,
            1,
        ));
        let full = self.lines.join("\n");
        let truncation = truncate_tail(&full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let available: Vec<&str> = if truncation.content.is_empty() {
            Vec::new()
        } else {
            truncation.content.split('\n').collect()
        };
        let preview_start = available.len().saturating_sub(PREVIEW_LINES);
        let hidden = preview_start;
        if !available.is_empty() {
            let muted = theme.fg("muted");
            let shown = if ctx.expanded {
                &available[..]
            } else {
                &available[preview_start..]
            };
            let mut content = vec![Line::default()];
            content.extend(shown.iter().map(|line| styled(*line, muted)));
            let rows = lines::text(&content, width, 1, 0, None);
            if ctx.expanded {
                out.extend(rows);
            } else {
                let skip = rows.len().saturating_sub(PREVIEW_LINES);
                out.extend(rows.into_iter().skip(skip));
            }
        }
        match &self.result {
            None => {
                let frame = SPINNER[(self.started.elapsed().as_millis()
                    / SPINNER_INTERVAL.as_millis()) as usize
                    % SPINNER.len()];
                out.push(Line::default());
                out.extend(lines::text_row(
                    Line::from(vec![
                        Span::styled(frame, theme.fg(color)),
                        Span::raw(" "),
                        Span::styled(
                            format!("Running... ({} to cancel)", ctx.cancel_key),
                            theme.fg("muted"),
                        ),
                    ]),
                    width,
                    1,
                ));
            }
            Some(result) => {
                let muted = theme.fg("muted");
                let dim = theme.fg("dim");
                let mut parts: Vec<StyledLine> = Vec::new();
                if hidden > 0 {
                    parts.push(Line::from(if ctx.expanded {
                        vec![
                            Span::styled("(", muted),
                            Span::styled(ctx.expand_key.to_owned(), dim),
                            Span::styled(" to collapse", muted),
                            Span::styled(")", muted),
                        ]
                    } else {
                        vec![
                            Span::styled(format!("... {hidden} more lines ("), muted),
                            Span::styled(ctx.expand_key.to_owned(), dim),
                            Span::styled(" to expand", muted),
                            Span::styled(")", muted),
                        ]
                    }));
                }
                if result.cancelled {
                    parts.push(styled("(cancelled)", theme.fg("warning")));
                } else if let Some(code) = result.exit_code.filter(|code| *code != 0) {
                    parts.push(styled(format!("(exit {code})"), theme.fg("error")));
                }
                if (result.truncated || truncation.truncated)
                    && let Some(path) = &result.full_output_path
                {
                    parts.push(styled(
                        format!("Output truncated. Full output: {path}"),
                        theme.fg("warning"),
                    ));
                }
                if !parts.is_empty() {
                    let mut content = vec![Line::default()];
                    content.extend(parts);
                    out.extend(lines::text(&content, width, 1, 0, None));
                }
            }
        }
        out.push(border);
        out
    }
}
