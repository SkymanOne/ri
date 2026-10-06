//! Markdown to styled terminal lines.
//!
//! Port of `packages/tui/src/components/markdown.ts` in pi `v1.0.0`. pi parses
//! with marked; this uses pulldown-cmark and rebuilds marked's block structure,
//! including its blank-line tokens, from source offsets. LaTeX is shown as
//! written and code blocks are not syntax highlighted.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::lines::{StyledLine, pad, raw as raw_line, width as line_width, wrap};
use crate::text::visible_width;

/// Styles for markdown elements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownTheme {
    /// Headings.
    pub heading: Style,
    /// Link text, on top of underline.
    pub link: Style,
    /// The ` (url)` after link text.
    pub link_url: Style,
    /// Inline code.
    pub code: Style,
    /// Code block lines.
    pub code_block: Style,
    /// Code fences.
    pub code_block_border: Style,
    /// Blockquote text, on top of italic.
    pub quote: Style,
    /// The `│ ` quote border.
    pub quote_border: Style,
    /// Horizontal rules.
    pub hr: Style,
    /// List markers.
    pub list_bullet: Style,
    /// Prefix of each code block line.
    pub code_block_indent: String,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        MarkdownTheme {
            heading: Style::new(),
            link: Style::new(),
            link_url: Style::new(),
            code: Style::new(),
            code_block: Style::new(),
            code_block_border: Style::new(),
            quote: Style::new(),
            quote_border: Style::new(),
            hr: Style::new(),
            list_bullet: Style::new(),
            code_block_indent: "  ".to_owned(),
        }
    }
}

/// Options for one markdown block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkdownOptions {
    /// Style of plain text: color and modifiers.
    pub text: Option<Style>,
    /// Background of every row, padding included.
    pub background: Option<Style>,
    /// Keep list markers as written (`*`, `3)`) instead of `-` and `N.`.
    pub preserve_list_markers: bool,
    /// Show backslash escapes as written.
    pub preserve_backslash_escapes: bool,
}

#[derive(Clone, Debug)]
enum Inline {
    Text(String),
    Code(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
    Link {
        children: Vec<Inline>,
        text: String,
        href: String,
    },
    Break,
}

#[derive(Clone, Debug)]
enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// Item text of a tight list, which marked calls a text token.
    Text(Vec<Inline>),
    Code(String, String),
    List {
        ordered: bool,
        start: u64,
        loose: bool,
        items: Vec<Item>,
    },
    Quote(Vec<Spaced>),
    Rule,
    Html(String),
    Table {
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        raw: String,
    },
}

/// A block and whether a blank line separates it from the next.
#[derive(Clone, Debug)]
struct Spaced {
    block: Block,
    blank_after: bool,
}

#[derive(Clone, Debug)]
struct Item {
    marker: Option<String>,
    task: Option<bool>,
    blocks: Vec<Spaced>,
}

struct Builder<'a> {
    source: &'a str,
    events: std::iter::Peekable<pulldown_cmark::OffsetIter<'a>>,
    preserve_escapes: bool,
}

/// Whether a blank line follows the block ending at byte `end`: two or more
/// newlines before the next visible text, or before the end of the source when
/// `at_end` allows it. Quote markers count as blank.
fn blank_line_after(source: &str, end: usize, at_end: bool) -> bool {
    let blank = |c: char| c.is_whitespace() || c == '>';
    let end = end.min(source.len());
    let content_end = source[..end].trim_end_matches(blank).len();
    let rest = &source[end..];
    let gap_end = rest.find(|c: char| !blank(c)).unwrap_or(rest.len());
    if gap_end == rest.len() && !at_end {
        return false;
    }
    let newlines =
        source[content_end..end].matches('\n').count() + rest[..gap_end].matches('\n').count();
    newlines >= 2
}

