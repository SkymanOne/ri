//! Tool calls in the transcript.
//!
//! Ports of `components/tool-execution.ts`, `components/diff.ts`,
//! `components/visual-truncate.ts` and `core/tools/renderers/*.ts` in pi
//! `v1.0.0`.

use std::time::Instant;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::Value;
use yapi_core::bash_executor::{sanitize_binary, strip_ansi};
use yapi_core::tools::truncate::format_size;
use yapi_tui::lines::{self, StyledLine, box_content_width, boxed};
use yapi_tui::theme::Theme;
use yapi_types::event::ToolResult;
use yapi_types::message::ContentBlock;

use super::chat::RenderContext;
use super::selectors::key_hint;

/// A tool call, its streamed progress and its result.
pub struct ToolView {
    /// The tool name.
    pub name: String,
    /// Arguments, complete once the call finished streaming.
    pub args: Value,
    /// The latest partial result while running.
    pub partial: Option<ToolResult>,
    /// The final result.
    pub result: Option<ToolResult>,
    /// The final result is an error.
    pub is_error: bool,
    /// When execution started.
    pub started: Option<Instant>,
    /// When execution finished.
    pub finished: Option<Instant>,
    /// The extension that draws the call and result, when one does.
    pub draw: Option<ToolDraw>,
    /// Whether the session has a tool of this name. pi shows a call to any
    /// other tool, which the model made up, as plain text.
    pub known: bool,
    /// For a built-in `edit` call: the diff its edits would make, or why
    /// they cannot apply, worked out once its arguments are complete.
    pub edit_preview: Option<Result<String, String>>,
    /// For an MCP server's tool: its `server/tool` label, which pi's MCP
    /// renderers show in place of the name.
    pub mcp_label: Option<String>,
}

/// Wrapped rows of MCP output that pi shows collapsed.
const MCP_PREVIEW_LINES: usize = 5;

/// pi's edit preview (`computeEditsDiff`) for a call's arguments; `None`
/// when they name no file or no complete edits.
pub fn edit_preview(args: &Value, cwd: &std::path::Path) -> Option<Result<String, String>> {
    use yapi_core::tools::{Replacement, preview_edits};
    let path = args["path"]
        .as_str()
        .or_else(|| args["file_path"].as_str())?;
    let replacement = |edit: &Value| {
        Some(Replacement {
            old_text: edit["oldText"].as_str()?.to_owned(),
            new_text: edit["newText"].as_str()?.to_owned(),
        })
    };
    let edits: Vec<Replacement> = match args["edits"].as_array() {
        Some(edits) if !edits.is_empty() => edits.iter().map(replacement).collect::<Option<_>>()?,
        _ => vec![replacement(args)?],
    };
    Some(preview_edits(path, &edits, cwd))
}

/// How an extension draws a tool (pi's `renderCall` and `renderResult`) and
/// the components it built for this call.
pub struct ToolDraw {
    /// The extension.
    pub extension: std::sync::Arc<dyn yapi_core::extensions::Extension>,
    /// What it draws.
    pub renderers: yapi_core::extensions::ToolRenderers,
    /// The call's component.
    pub call: Option<super::extension_ui::RemoteView>,
    /// The result's component.
    pub result: Option<super::extension_ui::RemoteView>,
    /// Requests sent for the call and the result, to drop stale answers.
    pub requests: [u64; 2],
}

/// pi-tui's `getImageDimensions` with the `Image` component's default: the
/// pixel size in a PNG, JPEG, GIF or WebP header, else 800×600.
pub fn image_dimensions(data: &str, mime_type: &str) -> (u32, u32) {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data.trim()) else {
        return (800, 600);
    };
    let u16_be = |at: usize| {
        Some(u32::from(u16::from_be_bytes([
            *bytes.get(at)?,
            *bytes.get(at + 1)?,
        ])))
    };
    let u16_le = |at: usize| {
        Some(u32::from(u16::from_le_bytes([
            *bytes.get(at)?,
            *bytes.get(at + 1)?,
        ])))
    };
    let u32_at = |at: usize, big: bool| {
        let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        })
    };
    let found = match mime_type {
        "image/png" if bytes.len() >= 24 && bytes.starts_with(&[0x89, b'P', b'N', b'G']) => {
            u32_at(16, true).zip(u32_at(20, true))
        }
        "image/jpeg" if bytes.starts_with(&[0xff, 0xd8]) => {
            let mut offset = 2;
            let mut size = None;
            while offset + 9 < bytes.len() {
                if bytes[offset] != 0xff {
                    offset += 1;
                    continue;
                }
                let marker = bytes[offset + 1];
                if (0xc0..=0xc2).contains(&marker) {
                    size = u16_be(offset + 7).zip(u16_be(offset + 5));
                    break;
                }
                match u16_be(offset + 2) {
                    Some(length) if length >= 2 => offset += 2 + length as usize,
                    _ => break,
                }
            }
            size
        }
        "image/gif" if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => {
            u16_le(6).zip(u16_le(8))
        }
        "image/webp"
            if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" =>
        {
            match &bytes[12..16] {
                b"VP8 " => u16_le(26)
                    .zip(u16_le(28))
                    .map(|(w, h)| (w & 0x3fff, h & 0x3fff)),
                b"VP8L" => {
                    u32_at(21, false).map(|bits| ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
                }
                b"VP8X" => {
                    let triple = |at: usize| {
                        u32::from(bytes[at])
                            | u32::from(bytes[at + 1]) << 8
                            | u32::from(bytes[at + 2]) << 16
                    };
                    Some((triple(24) + 1, triple(27) + 1))
                }
                _ => None,
            }
        }
        _ => None,
    };
    found.unwrap_or((800, 600))
}

