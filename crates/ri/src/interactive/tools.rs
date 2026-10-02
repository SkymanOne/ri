//! Tool calls in the transcript.
//!
//! Ports of `components/tool-execution.ts`, `components/diff.ts`,
//! `components/visual-truncate.ts` and `core/tools/renderers/*.ts` in pi
//! `v1.0.0`.

use std::time::Instant;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use ri_tui::lines::{self, StyledLine, box_content_width, boxed};
use ri_tui::theme::Theme;
use ri_types::event::ToolResult;
use ri_types::message::ContentBlock;
use serde_json::Value;

use super::chat::RenderContext;

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
}

/// Strips escape sequences and control characters other than tab and newline.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// pi's `getTextOutput`: text blocks joined by newlines, images as notes.
pub fn text_output(result: &ToolResult) -> String {
    let text: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(sanitize(&text.text)),
            _ => None,
        })
        .collect();
    let mut output = text.join("\n");
    let images: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(format!("[Image: [{}]]", image.mime_type)),
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

fn shorten_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path.starts_with(home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// Spans of a key hint: the key dim, the text muted.
fn key_hint(theme: &Theme, key: &str, text: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_owned(), theme.fg("dim")),
        Span::styled(format!(" {text}"), theme.fg("muted")),
    ]
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

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
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

    fn call_lines(&self, ctx: &RenderContext<'_>, _width: usize) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let home = ctx.home;
        let args = &self.args;
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
                            trim_trailing_empty(content.split('\n').map(str::to_owned).collect());
                        let max = if ctx.expanded { all.len() } else { 10 };
                        lines.push(Line::default());
                        for line in all.iter().take(max) {
                            lines.push(lines::styled(replace_tabs(line), theme.fg("toolOutput")));
                        }
                        if all.len() > max {
                            lines.push(more_lines_hint(
                                theme,
                                ctx,
                                format!(
                                    "... ({} more lines, {} total,",
                                    all.len() - max,
                                    all.len()
                                ),
                            ));
                        }
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
                let path = match string_arg(args, &["path"]) {
                    None => "[invalid arg]".to_owned(),
                    Some(path) => shorten_home(path.filter(|p| !p.is_empty()).unwrap_or("."), home),
                };
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
                let path = match string_arg(args, &["path"]) {
                    None => "[invalid arg]".to_owned(),
                    Some(path) => shorten_home(path.filter(|p| !p.is_empty()).unwrap_or("."), home),
                };
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
                let path = match string_arg(args, &["path"]) {
                    None => Span::styled("[invalid arg]", theme.fg("error")),
                    Some(path) => Span::styled(
                        shorten_home(path.filter(|p| !p.is_empty()).unwrap_or("."), home),
                        theme.fg("accent"),
                    ),
                };
                let mut spans = vec![title(theme, "ls"), Span::raw(" "), path];
                if let Some(limit) = number_arg(args, "limit") {
                    spans.push(Span::styled(
                        format!(" (limit {limit})"),
                        theme.fg("toolOutput"),
                    ));
                }
                vec![Line::from(spans)]
            }
            _ => generic_call(&self.name, args, theme, ctx.expanded),
        }
    }

    fn result_lines(&self, ctx: &RenderContext<'_>, width: usize) -> Vec<StyledLine> {
        let theme = ctx.theme;
        let Some(result) = self.shown_result() else {
            return Vec::new();
        };
        let output_style = theme.fg("toolOutput");
        let details = result.details.as_ref();
        match self.name.as_str() {
            "read" => {
                if !ctx.expanded && !self.is_error {
                    return Vec::new();
                }
                let output = text_output(result);
                let all = trim_trailing_empty(output.split('\n').map(str::to_owned).collect());
                let max = if ctx.expanded { all.len() } else { 10 };
                let mut out = vec![Line::default()];
                out.extend(
                    all.iter()
                        .take(max)
                        .map(|line| lines::styled(replace_tabs(line), output_style)),
                );
                if all.len() > max {
                    out.push(more_lines_hint(
                        theme,
                        ctx,
                        format!("... ({} more lines,", all.len() - max),
                    ));
                }
                if let Some(truncation) = details
                    .and_then(|d| d.get("truncation"))
                    .filter(|t| t["truncated"] == true)
                {
                    let max_bytes = truncation["maxBytes"].as_u64().unwrap_or(50 * 1024);
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
                    let all: Vec<&str> = output.split('\n').collect();
                    let limit = if self.name == "grep" { 15 } else { 20 };
                    let max = if ctx.expanded { all.len() } else { limit };
                    out.push(Line::default());
                    out.extend(
                        all.iter()
                            .take(max)
                            .map(|line| lines::styled((*line).to_owned(), output_style)),
                    );
                    if all.len() > max {
                        out.push(more_lines_hint(
                            theme,
                            ctx,
                            format!("... ({} more lines,", all.len() - max),
                        ));
                    }
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
                        format_size(truncation["maxBytes"].as_u64().unwrap_or(50 * 1024))
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
                let all: Vec<&str> = output.split('\n').collect();
                let max = if ctx.expanded { all.len() } else { 10 };
                let mut out: Vec<StyledLine> = all
                    .iter()
                    .take(max)
                    .map(|line| lines::styled(replace_tabs(line), output_style))
                    .collect();
                if all.len() > max {
                    out.push(Line::default());
                    out.push(more_lines_hint(
                        theme,
                        ctx,
                        format!("... ({} more lines,", all.len() - max),
                    ));
                }
                lines::wrap_all(&out, width)
            }
        }
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
                        format_size(truncation["maxBytes"].as_u64().unwrap_or(50 * 1024))
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
        let diff = self
            .result
            .as_ref()
            .and_then(|result| result.details.as_ref())
            .and_then(|details| details["diff"].as_str());
        if let Some(diff) = diff.filter(|_| !self.is_error) {
            body.push(Line::default());
            body.extend(lines::wrap_all(&render_diff(diff, theme), inner));
        }
        let bg = match &self.result {
            Some(_) if self.is_error => theme.bg("toolErrorBg"),
            Some(_) if diff.is_some() => theme.bg("toolSuccessBg"),
            Some(_) => theme.bg("toolSuccessBg"),
            None => theme.bg("toolPendingBg"),
        };
        let mut out = vec![Line::default()];
        out.extend(boxed(body, width, 1, 1, Some(bg)));
        if self.is_error
            && let Some(result) = &self.result
        {
            let text = text_output(result);
            if !text.is_empty() {
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
    let number = |n: f64| ri_core::tools::js_number(n);
    let text = match end {
        Some(end) => format!(":{}-{}", number(start), number(end)),
        None => format!(":{}", number(start)),
    };
    Some(Span::styled(text, theme.fg("warning")))
}

/// pi's generic call header: `name key=value ...`, or one `key: value` line per
/// argument when expanded.
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
                other => ri_types::json::to_string_pretty(other, "  ").unwrap_or_default(),
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
                ri_types::json::to_string(value).unwrap_or_default()
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

fn intra_line(old: &str, new: &str) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_words(old, new);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut first_removed = true;
    let mut first_added = true;
    let inverse = Style::new().add_modifier(Modifier::REVERSED);
    for change in diff.iter_all_changes() {
        let value = change.value().to_owned();
        let (target, first) = match change.tag() {
            ChangeTag::Delete => (&mut removed, &mut first_removed),
            ChangeTag::Insert => (&mut added, &mut first_added),
            ChangeTag::Equal => {
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
    use super::*;

    #[test]
    fn formats_durations_and_sanitizes() {
        assert_eq!(format_duration(1234), "1.2s");
        assert_eq!(format_duration(125_000), "2m 5s");
        assert_eq!(format_duration(3_725_000), "1h 2m 5s");
        assert_eq!(sanitize("a\x1b[31mb\x1b[0m\r\nc\x07"), "ab\nc");
    }

    #[test]
    fn parses_diff_lines() {
        assert_eq!(parse_diff_line("+12 added"), Some(('+', "12", "added")));
        assert_eq!(parse_diff_line("- 3 gone"), Some(('-', " 3", "gone")));
        assert_eq!(parse_diff_line("   ..."), Some((' ', " ", "...")));
        assert_eq!(parse_diff_line("x"), None);
    }
}