/// pi's `trimPartialClosingFences`: while a closing fence streams in, the
/// part that has arrived is not code, so the block does not flicker. `raw` is
/// the block's source; only an unclosed block can end in such a line.
fn trim_partial_closing_fence(raw: &str, code: &mut String) {
    let Some(fence) = raw.chars().next().filter(|c| matches!(c, '`' | '~')) else {
        return;
    };
    let marker = raw.chars().take_while(|c| *c == fence).count();
    let last = raw.rsplit('\n').next().unwrap_or_default();
    if marker < 3 || last.is_empty() || last.len() >= marker || last.chars().any(|c| c != fence) {
        return;
    }
    if let Some(kept) = code.strip_suffix(last) {
        let kept = kept.strip_suffix('\n').unwrap_or(kept).len();
        code.truncate(kept);
    }
}

fn is_inline(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Code(_)
            | Event::InlineHtml(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::Start(
                Tag::Strong
                    | Tag::Emphasis
                    | Tag::Strikethrough
                    | Tag::Link { .. }
                    | Tag::Image { .. }
                    | Tag::Superscript
                    | Tag::Subscript
            )
    )
}

impl<'a> Builder<'a> {
    /// Adds one inline event, consuming the rest of a container it opens.
    fn inline(&mut self, event: Event<'a>, range: Range<usize>, out: &mut Vec<Inline>) {
        match event {
            // pulldown-cmark decodes an entity reference into its own text;
            // marked, and so pi, prints it as written.
            Event::Text(text)
                if self.source.get(range.clone()).is_some_and(|raw| {
                    raw.len() > 2 && raw.starts_with('&') && raw.ends_with(';') && raw != &*text
                }) =>
            {
                push_text(out, self.source[range].to_owned());
            }
            Event::Text(text) => {
                // pulldown-cmark starts an escaped character's text just after
                // its backslash.
                let escaped = self.preserve_escapes
                    && range.start > 0
                    && self.source.as_bytes()[range.start - 1] == b'\\'
                    && text.starts_with(|c: char| c.is_ascii_punctuation());
                let text = if escaped {
                    format!("\\{text}")
                } else {
                    text.into_string()
                };
                push_text(out, text);
            }
            Event::Code(code) => out.push(Inline::Code(code.into_string())),
            Event::Html(html) | Event::InlineHtml(html) => push_text(out, html.into_string()),
            Event::SoftBreak => push_text(out, "\n".to_owned()),
            Event::HardBreak => out.push(Inline::Break),
            Event::InlineMath(math) => push_text(out, format!("${math}$")),
            Event::DisplayMath(math) => push_text(out, format!("$${math}$$")),
            Event::FootnoteReference(name) => push_text(out, format!("[^{name}]")),
            Event::Start(Tag::Strong) => {
                let children = self.inlines(TagEnd::Strong);
                out.push(Inline::Strong(children));
            }
            Event::Start(Tag::Emphasis) => {
                let children = self.inlines(TagEnd::Emphasis);
                out.push(Inline::Emphasis(children));
            }
            Event::Start(Tag::Strikethrough) => {
                let children = self.inlines(TagEnd::Strikethrough);
                if self.source[range].starts_with("~~") {
                    out.push(Inline::Strike(children));
                } else {
                    // pi only strikes through with double tildes.
                    push_text(out, "~".to_owned());
                    for child in children {
                        match child {
                            Inline::Text(text) => push_text(out, text),
                            other => out.push(other),
                        }
                    }
                    push_text(out, "~".to_owned());
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let children = self.inlines(TagEnd::Link);
                let text = plain_text(&children);
                out.push(Inline::Link {
                    children,
                    text,
                    href: dest_url.into_string(),
                });
            }
            Event::Start(tag @ (Tag::Image { .. } | Tag::Superscript | Tag::Subscript)) => {
                let children = self.inlines(tag.to_end());
                out.extend(children);
            }
            _ => {}
        }
    }

    fn inlines(&mut self, end: TagEnd) -> Vec<Inline> {
        let mut out = Vec::new();
        while let Some((event, range)) = self.events.next() {
            if matches!(&event, Event::End(tag) if *tag == end) {
                break;
            }
            self.inline(event, range, &mut out);
        }
        out
    }

    fn table(&mut self, range: Range<usize>) -> Block {
        let mut header = Vec::new();
        let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
        let mut in_head = false;
        while let Some((event, _)) = self.events.next() {
            match event {
                Event::End(TagEnd::Table) => break,
                Event::Start(Tag::TableHead) => in_head = true,
                Event::End(TagEnd::TableHead) => in_head = false,
                Event::Start(Tag::TableRow) => rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => {
                    let cell = self.inlines(TagEnd::TableCell);
                    if in_head {
                        header.push(cell);
                    } else if let Some(row) = rows.last_mut() {
                        row.push(cell);
                    }
                }
                _ => {}
            }
        }
        Block::Table {
            header,
            rows,
            raw: self.source[range].trim_end().to_owned(),
        }
    }

    fn list_item_marker(&self, start: usize, ordered: bool) -> Option<String> {
        let line = self.source[start..].trim_start_matches(' ');
        if ordered {
            let digits = line
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(line.len());
            let delimiter = line[digits..]
                .chars()
                .next()
                .filter(|c| matches!(c, '.' | ')'))?;
            (digits > 0 && digits <= 9).then(|| format!("{}{delimiter} ", &line[..digits]))
        } else {
            line.chars()
                .next()
                .filter(|c| matches!(c, '-' | '+' | '*'))
                .map(|c| format!("{c} "))
        }
    }

    fn list(&mut self, start: Option<u64>) -> Block {
        let ordered = start.is_some();
        let mut items = Vec::new();
        let mut loose = false;
        while let Some((event, range)) = self.events.next() {
            match event {
                Event::End(TagEnd::List(_)) => break,
                Event::Start(Tag::Item) => {
                    let marker = self.list_item_marker(range.start, ordered);
                    let task = match self.events.peek() {
                        Some((Event::TaskListMarker(checked), _)) => {
                            let checked = *checked;
                            self.events.next();
                            Some(checked)
                        }
                        _ => None,
                    };
                    let blocks = self.blocks(Some(TagEnd::Item));
                    loose |= blocks
                        .iter()
                        .any(|spaced| matches!(spaced.block, Block::Paragraph(_)));
                    items.push(Item {
                        marker,
                        task,
                        blocks,
                    });
                }
                _ => {}
            }
        }
        Block::List {
            ordered,
            start: start.unwrap_or(1),
            loose,
            items,
        }
    }

    /// Blocks until `end`. Inline content outside a paragraph, as in tight list
    /// items, becomes a text block.
    fn blocks(&mut self, end: Option<TagEnd>) -> Vec<Spaced> {
        let mut out: Vec<Spaced> = Vec::new();
        while let Some((event, range)) = self.events.next() {
            if matches!(&event, Event::End(tag) if Some(*tag) == end) {
                break;
            }
            let mut block_end = range.end;
            let block = if is_inline(&event) {
                let mut inlines = Vec::new();
                self.inline(event, range, &mut inlines);
                while let Some((next, _)) = self.events.peek() {
                    if !is_inline(next) {
                        break;
                    }
                    if let Some((next, next_range)) = self.events.next() {
                        block_end = next_range.end;
                        self.inline(next, next_range, &mut inlines);
                    }
                }
                Some(Block::Text(inlines))
            } else {
                match event {
                    Event::Start(Tag::Paragraph) => {
                        Some(Block::Paragraph(self.inlines(TagEnd::Paragraph)))
                    }
                    Event::Start(Tag::Heading { level, .. }) => {
                        let depth = level as u8;
                        Some(Block::Heading(depth, self.inlines(TagEnd::Heading(level))))
                    }
                    Event::Start(Tag::CodeBlock(kind)) => {
                        let lang = match kind {
                            CodeBlockKind::Fenced(info) => info.trim().to_owned(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        let mut code = String::new();
                        for (event, _) in self.events.by_ref() {
                            match event {
                                Event::Text(text) => code.push_str(&text),
                                Event::End(TagEnd::CodeBlock) => break,
                                _ => {}
                            }
                        }
                        if code.ends_with('\n') {
                            code.pop();
                        }
                        trim_partial_closing_fence(
                            self.source.get(range.clone()).unwrap_or_default(),
                            &mut code,
                        );
                        Some(Block::Code(lang, code))
                    }
                    Event::Start(Tag::BlockQuote(kind)) => {
                        Some(Block::Quote(self.blocks(Some(TagEnd::BlockQuote(kind)))))
                    }
                    Event::Start(Tag::List(start)) => Some(self.list(start)),
                    Event::Start(Tag::Table(_)) => Some(self.table(range)),
                    Event::Rule => Some(Block::Rule),
                    Event::Start(Tag::HtmlBlock) => {
                        let mut html = String::new();
                        for (event, _) in self.events.by_ref() {
                            match event {
                                Event::Html(text) | Event::Text(text) => html.push_str(&text),
                                Event::End(TagEnd::HtmlBlock) => break,
                                _ => {}
                            }
                        }
                        Some(Block::Html(html.trim().to_owned()))
                    }
                    _ => None,
                }
            };
            if let Some(block) = block {
                out.push(Spaced {
                    block,
                    blank_after: blank_line_after(self.source, block_end, false),
                });
            }
        }
        // Blank lines after the last nested block belong to the enclosing block;
        // at the top level they still render as one blank line.
        if let Some(last) = out.last_mut() {
            last.blank_after =
                end.is_none() && blank_line_after(self.source, self.source.len(), true);
        }
        out
    }
}

fn push_text(out: &mut Vec<Inline>, text: String) {
    if let Some(Inline::Text(last)) = out.last_mut() {
        last.push_str(&text);
    } else {
        out.push(Inline::Text(text));
    }
}

fn plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::Strong(children) | Inline::Emphasis(children) | Inline::Strike(children) => {
                out.push_str(&plain_text(children))
            }
            Inline::Link { children, .. } => out.push_str(&plain_text(children)),
            Inline::Break => out.push('\n'),
        }
    }
    out
}