/// pi's `getTextOutput`: text blocks joined by newlines, images as notes.
pub fn text_output(result: &ToolResult) -> String {
    let text: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => {
                Some(sanitize_binary(&strip_ansi(&text.text)).replace('\r', ""))
            }
            _ => None,
        })
        .collect();
    let mut output = text.join("\n");
    let images: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => {
                let (width, height) = image_dimensions(&image.data, &image.mime_type);
                Some(format!("[Image: [{}] {width}x{height}]", image.mime_type))
            }
            _ => None,
        })
        .collect();
    if !images.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&images.join("\n"));
    }
    output
}

/// pi's `SCRIPT_HEADER`: `Script completed|failed`, the wall time, `Output:`.
fn is_script_header(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("Script completed\nWall time ")
        .or_else(|| text.strip_prefix("Script failed\nWall time "))
    else {
        return false;
    };
    rest.strip_suffix(" seconds\nOutput:\n")
        .is_some_and(|time| {
            !time.is_empty() && time.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
}

fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

fn string_arg<'a>(args: &'a Value, keys: &[&str]) -> Option<Option<&'a str>> {
    for key in keys {
        match args.get(*key) {
            Some(Value::String(text)) => return Some(Some(text)),
            Some(Value::Null) | None => continue,
            Some(_) => return None,
        }
    }
    Some(None)
}

/// `path` with a leading `home` shown as `~`.
pub(super) fn shorten_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path.starts_with(home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// The first `limit` of `rows` in `style`, or all of them when expanded, and
/// pi's hint for the rest: `... (N more lines,`, with the row count when
/// `total`.
fn head_lines(
    rows: Vec<String>,
    limit: usize,
    style: Style,
    total: bool,
    ctx: &RenderContext<'_>,
) -> (Vec<StyledLine>, Option<StyledLine>) {
    let count = rows.len();
    let max = if ctx.expanded { count } else { limit };
    let shown = rows
        .into_iter()
        .take(max)
        .map(|row| lines::styled(row, style))
        .collect();
    let total = if total {
        format!(" {count} total,")
    } else {
        String::new()
    };
    let hint = (count > max).then(|| {
        more_lines_hint(
            ctx.theme,
            ctx,
            format!("... ({} more lines,{total}", count - max),
        )
    });
    (shown, hint)
}

fn more_lines_hint(theme: &Theme, ctx: &RenderContext<'_>, text: String) -> StyledLine {
    let mut spans = vec![Span::styled(text, theme.fg("muted")), Span::raw(" ")];
    spans.extend(key_hint(theme, ctx.expand_key, "to expand"));
    spans.push(Span::styled(")", theme.fg("muted")));
    Line::from(spans)
}

fn title(theme: &Theme, text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        theme.fg("toolTitle").add_modifier(Modifier::BOLD),
    )
}

fn path_span(
    theme: &Theme,
    path: Option<Option<&str>>,
    home: Option<&str>,
    fallback: Option<&str>,
) -> Span<'static> {
    match path {
        None => Span::styled("[invalid arg]", theme.fg("error")),
        Some(path) => match path.filter(|p| !p.is_empty()).or(fallback) {
            Some(path) => Span::styled(shorten_home(path, home), theme.fg("accent")),
            None => Span::styled("...", theme.fg("toolOutput")),
        },
    }
}

fn number_arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key)? {
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn trim_trailing_empty(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// `1.2s`, `3m 4s`, `1h 2m 3s`.
pub fn format_duration(ms: u128) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total = ms / 1000;
    let minutes = total / 60;
    let remainder = total % 60;
    if minutes < 60 {
        format!("{minutes}m {remainder}s")
    } else {
        format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
    }
}

/// The last `max` visual lines of `lines` at `width`, with a hint when some
/// are hidden.
fn tail_preview(
    lines: &[StyledLine],
    max: usize,
    width: usize,
    hint: impl Fn(usize) -> StyledLine,
) -> Vec<StyledLine> {
    let visual: Vec<StyledLine> = lines
        .iter()
        .flat_map(|line| lines::wrap(line, width))
        .collect();
    if visual.len() <= max {
        return visual;
    }
    let hidden = visual.len() - max;
    let mut out = vec![lines::truncate(&hint(hidden), width, "...")];
    out.extend(visual[hidden..].iter().cloned());
    out
}

/// pi's `VisualLinePreview` keeping the start: the first `max` visual lines,
/// then a hint for the rest.
fn head_preview(
    lines: &[StyledLine],
    max: usize,
    width: usize,
    hint: impl Fn(usize) -> StyledLine,
) -> Vec<StyledLine> {
    let mut visual: Vec<StyledLine> = lines
        .iter()
        .flat_map(|line| lines::wrap(line, width))
        .collect();
    if visual.len() <= max {
        return visual;
    }
    let hidden = visual.len() - max;
    visual.truncate(max);
    visual.push(lines::truncate(&hint(hidden), width, "..."));
    visual
}

/// The codemode renderer's durations: whole milliseconds, then seconds.
fn script_duration(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{}ms", yapi_types::js::round(ms))
    } else {
        format!("{}s", yapi_types::js::to_fixed(ms / 1000.0, 1))
    }
}

