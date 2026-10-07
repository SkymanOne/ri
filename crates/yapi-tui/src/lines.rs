//! Operations on styled lines and pi-tui's basic components.
//!
//! Ports of `wrapTextWithAnsi`, `truncateToWidth` and `applyBackgroundToLine`
//! in `packages/tui/src/utils.ts`, and of the `Text`, `TruncatedText`, `Box`,
//! `Spacer` and `DynamicBorder` components, in pi `v1.0.0`. Styles live on
//! spans, so a style that crosses a line break simply continues.

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::text::{grapheme_width, is_cjk, is_js_whitespace};

/// A line of the rendered document.
pub type StyledLine = Line<'static>;

/// One grapheme and its style.
#[derive(Clone, Debug, PartialEq)]
struct Cell {
    text: String,
    style: Style,
}

fn cells(line: &Line<'_>) -> Vec<Cell> {
    let mut out = Vec::new();
    for span in &line.spans {
        let style = line.style.patch(span.style);
        for grapheme in span.content.graphemes(true) {
            out.push(Cell {
                text: grapheme.to_owned(),
                style,
            });
        }
    }
    out
}

fn from_cells(cells: &[Cell]) -> StyledLine {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for cell in cells {
        match spans.last_mut() {
            Some(last) if last.style == cell.style => last.content.to_mut().push_str(&cell.text),
            _ => spans.push(Span::styled(cell.text.clone(), cell.style)),
        }
    }
    Line::from(spans)
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|cell| grapheme_width(&cell.text)).sum()
}

/// Columns a line occupies.
pub fn width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| crate::text::visible_width(&span.content))
        .sum()
}

/// A line of plain text, which may contain newlines for [`wrap`] to split.
///
/// ratatui's `Line::from(&str)` drops newlines; this keeps them.
pub fn raw(text: impl Into<String>) -> StyledLine {
    Line::from(Span::raw(text.into()))
}

/// A line of one styled text, which may contain newlines.
pub fn styled(text: impl Into<String>, style: Style) -> StyledLine {
    Line::from(Span::styled(text.into(), style))
}

fn is_space_cell(cell: &Cell) -> bool {
    cell.text.chars().all(is_js_whitespace)
}

fn trim_end(cells: &mut Vec<Cell>) {
    while cells.last().is_some_and(is_space_cell) {
        cells.pop();
    }
}

/// Splits a line at newlines (`\r\n`, `\r`, `\n`), keeping styles.
fn split_lines(line: &Line<'_>) -> Vec<Vec<Cell>> {
    let mut out = vec![Vec::new()];
    for cell in cells(line) {
        if matches!(cell.text.as_str(), "\n" | "\r" | "\r\n") {
            out.push(Vec::new());
        } else if let Some(last) = out.last_mut() {
            last.push(cell);
        }
    }
    out
}