/// What follows a block, for pi's spacing rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    None,
    Space,
    List,
    Other,
}

fn next_of(blocks: &[Spaced], index: usize) -> Next {
    if blocks[index].blank_after {
        return Next::Space;
    }
    match blocks.get(index + 1).map(|spaced| &spaced.block) {
        None => Next::None,
        Some(Block::List { .. }) => Next::List,
        Some(_) => Next::Other,
    }
}

/// A quoted line's spans under the quote style. pi colors the whole line,
/// and the first span with a color of its own, such as a list bullet, ends
/// with a foreground reset that leaves the rest of the line uncolored but
/// still italic.
fn quote_spans(spans: Vec<Span<'static>>, quote: Style) -> Vec<Span<'static>> {
    let mut outer = quote;
    spans
        .into_iter()
        .map(|span| {
            let style = outer.patch(span.style);
            if span.style.fg.is_some() {
                outer.fg = None;
            }
            Span::styled(span.content, style)
        })
        .collect()
}

fn longest_word(line: &StyledLine, cap: usize) -> usize {
    crate::lines::plain(line)
        .split_whitespace()
        .map(visible_width)
        .max()
        .unwrap_or(0)
        .min(cap)
}

struct Renderer<'t> {
    theme: &'t MarkdownTheme,
    options: MarkdownOptions,
}