/// One nested call of a codemode script; pi's `formatCall`.
fn script_call(call: &Value, theme: &Theme, expanded: bool) -> Vec<StyledLine> {
    let (icon, color) = match call["status"].as_str() {
        Some("running") => ("…", "warning"),
        Some("ok") => ("✓", "success"),
        Some("error") => ("✗", "error"),
        _ => ("⊘", "muted"),
    };
    let args = call["args"].as_str().unwrap_or_default();
    let args = if !expanded && yapi_types::js::len(args) > 80 {
        format!("{}...", yapi_types::js::slice(args, 0, 77))
    } else {
        args.to_owned()
    };
    let mut spans = vec![
        Span::styled(icon, theme.fg(color)),
        Span::raw(" "),
        Span::styled(
            call["name"].as_str().unwrap_or_default().to_owned(),
            theme.fg("toolTitle"),
        ),
    ];
    if !args.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(args, theme.fg("muted")));
    }
    if let Some(ms) = call["durationMs"].as_f64() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(script_duration(ms), theme.fg("dim")));
    }
    let mut out = vec![Line::from(spans)];
    if expanded && let Some(error) = call["error"].as_str().filter(|error| !error.is_empty()) {
        out.extend(error.split('\n').map(|line| {
            Line::from(vec![
                Span::raw("    "),
                Span::styled(line.to_owned(), theme.fg("error")),
            ])
        }));
    }
    out
}

impl ToolView {
    /// A call whose arguments are still streaming.
    pub fn new(name: &str, args: Value) -> ToolView {
        ToolView {
            name: name.to_owned(),
            args,
            partial: None,
            result: None,
            is_error: false,
            started: None,
            finished: None,
            draw: None,
            known: true,
            edit_preview: None,
            mcp_label: None,
        }
    }

    fn shown_result(&self) -> Option<&ToolResult> {
        self.result.as_ref().or(self.partial.as_ref())
    }

    fn background(&self, theme: &Theme) -> Style {
        match &self.result {
            None => theme.bg("toolPendingBg"),
            Some(_) if self.is_error => theme.bg("toolErrorBg"),
            Some(_) => theme.bg("toolSuccessBg"),
        }
    }

    /// The item's rows at `width`.
    pub fn render(&self, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        if let Some(draw) = &self.draw {
            return self.render_drawn(draw, width, ctx);
        }
        if !self.known {
            return self.render_unknown(width, ctx);
        }
        if self.name == "edit" {
            return self.render_edit(width, ctx);
        }
        let inner = box_content_width(width, 1);
        let mut body: Vec<StyledLine> = lines::wrap_all(&self.call_lines(ctx, inner), inner);
        body.extend(self.result_lines(ctx, inner));
        let mut out = lines::spacer(1);
        out.extend(boxed(body, width, 1, 1, Some(self.background(theme))));
        out
    }

    /// pi's `formatToolExecution` for a tool without a definition: the name,
    /// the arguments as indented JSON and the output, as plain text.
    fn render_unknown(&self, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let inner = box_content_width(width, 1);
        let mut text = vec![Line::from(title(ctx.theme, &self.name))];
        let json = yapi_types::json::to_string_pretty(&self.args, "  ").unwrap_or_default();
        if !json.is_empty() {
            text.push(Line::default());
            text.extend(json.split('\n').map(|line| Line::from(line.to_owned())));
        }
        if let Some(result) = self.shown_result() {
            let output = text_output(result);
            if !output.is_empty() {
                text.extend(output.split('\n').map(|line| Line::from(line.to_owned())));
            }
        }
        let mut out = lines::spacer(1);
        out.extend(boxed(
            lines::wrap_all(&text, inner),
            width,
            1,
            1,
            Some(self.background(ctx.theme)),
        ));
        out
    }

    /// pi's composition for tools with a definition: the call's component
    /// (or the generic call) and the result's (or the output preview), in the
    /// tool box or, with `renderShell: "self"`, after a blank row.
    fn render_drawn(
        &self,
        draw: &ToolDraw,
        width: usize,
        ctx: &RenderContext<'_>,
    ) -> Vec<StyledLine> {
        let own = draw.renderers.own_shell;
        let inner = if own {
            width
        } else {
            box_content_width(width, 1)
        };
        let mut body = match &draw.call {
            Some(view) if draw.renderers.call => view.render(inner).0,
            _ => lines::wrap_all(
                &generic_call(&self.name, &self.args, ctx.theme, ctx.expanded),
                inner,
            ),
        };
        if let Some(result) = self.shown_result() {
            match &draw.result {
                Some(view) if draw.renderers.result => body.extend(view.render(inner).0),
                _ => body.extend(fallback_result(result, ctx, inner)),
            }
        }
        if own {
            if body.is_empty() {
                return Vec::new();
            }
            let mut out = vec![Line::default()];
            out.extend(body);
            return out;
        }
        let mut out = lines::spacer(1);
        out.extend(boxed(body, width, 1, 1, Some(self.background(ctx.theme))));
        out
    }