fn tokens(cells: Vec<Cell>) -> Vec<Vec<Cell>> {
    let mut tokens: Vec<Vec<Cell>> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut current_space = false;
    for cell in cells {
        let space = cell.text == " ";
        if !space && cell.text.chars().any(is_cjk) {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(vec![cell]);
            continue;
        }
        if !current.is_empty() && current_space != space {
            tokens.push(std::mem::take(&mut current));
        }
        current_space = space;
        current.push(cell);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn break_long(token: Vec<Cell>, width: usize, current: &mut Vec<Cell>, out: &mut Vec<Vec<Cell>>) {
    let mut used = cells_width(current);
    for cell in token {
        let cell_width = grapheme_width(&cell.text);
        if used + cell_width > width && !current.is_empty() {
            out.push(std::mem::take(current));
            used = 0;
        }
        used += cell_width;
        current.push(cell);
    }
}

fn wrap_cells(line: Vec<Cell>, width: usize) -> Vec<Vec<Cell>> {
    if line.is_empty() || cells_width(&line) <= width {
        return vec![line];
    }
    let mut out: Vec<Vec<Cell>> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut current_width = 0;
    for token in tokens(line) {
        let token_width = cells_width(&token);
        let whitespace = token.iter().all(is_space_cell);
        if token_width > width && !whitespace {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            break_long(token, width, &mut current, &mut out);
            current_width = cells_width(&current);
            continue;
        }
        if current_width + token_width > width && current_width > 0 {
            let mut finished = std::mem::take(&mut current);
            trim_end(&mut finished);
            out.push(finished);
            if whitespace {
                current_width = 0;
            } else {
                current = token;
                current_width = token_width;
            }
        } else {
            current.extend(token);
            current_width += token_width;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    for line in &mut out {
        trim_end(line);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

/// Wraps a line, which may contain newlines, to `width` columns: breaks fall
/// between words or beside CJK characters, words wider than a line break by
/// grapheme, and trailing spaces are dropped.
pub fn wrap(line: &Line<'_>, width: usize) -> Vec<StyledLine> {
    let width = width.max(1);
    split_lines(line)
        .into_iter()
        .flat_map(|cells| wrap_cells(cells, width))
        .map(|cells| from_cells(&cells))
        .collect()
}

/// Wraps several lines.
pub fn wrap_all(lines: &[Line<'_>], width: usize) -> Vec<StyledLine> {
    lines.iter().flat_map(|line| wrap(line, width)).collect()
}

/// Truncates a line to `max_width` columns, appending an unstyled `ellipsis`
/// when cut.
pub fn truncate(line: &Line<'_>, max_width: usize, ellipsis: &str) -> StyledLine {
    if max_width == 0 {
        return Line::default();
    }
    let all = cells(line);
    if cells_width(&all) <= max_width {
        return from_cells(&all);
    }
    let (ellipsis, ellipsis_width) = crate::text::fit_ellipsis(ellipsis, max_width);
    let target = max_width - ellipsis_width;
    let mut kept = Vec::new();
    let mut used = 0;
    for cell in all {
        let cell_width = grapheme_width(&cell.text);
        if used + cell_width > target {
            break;
        }
        used += cell_width;
        kept.push(cell);
    }
    let mut out = from_cells(&kept);
    if !ellipsis.is_empty() {
        out.spans.push(Span::raw(ellipsis));
    }
    out
}

/// pi-tui's `compositeTuiLine`: `top`, cut or padded to `width` columns,
/// drawn over `base` from column `col`; the base shows before and after it.
/// A wide character cut by the left edge becomes spaces; one cut by the
/// right edge is dropped, so the rest of the base moves left and the line is
/// padded at its end. Spaces that reach `col` keep the style of the base's
/// last cell before it, as pi's do. The result is no wider than `total`.
pub(crate) fn composite(
    base: &Line<'_>,
    top: &Line<'_>,
    col: usize,
    width: usize,
    total: usize,
) -> StyledLine {
    let space = |style: Style| Cell {
        text: " ".to_owned(),
        style,
    };
    let base = cells(base);
    let end = col + width;
    let mut out: Vec<Cell> = Vec::new();
    let mut column = 0;
    for cell in &base {
        let cell_width = grapheme_width(&cell.text);
        if column + cell_width > col {
            break;
        }
        out.push(cell.clone());
        column += cell_width;
    }
    let style = out.last().map_or_else(Style::default, |cell| cell.style);
    out.extend(std::iter::repeat_with(|| space(style)).take(col.saturating_sub(column)));
    let mut used = 0;
    for cell in cells(top) {
        let cell_width = grapheme_width(&cell.text);
        if used + cell_width > width {
            break;
        }
        used += cell_width;
        out.push(cell);
    }
    out.extend(std::iter::repeat_with(|| space(Style::default())).take(width - used));
    let mut column = 0;
    for cell in &base {
        let cell_width = grapheme_width(&cell.text);
        if column >= end && column + cell_width <= total {
            out.push(cell.clone());
        }
        column += cell_width;
    }
    let filled = cells_width(&out);
    out.extend(
        std::iter::repeat_with(|| space(Style::default())).take(total.saturating_sub(filled)),
    );
    let mut line = from_cells(&out);
    if cells_width(&out) > total {
        line = truncate(&line, total, "");
    }
    line
}

/// Whether a grapheme `width` wide at `column` lies within columns
/// `from..to`, as pi-tui's `sliceByColumn` with `strict` keeps it.
fn within(column: usize, width: usize, from: usize, to: usize) -> bool {
    column >= from && column < to && column + width <= to
}

/// `line` with `style` over its graphemes within columns `from..to`.
pub(crate) fn restyle_columns(line: &Line<'_>, from: usize, to: usize, style: Style) -> StyledLine {
    let mut cells = cells(line);
    let mut column = 0;
    for cell in &mut cells {
        let cell_width = grapheme_width(&cell.text);
        if within(column, cell_width, from, to) {
            cell.style = cell.style.patch(style);
        }
        column += cell_width;
    }
    from_cells(&cells)
}

/// The text of `line`'s graphemes within columns `from..to`.
pub(crate) fn text_in_columns(line: &Line<'_>, from: usize, to: usize) -> String {
    let mut column = 0;
    let mut out = String::new();
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let cell_width = grapheme_width(grapheme);
            if within(column, cell_width, from, to) {
                out.push_str(grapheme);
            }
            column += cell_width;
        }
    }
    out
}

/// pi-tui's `getGraphemeCellRange`: the columns of the grapheme that covers
/// `column`.
pub(crate) fn cell_range(line: &Line<'_>, column: usize) -> Option<(usize, usize)> {
    let mut at = 0;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let cell_width = grapheme_width(grapheme);
            if cell_width > 0 && column >= at && column < at + cell_width {
                return Some((at, at + cell_width));
            }
            at += cell_width;
        }
    }
    None
}

/// Pads a line with spaces to `width` columns.
pub(crate) fn pad(mut line: StyledLine, width: usize) -> StyledLine {
    let used = self::width(&line);
    if used < width {
        line.spans.push(Span::raw(" ".repeat(width - used)));
    }
    line
}

/// `spans` with `style` under each; a span's own style wins.
pub(crate) fn under(spans: Vec<Span<'static>>, style: Style) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| Span::styled(span.content, style.patch(span.style)))
        .collect()
}

/// Pads a line to `width`, with `bg` under every span when given.
pub(crate) fn fill(line: StyledLine, width: usize, bg: Option<Style>) -> StyledLine {
    let line = pad(line, width);
    match bg {
        Some(bg) => Line::from(under(line.spans, bg)),
        None => line,
    }
}

fn indent(line: StyledLine, columns: usize) -> StyledLine {
    if columns == 0 {
        return line;
    }
    let mut spans = vec![Span::raw(" ".repeat(columns))];
    spans.extend(line.spans);
    Line::from(spans)
}

fn is_blank(lines: &[Line<'_>]) -> bool {
    lines
        .iter()
        .all(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
}

/// pi-tui's `Text`: wraps `content` within horizontal padding, pads every row
/// to `width` (over `bg` when given) and adds `py` blank rows above and below.
/// Blank content renders nothing.
pub fn text(
    content: &[Line<'_>],
    width: usize,
    px: usize,
    py: usize,
    bg: Option<Style>,
) -> Vec<StyledLine> {
    if is_blank(content) {
        return Vec::new();
    }
    padded(content, width, px.min(width.saturating_sub(1) / 2), py, bg)
}

/// The layout pi-tui's `Text` and `Markdown` share: `content` wrapped
/// within `px` columns of padding on each side, every row padded to `width`
/// (over `bg` when given), and `py` blank rows above and below.
pub(crate) fn padded(
    content: &[Line<'_>],
    width: usize,
    px: usize,
    py: usize,
    bg: Option<Style>,
) -> Vec<StyledLine> {
    let content_width = width.saturating_sub(px * 2).max(1);
    let finish = |line: StyledLine| -> StyledLine {
        let mut line = indent(line, px);
        line.spans.push(Span::raw(" ".repeat(px)));
        fill(line, width, bg)
    };
    let blank = || finish(Line::default());
    let mut out: Vec<StyledLine> = (0..py).map(|_| blank()).collect();
    out.extend(wrap_all(content, content_width).into_iter().map(finish));
    out.extend((0..py).map(|_| blank()));
    out
}

/// pi-tui's `Text` holding one line, without vertical padding or background.
pub fn text_row(line: StyledLine, width: usize, px: usize) -> Vec<StyledLine> {
    text(&[line], width, px, 0, None)
}

/// pi-tui's `TruncatedText`: the first line only, cut to fit with `...`.
pub fn truncated_text(content: &Line<'_>, width: usize, px: usize) -> StyledLine {
    let available = width.saturating_sub(px * 2).max(1);
    let first = split_lines(content).into_iter().next().unwrap_or_default();
    let line = truncate(&from_cells(&first), available, "...");
    let mut line = indent(line, px);
    line.spans.push(Span::raw(" ".repeat(px)));
    pad(line, width)
}

/// pi-tui's `Box`: `children` were rendered at `width - 2 * px`; each row is
/// indented by `px`, padded to `width` over `bg`, with `py` padding rows.
/// Renders nothing when the children are empty.
pub fn boxed(
    children: Vec<StyledLine>,
    width: usize,
    px: usize,
    py: usize,
    bg: Option<Style>,
) -> Vec<StyledLine> {
    if children.is_empty() {
        return Vec::new();
    }
    let finish = |line: StyledLine| fill(line, width, bg);
    let mut out: Vec<StyledLine> = (0..py).map(|_| finish(Line::default())).collect();
    out.extend(children.into_iter().map(|line| finish(indent(line, px))));
    out.extend((0..py).map(|_| finish(Line::default())));
    out
}

/// The inner width of a [`boxed`] block.
pub fn box_content_width(width: usize, px: usize) -> usize {
    width.saturating_sub(px * 2).max(1)
}

/// pi's `DynamicBorder`: a full-width rule.
pub fn border(width: usize, style: Style) -> StyledLine {
    styled("─".repeat(width.max(1)), style)
}

/// `n` empty rows.
pub fn spacer(n: usize) -> Vec<StyledLine> {
    (0..n).map(|_| Line::default()).collect()
}

/// Plain text of a line, for tests and comparisons.
pub fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui_core::style::{Color, Modifier};

    use super::*;

    #[test]
    fn composite_drops_a_wide_character_cut_by_the_right_edge() {
        let base = Line::from("ab日本cd");
        let top = Line::from("XY");
        assert_eq!(plain(&composite(&base, &top, 2, 3, 8)), "abXY cd ");
        // At the left edge it becomes a space.
        assert_eq!(plain(&composite(&base, &top, 3, 2, 8)), "ab XYcd ");
        assert_eq!(plain(&composite(&base, &top, 2, 2, 8)), "abXY本cd");
    }

    fn texts(lines: &[StyledLine]) -> Vec<String> {
        lines.iter().map(plain).collect()
    }

    #[test]
    fn wraps_words_and_keeps_styles() {
        let line = Line::from(vec![
            Span::raw("hello "),
            Span::styled("bold world", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" again"),
        ]);
        let wrapped = wrap(&line, 11);
        assert_eq!(texts(&wrapped), ["hello bold", "world again"]);
        assert_eq!(
            wrapped[1].spans[0].style,
            Style::new().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            texts(&wrap(&Line::from("abcdefghij"), 4)),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(texts(&wrap(&raw("a\nb"), 4)), ["a", "b"]);
        assert_eq!(
            texts(&wrap(&Line::from("日本語テキスト"), 6)),
            ["日本語", "テキス", "ト"]
        );
        assert_eq!(texts(&wrap(&Line::from(""), 4)), [""]);
    }

    #[test]
    fn truncates_and_pads() {
        assert_eq!(
            plain(&truncate(&Line::from("hello world"), 8, "...")),
            "hello..."
        );
        assert_eq!(plain(&truncate(&Line::from("hi"), 8, "...")), "hi");
        let boxed = boxed(
            vec![Line::from("x")],
            5,
            1,
            1,
            Some(Style::new().bg(Color::Red)),
        );
        assert_eq!(texts(&boxed), ["     ", " x   ", "     "]);
        assert_eq!(boxed[1].spans[1].style.bg, Some(Color::Red));
        assert_eq!(
            texts(&text(&[Line::from("a b c")], 5, 1, 0, None)),
            [" a b ", " c   "]
        );
        assert!(text(&[Line::from("  ")], 5, 1, 1, None).is_empty());
    }
}