impl Renderer<'_> {
    fn inlines(&self, inlines: &[Inline], text_style: Style) -> Vec<Span<'static>> {
        let mut out = Vec::new();
        for inline in inlines {
            match inline {
                Inline::Text(text) => out.push(Span::styled(text.clone(), text_style)),
                Inline::Code(code) => out.push(Span::styled(code.clone(), self.theme.code)),
                Inline::Strong(children) => out.extend(crate::lines::under(
                    self.inlines(children, text_style),
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Inline::Emphasis(children) => out.extend(crate::lines::under(
                    self.inlines(children, text_style),
                    Style::new().add_modifier(Modifier::ITALIC),
                )),
                Inline::Strike(children) => out.extend(crate::lines::under(
                    self.inlines(children, text_style),
                    Style::new().add_modifier(Modifier::CROSSED_OUT),
                )),
                Inline::Link {
                    children,
                    text,
                    href,
                } => {
                    let link = self.theme.link.add_modifier(Modifier::UNDERLINED);
                    out.extend(crate::lines::under(
                        self.inlines(children, text_style),
                        link,
                    ));
                    let bare = href.strip_prefix("mailto:").unwrap_or(href);
                    if text != href && text != bare {
                        out.push(Span::styled(format!(" ({href})"), self.theme.link_url));
                    }
                }
                Inline::Break => out.push(Span::raw("\n")),
            }
        }
        out
    }

    fn text_style(&self, quoted: bool) -> Style {
        if quoted {
            Style::new()
        } else {
            self.options.text.unwrap_or_default()
        }
    }

    fn block(&self, block: &Block, width: usize, next: Next, quoted: bool) -> Vec<StyledLine> {
        let mut lines = Vec::new();
        let spaced_unless = |lines: &mut Vec<StyledLine>, skip_list: bool| {
            let add = match next {
                Next::None | Next::Space => false,
                Next::List => !skip_list,
                Next::Other => true,
            };
            if add {
                lines.push(Line::default());
            }
        };
        match block {
            Block::Heading(depth, inlines) => {
                let mut style = self.theme.heading.add_modifier(Modifier::BOLD);
                if *depth == 1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                let mut spans = Vec::new();
                if *depth >= 3 {
                    spans.push(Span::styled(
                        format!("{} ", "#".repeat(usize::from(*depth))),
                        style,
                    ));
                }
                spans.extend(self.inlines(inlines, style));
                lines.push(Line::from(spans));
                spaced_unless(&mut lines, false);
            }
            Block::Paragraph(inlines) => {
                lines.push(Line::from(self.inlines(inlines, self.text_style(quoted))));
                spaced_unless(&mut lines, true);
            }
            Block::Text(inlines) => {
                lines.push(Line::from(self.inlines(inlines, self.text_style(quoted))))
            }
            Block::Code(lang, code) => {
                let border = self.theme.code_block_border;
                lines.push(Line::from(Span::styled(format!("```{lang}"), border)));
                for code_line in code.split('\n') {
                    lines.push(Line::from(vec![
                        Span::raw(self.theme.code_block_indent.clone()),
                        Span::styled(code_line.to_owned(), self.theme.code_block),
                    ]));
                }
                lines.push(Line::from(Span::styled("```", border)));
                spaced_unless(&mut lines, false);
            }
            Block::List {
                ordered,
                start,
                loose,
                items,
            } => lines.extend(self.list(*ordered, *start, *loose, items, 0, width, quoted)),
            Block::Quote(blocks) => {
                let quote_width = width.saturating_sub(2).max(1);
                let mut inner = Vec::new();
                for (index, spaced) in blocks.iter().enumerate() {
                    inner.extend(self.block(
                        &spaced.block,
                        quote_width,
                        next_of(blocks, index),
                        true,
                    ));
                    if spaced.blank_after {
                        inner.push(Line::default());
                    }
                }
                while inner
                    .last()
                    .is_some_and(|line| line.spans.iter().all(|s| s.content.is_empty()))
                {
                    inner.pop();
                }
                let quote_style = self.theme.quote.add_modifier(Modifier::ITALIC);
                for line in inner {
                    let styled = Line::from(quote_spans(line.spans, quote_style));
                    for wrapped in wrap(&styled, quote_width) {
                        let mut spans = vec![Span::styled("│ ", self.theme.quote_border)];
                        spans.extend(wrapped.spans);
                        lines.push(Line::from(spans));
                    }
                }
                spaced_unless(&mut lines, false);
            }
            Block::Rule => {
                lines.push(Line::from(Span::styled(
                    "─".repeat(width.min(80)),
                    self.theme.hr,
                )));
                spaced_unless(&mut lines, false);
            }
            Block::Html(html) => lines.push(Line::from(Span::styled(
                html.clone(),
                self.text_style(quoted),
            ))),
            Block::Table { header, rows, raw } => {
                lines.extend(self.table(header, rows, raw, width, quoted));
                spaced_unless(&mut lines, false);
            }
        }
        lines
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors pi's recursive renderList"
    )]
    fn list(
        &self,
        ordered: bool,
        start: u64,
        loose: bool,
        items: &[Item],
        depth: usize,
        width: usize,
        quoted: bool,
    ) -> Vec<StyledLine> {
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        for (index, item) in items.iter().enumerate() {
            let default_bullet = if ordered {
                format!("{}. ", start + index as u64)
            } else {
                "- ".to_owned()
            };
            let bullet = if self.options.preserve_list_markers {
                item.marker.clone().unwrap_or(default_bullet)
            } else {
                default_bullet
            };
            let task = match item.task {
                Some(true) => "[x] ",
                Some(false) => "[ ] ",
                None => "",
            };
            let marker = format!("{bullet}{task}");
            let marker_width = visible_width(&marker);
            let first_prefix = vec![
                Span::raw(indent.clone()),
                Span::styled(marker, self.theme.list_bullet),
            ];
            let continuation = format!("{indent}{}", " ".repeat(marker_width));
            let item_width = width.saturating_sub(indent.len() + marker_width).max(1);
            let mut rendered_any = false;
            let count = item.blocks.len();
            for (block_index, spaced) in item.blocks.iter().enumerate() {
                if let Block::List {
                    ordered,
                    start,
                    loose,
                    items,
                } = &spaced.block
                {
                    lines.extend(self.list(
                        *ordered,
                        *start,
                        *loose,
                        items,
                        depth + 1,
                        width,
                        quoted,
                    ));
                    rendered_any = true;
                } else {
                    let mut block_lines = self.block(&spaced.block, item_width, Next::None, quoted);
                    if spaced.blank_after && block_index + 1 < count {
                        block_lines.push(Line::default());
                    }
                    for line in block_lines {
                        for wrapped in wrap(&line, item_width) {
                            let mut spans = if rendered_any {
                                vec![Span::raw(continuation.clone())]
                            } else {
                                first_prefix.clone()
                            };
                            spans.extend(wrapped.spans);
                            lines.push(Line::from(spans));
                            rendered_any = true;
                        }
                    }
                }
            }
            if !rendered_any {
                lines.push(Line::from(first_prefix));
            }
            if loose && index + 1 < items.len() {
                lines.push(Line::default());
            }
        }
        lines
    }

    fn table(
        &self,
        header: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        raw: &str,
        available: usize,
        quoted: bool,
    ) -> Vec<StyledLine> {
        let columns = header.len();
        if columns == 0 {
            return Vec::new();
        }
        let overhead = 3 * columns + 1;
        if available < overhead + columns {
            return wrap(&raw_line(raw), available);
        }
        let for_cells = available - overhead;
        let style = self.text_style(quoted);
        let render = |cell: &[Inline]| Line::from(self.inlines(cell, style));
        let header_lines: Vec<StyledLine> = header.iter().map(|cell| render(cell)).collect();
        let row_lines: Vec<Vec<StyledLine>> = rows
            .iter()
            .map(|row| row.iter().map(|cell| render(cell)).collect())
            .collect();
        let mut natural: Vec<usize> = header_lines.iter().map(line_width).collect();
        let mut min_words: Vec<usize> = header_lines
            .iter()
            .map(|line| longest_word(line, 30).max(1))
            .collect();
        for row in &row_lines {
            for (index, cell) in row.iter().enumerate().take(columns) {
                natural[index] = natural[index].max(line_width(cell));
                min_words[index] = min_words[index].max(longest_word(cell, 30));
            }
        }
        let mut min_columns = min_words.clone();
        let mut min_total: usize = min_columns.iter().sum();
        if min_total > for_cells {
            min_columns = vec![1; columns];
            let remaining = for_cells - columns;
            if remaining > 0 {
                let total_weight: usize = min_words.iter().map(|w| w.saturating_sub(1)).sum();
                let growth: Vec<usize> = min_words
                    .iter()
                    .map(|w| {
                        (w.saturating_sub(1) * remaining)
                            .checked_div(total_weight)
                            .unwrap_or(0)
                    })
                    .collect();
                for (index, grow) in growth.iter().enumerate() {
                    min_columns[index] += grow;
                }
                let mut leftover = remaining - growth.iter().sum::<usize>();
                for column in min_columns.iter_mut() {
                    if leftover == 0 {
                        break;
                    }
                    *column += 1;
                    leftover -= 1;
                }
            }
            min_total = min_columns.iter().sum();
        }
        let widths: Vec<usize> = if natural.iter().sum::<usize>() + overhead <= available {
            natural
                .iter()
                .zip(&min_columns)
                .map(|(n, m)| (*n).max(*m))
                .collect()
        } else {
            let potential: usize = natural
                .iter()
                .zip(&min_columns)
                .map(|(n, m)| n.saturating_sub(*m))
                .sum();
            let extra = for_cells.saturating_sub(min_total);
            let mut widths: Vec<usize> = min_columns
                .iter()
                .zip(&natural)
                .map(|(m, n)| {
                    let delta = n.saturating_sub(*m);
                    m + (delta * extra).checked_div(potential).unwrap_or(0)
                })
                .collect();
            let mut remaining = for_cells.saturating_sub(widths.iter().sum());
            while remaining > 0 {
                let mut grew = false;
                for (index, width) in widths.iter_mut().enumerate() {
                    if remaining > 0 && *width < natural[index] {
                        *width += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            widths
        };
        let rule = |left: &str, middle: &str, right: &str| -> StyledLine {
            let cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            raw_line(format!(
                "{left}─{}─{right}",
                cells.join(&format!("─{middle}─"))
            ))
        };
        let row = |cells: &[StyledLine], bold: bool| -> Vec<StyledLine> {
            let wrapped: Vec<Vec<StyledLine>> = cells
                .iter()
                .enumerate()
                .map(|(index, cell)| wrap(cell, widths[index].max(1)))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            (0..height)
                .map(|line_index| {
                    let mut spans = vec![Span::raw("│ ")];
                    for (index, cell_lines) in wrapped.iter().enumerate() {
                        if index > 0 {
                            spans.push(Span::raw(" │ "));
                        }
                        let cell = pad(
                            cell_lines.get(line_index).cloned().unwrap_or_default(),
                            widths[index],
                        );
                        if bold {
                            spans.extend(crate::lines::under(
                                cell.spans,
                                Style::new().add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.extend(cell.spans);
                        }
                    }
                    spans.push(Span::raw(" │"));
                    Line::from(spans)
                })
                .collect()
        };
        let mut lines = vec![rule("┌", "┬", "┐")];
        lines.extend(row(&header_lines, true));
        lines.push(rule("├", "┼", "┤"));
        for (index, cells) in row_lines.iter().enumerate() {
            let mut cells = cells.clone();
            cells.resize(columns, Line::default());
            lines.extend(row(&cells, false));
            if index + 1 < row_lines.len() {
                lines.push(rule("├", "┼", "┤"));
            }
        }
        lines.push(rule("└", "┴", "┘"));
        lines
    }
}

/// Renders markdown at `width` columns with `px` columns of padding on each
/// side and `py` blank rows above and below. Blank text renders nothing.
pub fn render(
    text: &str,
    width: usize,
    px: usize,
    py: usize,
    theme: &MarkdownTheme,
    options: MarkdownOptions,
) -> Vec<StyledLine> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let content_width = width.saturating_sub(px * 2).max(1);
    let source = text.replace('\t', "   ");
    let mut parser_options = Options::empty();
    parser_options.insert(Options::ENABLE_TABLES);
    parser_options.insert(Options::ENABLE_STRIKETHROUGH);
    parser_options.insert(Options::ENABLE_TASKLISTS);
    let mut builder = Builder {
        source: &source,
        events: Parser::new_ext(&source, parser_options)
            .into_offset_iter()
            .peekable(),
        preserve_escapes: options.preserve_backslash_escapes,
    };
    let blocks = builder.blocks(None);
    let renderer = Renderer { theme, options };
    let mut rendered = Vec::new();
    for (index, spaced) in blocks.iter().enumerate() {
        rendered.extend(renderer.block(
            &spaced.block,
            content_width,
            next_of(&blocks, index),
            false,
        ));
        if spaced.blank_after {
            rendered.push(Line::default());
        }
    }
    let finish = |line: StyledLine| -> StyledLine {
        let mut spans = vec![Span::raw(" ".repeat(px))];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(px)));
        crate::lines::fill(Line::from(spans), width, options.background)
    };
    let mut out: Vec<StyledLine> = (0..py).map(|_| finish(Line::default())).collect();
    for line in rendered {
        out.extend(wrap(&line, content_width).into_iter().map(&finish));
    }
    out.extend((0..py).map(|_| finish(Line::default())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::plain;

    fn md(text: &str, width: usize) -> Vec<String> {
        render(
            text,
            width,
            0,
            0,
            &MarkdownTheme::default(),
            MarkdownOptions::default(),
        )
        .iter()
        .map(|line| plain(line).trim_end().to_owned())
        .collect()
    }

    #[test]
    fn trims_a_streaming_closing_fence() {
        assert_eq!(
            md("```rust\nlet a = 1;\n``", 40),
            ["```rust", "  let a = 1;", "```"]
        );
        assert_eq!(
            md("```rust\nlet a = 1;\n```", 40),
            ["```rust", "  let a = 1;", "```"]
        );
        let tilde = md("~~~\nx\n~", 40);
        assert_eq!((tilde.len(), tilde[1].as_str()), (3, "  x"));
    }

    #[test]
    fn entity_references_stay_as_written() {
        assert_eq!(
            md(
                "Entities: &amp; &lt;tag&gt; &copy; &#x1F600; and `&amp;`",
                80
            ),
            ["Entities: &amp; &lt;tag&gt; &copy; &#x1F600; and &amp;"]
        );
    }

    #[test]
    fn spaces_blocks_like_pi() {
        assert_eq!(md("p\n\np", 20), ["p", "", "p"]);
        assert_eq!(md("p\n\n\n\np", 20), ["p", "", "p"]);
        assert_eq!(md("# H\np", 20), ["H", "", "p"]);
        assert_eq!(md("p\n- a", 20), ["p", "- a"]);
        assert_eq!(md("### Third", 20), ["### Third"]);
        assert_eq!(md("one\ntwo", 20), ["one", "two"]);
    }

    #[test]
    fn renders_code_lists_quotes() {
        assert_eq!(
            md("```rs\nfn x() {}\n```", 20),
            ["```rs", "  fn x() {}", "```"]
        );
        assert_eq!(md("- a\n  - b\n- c", 20), ["- a", "    - b", "- c"]);
        assert_eq!(md("1. a\n2. b", 20), ["1. a", "2. b"]);
        assert_eq!(md("- [x] done", 20), ["- [x] done"]);
        assert_eq!(md("> quoted\n> text", 20), ["│ quoted", "│ text"]);
        assert_eq!(md("[a](http://x)", 30), ["a (http://x)"]);
        assert_eq!(md("---", 5), ["─────"]);
    }

    #[test]
    fn renders_tables() {
        assert_eq!(
            md("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |", 30),
            [
                "┌───┬───┐",
                "│ a │ b │",
                "├───┼───┤",
                "│ 1 │ 2 │",
                "├───┼───┤",
                "│ 3 │ 4 │",
                "└───┴───┘"
            ]
        );
    }
}