    fn call_lines(&self, ctx: &RenderContext<'_>, width: usize) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let home = ctx.home;
        let args = &self.args;
        if let Some(label) = &self.mcp_label {
            return generic_call(label, args, theme, ctx.expanded);
        }
        match self.name.as_str() {
            "read" => {
                let mut spans = vec![title(theme, "read"), Span::raw(" ")];
                spans.push(path_span(
                    theme,
                    string_arg(args, &["file_path", "path"]),
                    home,
                    None,
                ));
                spans.extend(read_range(args, theme));
                vec![Line::from(spans)]
            }
            "bash" => {
                let command = match string_arg(args, &["command"]) {
                    None => Span::styled("[invalid arg]", theme.fg("error")),
                    Some(Some(command)) if !command.is_empty() => {
                        title(theme, &format!("$ {command}"))
                    }
                    Some(_) => Span::styled("...", theme.fg("toolOutput")),
                };
                let mut spans = if matches!(string_arg(args, &["command"]), Some(Some(c)) if !c.is_empty())
                {
                    vec![command]
                } else {
                    vec![title(theme, "$ "), command]
                };
                if let Some(timeout) = number_arg(args, "timeout").filter(|t| t != "0") {
                    spans.push(Span::styled(
                        format!(" (timeout {timeout}s)"),
                        theme.fg("muted"),
                    ));
                }
                vec![Line::from(spans)]
            }
            "write" => {
                let mut lines = vec![Line::from(vec![
                    title(theme, "write"),
                    Span::raw(" "),
                    path_span(theme, string_arg(args, &["file_path", "path"]), home, None),
                ])];
                match string_arg(args, &["content"]) {
                    None => {
                        lines.push(Line::default());
                        lines.push(Line::from(Span::styled(
                            "[invalid content arg - expected string]",
                            theme.fg("error"),
                        )));
                    }
                    Some(Some(content)) if !content.is_empty() => {
                        let content = content.replace('\r', "");
                        let all =
                            trim_trailing_empty(content.split('\n').map(replace_tabs).collect());
                        let (shown, hint) = head_lines(all, 10, theme.fg("toolOutput"), true, ctx);
                        lines.push(Line::default());
                        lines.extend(shown);
                        lines.extend(hint);
                    }
                    Some(_) => {}
                }
                lines
            }
            "grep" => {
                let pattern = match string_arg(args, &["pattern"]) {
                    None => Span::styled("[invalid arg]", theme.fg("error")),
                    Some(pattern) => Span::styled(
                        format!("/{}/", pattern.unwrap_or_default()),
                        theme.fg("accent"),
                    ),
                };
                let path = path_span(theme, string_arg(args, &["path"]), home, Some(".")).content;
                let mut spans = vec![
                    title(theme, "grep"),
                    Span::raw(" "),
                    pattern,
                    Span::styled(format!(" in {path}"), theme.fg("toolOutput")),
                ];
                if let Some(Some(glob)) =
                    string_arg(args, &["glob"]).filter(|g| g.is_some_and(|g| !g.is_empty()))
                {
                    spans.push(Span::styled(format!(" ({glob})"), theme.fg("toolOutput")));
                }
                if let Some(limit) = number_arg(args, "limit") {
                    spans.push(Span::styled(
                        format!(" limit {limit}"),
                        theme.fg("toolOutput"),
                    ));
                }
                vec![Line::from(spans)]
            }
            "find" => {
                let pattern = match string_arg(args, &["pattern"]) {
                    None => Span::styled("[invalid arg]", theme.fg("error")),
                    Some(pattern) => {
                        Span::styled(pattern.unwrap_or_default().to_owned(), theme.fg("accent"))
                    }
                };
                let path = path_span(theme, string_arg(args, &["path"]), home, Some(".")).content;
                let mut spans = vec![
                    title(theme, "find"),
                    Span::raw(" "),
                    pattern,
                    Span::styled(format!(" in {path}"), theme.fg("toolOutput")),
                ];
                if let Some(limit) = number_arg(args, "limit") {
                    spans.push(Span::styled(
                        format!(" (limit {limit})"),
                        theme.fg("toolOutput"),
                    ));
                }
                vec![Line::from(spans)]
            }
            "ls" => {
                let mut spans = vec![
                    title(theme, "ls"),
                    Span::raw(" "),
                    path_span(theme, string_arg(args, &["path"]), home, Some(".")),
                ];
                if let Some(limit) = number_arg(args, "limit") {
                    spans.push(Span::styled(
                        format!(" (limit {limit})"),
                        theme.fg("toolOutput"),
                    ));
                }
                vec![Line::from(spans)]
            }
            "codemode" => self.codemode_call(ctx, width),
            _ => generic_call(&self.name, args, theme, ctx.expanded),
        }
    }

    /// pi's codemode `renderCall`: the title, then the script.
    fn codemode_call(&self, ctx: &RenderContext<'_>, width: usize) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let code = match self.args.get("code") {
            Some(Value::String(code)) => code.as_str(),
            None | Some(Value::Null) => "",
            Some(_) => {
                return vec![Line::from(vec![
                    title(theme, "codemode"),
                    Span::raw(" "),
                    Span::styled("[invalid arg]", theme.fg("error")),
                ])];
            }
        };
        let mut out = vec![Line::from(title(theme, "codemode"))];
        if code.is_empty() {
            return out;
        }
        let code: Vec<StyledLine> = replace_tabs(code.replace('\r', "").trim_end())
            .split('\n')
            .map(|line| Line::from(line.to_owned()))
            .collect();
        if ctx.expanded {
            out.extend(code);
        } else {
            out.extend(head_preview(&code, 10, width, |hidden| {
                more_lines_hint(theme, ctx, format!("... ({hidden} more lines,"))
            }));
        }
        out
    }

    /// pi's codemode `renderResult`: the nested calls, then the output
    /// without the result header.
    fn codemode_result(
        &self,
        result: &ToolResult,
        ctx: &RenderContext<'_>,
        width: usize,
    ) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let mut out = Vec::new();
        let details = result.details.as_ref();
        let calls: &[Value] = details
            .and_then(|details| details["calls"].as_array())
            .map_or(&[], Vec::as_slice);
        if !calls.is_empty() {
            let skipped = if ctx.expanded {
                0
            } else {
                calls.len().saturating_sub(8)
            };
            let mut rows = Vec::new();
            if skipped > 0 {
                rows.push(more_lines_hint(
                    theme,
                    ctx,
                    format!("... ({skipped} earlier calls,"),
                ));
            }
            for call in &calls[skipped..] {
                rows.extend(script_call(call, theme, ctx.expanded));
            }
            out.push(Line::default());
            out.extend(lines::wrap_all(&rows, width));
        }
        if self.result.is_none() {
            return out;
        }
        let header = result.content.first().is_some_and(
            |block| matches!(block, ContentBlock::Text(text) if is_script_header(&text.text)),
        );
        let shown = ToolResult {
            content: result.content[usize::from(header)..].to_vec(),
            ..ToolResult::default()
        };
        let output = text_output(&shown);
        let output = output.trim();
        if output.is_empty() {
            return out;
        }
        let style = theme.fg(if self.is_error { "error" } else { "toolOutput" });
        let styled: Vec<StyledLine> = replace_tabs(output)
            .split('\n')
            .map(|line| lines::styled(line.to_owned(), style))
            .collect();
        out.push(Line::default());
        if ctx.expanded {
            out.extend(lines::wrap_all(&styled, width));
        } else {
            out.extend(head_preview(&styled, 5, width, |hidden| {
                more_lines_hint(theme, ctx, format!("... ({hidden} more lines,"))
            }));
            if let Some(path) = details.and_then(|details| details["fullOutputPath"].as_str()) {
                out.extend(lines::wrap(
                    &lines::styled(format!("Full output: {path}"), theme.fg("muted")),
                    width,
                ));
            }
        }
        out
    }

    fn result_lines(&self, ctx: &RenderContext<'_>, width: usize) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let Some(result) = self.shown_result() else {
            return Vec::new();
        };
        if self.mcp_label.is_some() {
            return self.mcp_result(result, ctx, width);
        }
        let output_style = theme.fg("toolOutput");
        let details = result.details.as_ref();
        match self.name.as_str() {
            "read" => {
                if !ctx.expanded && !self.is_error {
                    return Vec::new();
                }
                let output = text_output(result);
                let all = trim_trailing_empty(output.split('\n').map(replace_tabs).collect());
                let (shown, hint) = head_lines(all, 10, output_style, false, ctx);
                let mut out = vec![Line::default()];
                out.extend(shown);
                out.extend(hint);
                if let Some(truncation) = details
                    .and_then(|d| d.get("truncation"))
                    .filter(|t| t["truncated"] == true)
                {
                    let max_bytes = truncation["maxBytes"].as_u64().unwrap_or(50 * 1024) as usize;
                    let warning = if truncation["firstLineExceedsLimit"] == true {
                        format!("[First line exceeds {} limit]", format_size(max_bytes))
                    } else if truncation["truncatedBy"] == "lines" {
                        format!(
                            "[Truncated: showing {} of {} lines ({} line limit)]",
                            truncation["outputLines"],
                            truncation["totalLines"],
                            truncation["maxLines"].as_u64().unwrap_or(2000)
                        )
                    } else {
                        format!(
                            "[Truncated: {} lines shown ({} limit)]",
                            truncation["outputLines"],
                            format_size(max_bytes)
                        )
                    };
                    out.push(lines::styled(warning, theme.fg("warning")));
                }
                lines::wrap_all(&out, width)
            }
            "bash" => self.bash_result(result, ctx, width),
            "codemode" => self.codemode_result(result, ctx, width),
            "write" => {
                if !self.is_error {
                    return Vec::new();
                }
                let output = text_output(result);
                if output.is_empty() {
                    return Vec::new();
                }
                let mut out = vec![Line::default()];
                out.extend(
                    output
                        .split('\n')
                        .map(|line| lines::styled(line.to_owned(), theme.fg("error"))),
                );
                lines::wrap_all(&out, width)
            }
            "grep" | "find" | "ls" => {
                let output = text_output(result);
                let output = output.trim();
                let mut out = Vec::new();
                if !output.is_empty() {
                    let all = output.split('\n').map(str::to_owned).collect();
                    let limit = if self.name == "grep" { 15 } else { 20 };
                    let (shown, hint) = head_lines(all, limit, output_style, false, ctx);
                    out.push(Line::default());
                    out.extend(shown);
                    out.extend(hint);
                }
                let mut warnings = Vec::new();
                let unit = match self.name.as_str() {
                    "grep" => "matches",
                    "find" => "results",
                    _ => "entries",
                };
                let limit_key = match self.name.as_str() {
                    "grep" => "matchLimitReached",
                    "find" => "resultLimitReached",
                    _ => "entryLimitReached",
                };
                if let Some(limit) = details
                    .and_then(|d| d.get(limit_key))
                    .filter(|v| !v.is_null())
                {
                    warnings.push(format!("{limit} {unit} limit"));
                }
                if let Some(truncation) = details
                    .and_then(|d| d.get("truncation"))
                    .filter(|t| t["truncated"] == true)
                {
                    warnings.push(format!(
                        "{} limit",
                        format_size(truncation["maxBytes"].as_u64().unwrap_or(50 * 1024) as usize)
                    ));
                }
                if self.name == "grep" && details.is_some_and(|d| d["linesTruncated"] == true) {
                    warnings.push("some lines truncated".to_owned());
                }
                if !warnings.is_empty() {
                    out.push(lines::styled(
                        format!("[Truncated: {}]", warnings.join(", ")),
                        theme.fg("warning"),
                    ));
                }
                lines::wrap_all(&out, width)
            }
            _ => {
                let output = text_output(result);
                if output.is_empty() {
                    return Vec::new();
                }
                let all = output.split('\n').map(replace_tabs).collect();
                let (mut out, hint) = head_lines(all, 10, output_style, false, ctx);
                if let Some(hint) = hint {
                    out.push(Line::default());
                    out.push(hint);
                }
                lines::wrap_all(&out, width)
            }
        }
    }

    /// pi's MCP `renderResult`: a blank row and the output colored by
    /// outcome; collapsed, its first wrapped rows, how many more, and the
    /// file holding output that was cut for the model.
    fn mcp_result(
        &self,
        result: &ToolResult,
        ctx: &RenderContext<'_>,
        width: usize,
    ) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let output = text_output(result);
        let output = output.trim();
        if output.is_empty() {
            return Vec::new();
        }
        let color = theme.fg(if self.is_error { "error" } else { "toolOutput" });
        let text: Vec<StyledLine> = replace_tabs(output)
            .split('\n')
            .map(|line| lines::styled(line.to_owned(), color))
            .collect();
        let rows = lines::wrap_all(&text, width);
        let mut out = vec![Line::default()];
        if ctx.expanded {
            out.extend(rows);
            return out;
        }
        let hidden = rows.len().saturating_sub(MCP_PREVIEW_LINES);
        out.extend(rows.into_iter().take(MCP_PREVIEW_LINES));
        if hidden > 0 {
            let hint = more_lines_hint(theme, ctx, format!("... ({hidden} more lines,"));
            out.push(lines::truncate(&hint, width, "..."));
        }
        let path = result
            .details
            .as_ref()
            .and_then(|details| details.get("fullOutputPath"))
            .and_then(Value::as_str);
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            out.extend(lines::wrap(
                &lines::styled(format!("Full output: {path}"), theme.fg("muted")),
                width,
            ));
        }
        out
    }

    fn bash_result(
        &self,
        result: &ToolResult,
        ctx: &RenderContext<'_>,
        width: usize,
    ) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let partial = self.result.is_none();
        let details = result.details.as_ref();
        let mut output = text_output(result).trim().to_owned();
        let truncation = details
            .and_then(|d| d.get("truncation"))
            .filter(|t| t["truncated"] == true);
        let full_output = details.and_then(|d| d["fullOutputPath"].as_str());
        if !partial
            && truncation.is_some()
            && let Some(path) = full_output
            && output.ends_with(']')
            && let Some(start) = output.rfind("\n\n[")
            && output[start..].contains(path)
        {
            output = output[..start].trim_end().to_owned();
        }
        let mut out = Vec::new();
        if !output.is_empty() {
            let styled: Vec<StyledLine> = output
                .split('\n')
                .map(|line| lines::styled(line.to_owned(), theme.fg("toolOutput")))
                .collect();
            if ctx.expanded {
                out.push(Line::default());
                out.extend(lines::wrap_all(&styled, width));
            } else {
                out.push(Line::default());
                out.extend(tail_preview(&styled, 5, width, |hidden| {
                    more_lines_hint(theme, ctx, format!("... ({hidden} earlier lines,"))
                }));
            }
        }
        if truncation.is_some() || full_output.is_some() {
            let mut warnings = Vec::new();
            if let Some(path) = full_output {
                warnings.push(format!("Full output: {path}"));
            }
            if let Some(truncation) = truncation {
                if truncation["truncatedBy"] == "lines" {
                    warnings.push(format!(
                        "Truncated: showing {} of {} lines",
                        truncation["outputLines"], truncation["totalLines"]
                    ));
                } else {
                    warnings.push(format!(
                        "Truncated: {} lines shown ({} limit)",
                        truncation["outputLines"],
                        format_size(truncation["maxBytes"].as_u64().unwrap_or(50 * 1024) as usize)
                    ));
                }
            }
            out.push(Line::default());
            out.extend(lines::wrap(
                &lines::styled(format!("[{}]", warnings.join(". ")), theme.fg("warning")),
                width,
            ));
        }
        if let Some(started) = self.started {
            let label = if partial { "Elapsed" } else { "Took" };
            let end = self.finished.unwrap_or_else(Instant::now);
            out.push(Line::default());
            out.push(lines::styled(
                format!(
                    "{label} {}",
                    format_duration(end.duration_since(started).as_millis())
                ),
                theme.fg("muted"),
            ));
        }
        out
    }

    fn render_edit(&self, width: usize, ctx: &RenderContext<'_>) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let inner = box_content_width(width, 1);
        let header = Line::from(vec![
            title(theme, "edit"),
            Span::raw(" "),
            path_span(
                theme,
                string_arg(&self.args, &["file_path", "path"]),
                ctx.home,
                None,
            ),
        ]);
        let mut body = lines::wrap(&header, inner);
        // pi's edit renderer: a successful result's diff replaces the
        // preview, which shows in the box, and colors its background.
        let result_diff = self
            .result
            .as_ref()
            .filter(|_| !self.is_error)
            .and_then(|result| result.details.as_ref())
            .and_then(|details| details["diff"].as_str());
        let preview = match result_diff {
            Some(diff) => Some(Ok(diff.to_owned())),
            None => self.edit_preview.clone(),
        };
        match &preview {
            Some(Ok(diff)) => {
                body.push(Line::default());
                body.extend(lines::wrap_all(&render_diff(diff, theme), inner));
            }
            Some(Err(error)) => {
                body.push(Line::default());
                body.extend(lines::wrap(
                    &lines::styled(error.clone(), theme.fg("error")),
                    inner,
                ));
            }
            None => {}
        }
        let bg = match &preview {
            Some(Ok(_)) => theme.bg("toolSuccessBg"),
            Some(Err(_)) => theme.bg("toolErrorBg"),
            None if self.result.is_some() && self.is_error => theme.bg("toolErrorBg"),
            None => theme.bg("toolPendingBg"),
        };
        let mut out = vec![Line::default()];
        out.extend(boxed(body, width, 1, 1, Some(bg)));
        // The result's error shows below, unless the preview showed it.
        if self.is_error
            && let Some(result) = &self.result
        {
            let text = text_output(result);
            let previewed = matches!(&preview, Some(Err(error)) if *error == text);
            if !text.is_empty() && !previewed {
                out.extend(lines::spacer(1));
                out.extend(lines::text(
                    &[lines::styled(text, theme.fg("error"))],
                    width,
                    1,
                    0,
                    None,
                ));
            }
        }
        out
    }
}

fn read_range(args: &Value, theme: &Theme) -> Option<Span<'static>> {
    let offset = args.get("offset").and_then(Value::as_f64);
    let limit = args.get("limit").and_then(Value::as_f64);
    if offset.is_none() && limit.is_none() {
        return None;
    }
    let start = offset.unwrap_or(1.0);
    let end = limit.map(|limit| start + limit - 1.0);
    let number = |n: f64| yapi_core::tools::js_number(n);
    let text = match end {
        Some(end) => format!(":{}-{}", number(start), number(end)),
        None => format!(":{}", number(start)),
    };
    Some(Span::styled(text, theme.fg("warning")))
}

/// pi's generic call header: `name key=value ...`, or one `key: value` line per
/// argument when expanded.
/// pi's `createResultFallback`: the output's first lines and how many more.
fn fallback_result(result: &ToolResult, ctx: &RenderContext<'_>, width: usize) -> Vec<StyledLine> {
    let output = text_output(result);
    if output.is_empty() {
        return Vec::new();
    }
    let all: Vec<&str> = output.split('\n').collect();
    let shown = if ctx.expanded {
        all.len()
    } else {
        10.min(all.len())
    };
    let mut out: Vec<StyledLine> = all[..shown]
        .iter()
        .map(|line| lines::styled((*line).to_owned(), ctx.theme.fg("toolOutput")))
        .collect();
    if all.len() > shown {
        out.push(more_lines_hint(
            ctx.theme,
            ctx,
            format!("... ({} more lines,", all.len() - shown),
        ));
    }
    lines::wrap_all(&out, width)
}

fn generic_call(name: &str, args: &Value, theme: &Theme, expanded: bool) -> Vec<StyledLine> {
    let header = title(theme, name);
    let entries: Vec<(String, Value)> = match args {
        Value::Null => return vec![Line::from(header)],
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        other => vec![("args".to_owned(), other.clone())],
    };
    if entries.is_empty() {
        return vec![Line::from(header)];
    }
    if expanded {
        let mut lines = vec![Line::from(header)];
        for (key, value) in entries {
            let text = match &value {
                Value::String(text) => text.clone(),
                other => yapi_types::json::to_string_pretty(other, "  ").unwrap_or_default(),
            };
            let text = replace_tabs(&text)
                .replace('\r', "")
                .replace('\n', "\n    ");
            for (index, part) in format!("  {key}: {text}").split('\n').enumerate() {
                let _ = index;
                lines.push(lines::styled(part.to_owned(), theme.fg("muted")));
            }
        }
        return lines;
    }
    let pairs: Vec<String> = entries
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}={}",
                yapi_types::json::to_string(value).unwrap_or_default()
            )
        })
        .collect();
    let pairs = pairs.join(" ");
    let preview = if pairs.chars().count() > 100 {
        format!("{}...", pairs.chars().take(97).collect::<String>())
    } else {
        pairs
    };
    vec![Line::from(vec![
        header,
        Span::raw(" "),
        Span::styled(preview, theme.fg("muted")),
    ])]
}

/// pi's `^([+-\s])(\s*\d*)\s(.*)$`, trying the same backtracking order.
fn parse_diff_line(line: &str) -> Option<(char, &str, &str)> {
    let prefix = line
        .chars()
        .next()
        .filter(|c| matches!(c, '+' | '-') || c.is_whitespace())?;
    let rest = &line[prefix.len_utf8()..];
    let split_at = |end: usize| -> Option<(char, &str, &str)> {
        let separator = rest[end..].chars().next().filter(|c| c.is_whitespace())?;
        Some((prefix, &rest[..end], &rest[end + separator.len_utf8()..]))
    };
    let spaces = rest.len() - rest.trim_start().len();
    let digits = rest[spaces..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    for taken in (0..=digits).rev() {
        if let Some(parsed) = split_at(spaces + taken) {
            return Some(parsed);
        }
    }
    let boundaries: Vec<usize> = rest[..spaces]
        .char_indices()
        .map(|(index, _)| index)
        .collect();
    boundaries.into_iter().rev().find_map(split_at)
}

/// pi's `renderIntraLineDiff`: jsdiff's word diff, with changed words in
/// inverse video and the first change's indentation left plain.
fn intra_line(old: &str, new: &str) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    use super::word_diff::{Tag, diff_words};
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut first_removed = true;
    let mut first_added = true;
    let inverse = Style::new().add_modifier(Modifier::REVERSED);
    for (tag, value) in diff_words(old, new) {
        let (target, first) = match tag {
            Tag::Removed => (&mut removed, &mut first_removed),
            Tag::Added => (&mut added, &mut first_added),
            Tag::Keep => {
                removed.push(Span::raw(value.clone()));
                added.push(Span::raw(value));
                continue;
            }
        };
        let mut value = value.as_str();
        if *first {
            let trimmed = value.trim_start();
            let leading = &value[..value.len() - trimmed.len()];
            if !leading.is_empty() {
                target.push(Span::raw(leading.to_owned()));
            }
            value = trimmed;
            *first = false;
        }
        if !value.is_empty() {
            target.push(Span::styled(value.to_owned(), inverse));
        }
    }
    (removed, added)
}

/// pi's `renderDiff`: colored diff lines with word highlights for a single
/// replaced line.
pub fn render_diff(diff: &str, theme: &Theme) -> Vec<StyledLine> {
    let lines: Vec<&str> = diff.split('\n').collect();
    let mut out = Vec::new();
    let colored = |prefix: char, number: &str, spans: Vec<Span<'static>>, token: &str| {
        let style = theme.fg(token);
        let mut all = vec![Span::raw(format!("{prefix}{number} "))];
        all.extend(spans);
        Line::from(
            all.into_iter()
                .map(|span| {
                    let patched = style.patch(span.style);
                    Span::styled(span.content, patched)
                })
                .collect::<Vec<_>>(),
        )
    };
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        match parse_diff_line(line) {
            Some(('-', _, _)) => {
                let mut removed = Vec::new();
                while let Some(('-', number, content)) =
                    lines.get(index).and_then(|l| parse_diff_line(l))
                {
                    removed.push((number, content));
                    index += 1;
                }
                let mut added = Vec::new();
                while let Some(('+', number, content)) =
                    lines.get(index).and_then(|l| parse_diff_line(l))
                {
                    added.push((number, content));
                    index += 1;
                }
                if removed.len() == 1 && added.len() == 1 {
                    let (old, new) =
                        intra_line(&replace_tabs(removed[0].1), &replace_tabs(added[0].1));
                    out.push(colored('-', removed[0].0, old, "toolDiffRemoved"));
                    out.push(colored('+', added[0].0, new, "toolDiffAdded"));
                } else {
                    for (number, content) in removed {
                        out.push(colored(
                            '-',
                            number,
                            vec![Span::raw(replace_tabs(content))],
                            "toolDiffRemoved",
                        ));
                    }
                    for (number, content) in added {
                        out.push(colored(
                            '+',
                            number,
                            vec![Span::raw(replace_tabs(content))],
                            "toolDiffAdded",
                        ));
                    }
                }
            }
            Some(('+', number, content)) => {
                out.push(colored(
                    '+',
                    number,
                    vec![Span::raw(replace_tabs(content))],
                    "toolDiffAdded",
                ));
                index += 1;
            }
            Some((_, number, content)) => {
                out.push(colored(
                    ' ',
                    number,
                    vec![Span::raw(replace_tabs(content))],
                    "toolDiffContext",
                ));
                index += 1;
            }
            None => {
                out.push(lines::styled(
                    replace_tabs(line),
                    theme.fg("toolDiffContext"),
                ));
                index += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_image_sizes_as_pi_tui() {
        // A 1×1 PNG and a 3×2 GIF.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        assert_eq!(image_dimensions(png, "image/png"), (1, 1));
        let gif = "R0lGODlhAwACAIAAAP///wAAACwAAAAAAwACAAACAoQRADs=";
        assert_eq!(image_dimensions(gif, "image/gif"), (3, 2));
        assert_eq!(image_dimensions("not base64!", "image/png"), (800, 600));
        assert_eq!(image_dimensions(png, "image/bmp"), (800, 600));
    }

    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(1234), "1.2s");
        assert_eq!(format_duration(125_000), "2m 5s");
        assert_eq!(format_duration(3_725_000), "1h 2m 5s");
    }

    #[test]
    fn text_output_sanitizes_as_pi() {
        let result = |text: &str| ToolResult {
            content: vec![ContentBlock::text(text)],
            ..ToolResult::default()
        };
        assert_eq!(text_output(&result("a\x1b[31mb\x1b[0m\r\nc\x07")), "ab\nc");
        // pi keeps DEL and C1 controls, drops interlinear annotations, and
        // strips charset selections and 8-bit CSI sequences whole.
        assert_eq!(
            text_output(&result("a\x7fb\u{85}c\u{fff9}d\x1b(Be\u{9b}31mf\tg")),
            "a\x7fb\u{85}cdef\tg"
        );
    }

    #[test]
    fn previews_mcp_output_as_pi() {
        let theme = Theme::builtin("dark", yapi_tui::color::ColorMode::TrueColor).unwrap();
        let markdown = yapi_tui::markdown::MarkdownTheme::default();
        let mut ctx = RenderContext {
            theme: &theme,
            markdown: &markdown,
            expanded: false,
            hide_thinking: false,
            output_pad: 1,
            expand_key: "ctrl+o",
            cancel_key: "esc",
            home: None,
            thinking_label: "Thinking...",
        };
        let mut view = ToolView::new("mcp__demo__big", Value::Null);
        view.mcp_label = Some("demo/big".to_owned());
        let rows = |view: &ToolView, ctx: &RenderContext<'_>| -> Vec<String> {
            view.result_lines(ctx, 20)
                .iter()
                .map(|line| lines::plain(line).trim_end().to_owned())
                .collect()
        };
        let result = |text: &str, details: Value| -> Option<ToolResult> {
            let content = serde_json::json!([{"type": "text", "text": text}]);
            serde_json::from_value(serde_json::json!({"content": content, "details": details})).ok()
        };
        view.result = result(
            "  one two three four five six seven eight\nnine  \n",
            serde_json::json!({"fullOutputPath": "/tmp/out.txt"}),
        );
        assert_eq!(
            rows(&view, &ctx),
            [
                "",
                "one two three four",
                "five six seven eight",
                "nine",
                "Full output:",
                "/tmp/out.txt"
            ]
        );
        let rows8: Vec<String> = (1..=8).map(|n| format!("row {n}")).collect();
        view.result = result(&rows8.join("\n"), Value::Null);
        assert_eq!(
            rows(&view, &ctx),
            [
                "",
                "row 1",
                "row 2",
                "row 3",
                "row 4",
                "row 5",
                "... (3 more lines..."
            ]
        );
        ctx.expanded = true;
        assert_eq!(rows(&view, &ctx).len(), 9);
        view.result = result(" \n ", Value::Null);
        assert!(rows(&view, &ctx).is_empty());
        assert_eq!(
            lines::plain(&view.call_lines(&ctx, 80)[0]),
            "demo/big",
            "the call shows the server/tool label"
        );
    }

    #[test]
    fn parses_diff_lines() {
        assert_eq!(parse_diff_line("+12 added"), Some(('+', "12", "added")));
        assert_eq!(parse_diff_line("- 3 gone"), Some(('-', " 3", "gone")));
        assert_eq!(parse_diff_line("   ..."), Some((' ', " ", "...")));
        assert_eq!(parse_diff_line("x"), None);
    }
}
