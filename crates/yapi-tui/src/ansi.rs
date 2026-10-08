//! Styled lines to terminal escape sequences.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};

/// Resets all attributes at the end of every line, as pi does, so styles never
/// leak across lines.
pub(crate) const LINE_RESET: &str = "\x1b[0m";

/// Closes an OSC 8 hyperlink.
const LINK_CLOSE: &str = "\x1b]8;;\x1b\\";

/// The URLs of hyperlinks, by id.
#[derive(Default)]
struct Links {
    ids: HashMap<Arc<str>, u32>,
    urls: Vec<Arc<str>>,
}

// ponytail: grows by each distinct URL drawn and never shrinks; a session draws few.
static LINKS: LazyLock<Mutex<Links>> = LazyLock::new(Mutex::default);

/// `style` with its text linked to `url` by an OSC 8 hyperlink, as pi-tui's
/// `hyperlink`. The link rides in the style's underline color as an id that
/// no output writes as a color, so it never changes widths or wrapping.
pub fn link(style: Style, url: &str) -> Style {
    let mut links = LINKS.lock().unwrap_or_else(PoisonError::into_inner);
    let id = match links.ids.get(url) {
        Some(id) => *id,
        None => {
            let id = links.urls.len() as u32;
            if id >= 1 << 24 {
                return style;
            }
            let url: Arc<str> = url.into();
            links.ids.insert(Arc::clone(&url), id);
            links.urls.push(url);
            id
        }
    };
    let [_, r, g, b] = id.to_be_bytes();
    style.underline_color(Color::Rgb(r, g, b))
}

/// The URL `style` links to.
pub fn link_url(style: Style) -> Option<Arc<str>> {
    let Some(Color::Rgb(r, g, b)) = style.underline_color else {
        return None;
    };
    let id = u32::from_be_bytes([0, r, g, b]) as usize;
    let links = LINKS.lock().unwrap_or_else(PoisonError::into_inner);
    links.urls.get(id).cloned()
}

/// The 16 named colors in SGR order: 30-37, then the bright 90-97.
const NAMED: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

fn color_code(out: &mut String, color: Color, background: bool) {
    let base = if background { 40 } else { 30 };
    let _ = match color {
        Color::Reset => write!(out, "{}", base + 9),
        Color::Indexed(index) => write!(out, "{};5;{index}", base + 8),
        Color::Rgb(r, g, b) => write!(out, "{};2;{r};{g};{b}", base + 8),
        named => {
            let index = NAMED.iter().position(|color| *color == named).unwrap_or(0);
            write!(out, "{}", base + index % 8 + 60 * (index / 8))
        }
    };
}

/// The SGR sequence that sets `style` from a reset state; empty for the
/// default style.
pub fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    let modifiers = style.add_modifier;
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifiers.contains(modifier) {
            codes.push(code.to_owned());
        }
    }
    if let Some(fg) = style.fg {
        let mut code = String::new();
        color_code(&mut code, fg, false);
        codes.push(code);
    }
    if let Some(bg) = style.bg {
        let mut code = String::new();
        color_code(&mut code, bg, true);
        codes.push(code);
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// `line` as text with escape sequences, ending with a full reset. Linked
/// text is wrapped in OSC 8 hyperlinks, closed at the end of the line.
pub fn line_to_ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    let mut current = Style::default();
    let mut current_link = None;
    for span in &line.spans {
        if span.content.is_empty() {
            continue;
        }
        let mut style = line.style.patch(span.style);
        if style.underline_color != current_link {
            if current_link.is_some() {
                out.push_str(LINK_CLOSE);
            }
            if let Some(url) = link_url(style) {
                let _ = write!(out, "\x1b]8;;{url}\x1b\\");
            }
            current_link = style.underline_color;
        }
        style.underline_color = None;
        if style != current {
            out.push_str(LINE_RESET);
            out.push_str(&sgr(style));
            current = style;
        }
        out.push_str(&span.content.replace('\t', "   "));
    }
    if current_link.is_some() {
        out.push_str(LINK_CLOSE);
    }
    out.push_str(LINE_RESET);
    out
}

/// pi-tui's `CURSOR_MARKER`: an APC sequence a focused component puts where
/// the terminal cursor belongs.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

fn basic_color(index: u16, bright: bool) -> Color {
    NAMED[usize::from(index.min(7)) + 8 * usize::from(bright)]
}

/// An extended color from `38`/`48` parameters; consumes what it reads.
fn extended_color(params: &mut std::slice::Iter<'_, u16>) -> Option<Color> {
    match params.next()? {
        5 => params.next().map(|index| Color::Indexed(*index as u8)),
        2 => {
            let r = *params.next()? as u8;
            let g = *params.next()? as u8;
            let b = *params.next()? as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

/// Applies SGR parameters to `style`.
fn apply_sgr(style: &mut Style, params: &[u16]) {
    if params.is_empty() {
        *style = Style::default();
        return;
    }
    let mut params = params.iter();
    while let Some(code) = params.next() {
        match code {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 | 21 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            8 => *style = style.add_modifier(Modifier::HIDDEN),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            28 => *style = style.remove_modifier(Modifier::HIDDEN),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(basic_color(code - 30, false)),
            38 => style.fg = extended_color(&mut params).or(style.fg),
            39 => style.fg = None,
            40..=47 => style.bg = Some(basic_color(code - 40, false)),
            48 => style.bg = extended_color(&mut params).or(style.bg),
            49 => style.bg = None,
            90..=97 => style.fg = Some(basic_color(code - 90, true)),
            100..=107 => style.bg = Some(basic_color(code - 100, true)),
            _ => {}
        }
    }
}

/// A line of text with escape sequences as a styled line, and the column of
/// pi-tui's cursor marker when the line has one. SGR sequences become styles
/// and OSC 8 hyperlinks [`link`]s; other OSC and APC strings and other CSI
/// sequences are dropped, keeping their visible text.
pub fn parse_line(text: &str) -> (Line<'static>, Option<usize>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut current = String::new();
    let mut cursor = None;
    let mut column = 0;
    let mut chars = text.char_indices().peekable();
    let flush = |spans: &mut Vec<Span<'static>>, current: &mut String, style: Style| {
        if !current.is_empty() {
            spans.push(Span::styled(std::mem::take(current), style));
        }
    };
    while let Some((index, c)) = chars.next() {
        if c != '\x1b' {
            current.push(c);
            continue;
        }
        if text[index..].starts_with(CURSOR_MARKER) {
            column += crate::text::visible_width(&current);
            flush(&mut spans, &mut current, style);
            cursor = Some(column);
            for _ in 1..CURSOR_MARKER.chars().count() {
                chars.next();
            }
            continue;
        }
        match chars.peek().map(|(_, next)| *next) {
            Some('[') => {
                chars.next();
                let mut body = String::new();
                for (_, c) in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        if c == 'm' {
                            column += crate::text::visible_width(&current);
                            flush(&mut spans, &mut current, style);
                            let params: Vec<u16> = body
                                .split([';', ':'])
                                .map(|part| part.parse().unwrap_or(0))
                                .collect();
                            let params = if body.is_empty() { Vec::new() } else { params };
                            // A reset keeps the hyperlink, as in pi-tui.
                            let linked = style.underline_color;
                            apply_sgr(&mut style, &params);
                            style.underline_color = linked;
                        }
                        break;
                    }
                    body.push(c);
                }
            }
            // OSC, APC, DCS, PM and SOS strings end with BEL or ST.
            Some(kind @ (']' | '_' | 'P' | '^' | 'X')) => {
                chars.next();
                let mut body = String::new();
                while let Some((_, c)) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek().is_some_and(|(_, next)| *next == '\\') {
                        chars.next();
                        break;
                    }
                    body.push(c);
                }
                // An OSC 8 hyperlink: `8;params;url`, closed by an empty URL.
                if kind == ']'
                    && let Some((_, url)) = body
                        .strip_prefix("8;")
                        .and_then(|rest| rest.split_once(';'))
                {
                    column += crate::text::visible_width(&current);
                    flush(&mut spans, &mut current, style);
                    style.underline_color = None;
                    if !url.is_empty() {
                        style = link(style, url);
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    flush(&mut spans, &mut current, style);
    (Line::from(spans), cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_styles_links_and_the_cursor() {
        let (line, cursor) = parse_line(
            "a\x1b[1;38;2;1;2;3mb\x1b[0m\x1b[48;5;4mc\x1b[49m \x1b]8;;https://x\x07link\x1b]8;;\x07\x1b_pi:c\x07!",
        );
        assert_eq!(
            line_to_ansi(&line),
            "a\x1b[0m\x1b[1;38;2;1;2;3mb\x1b[0m\x1b[48;5;4mc\x1b[0m \x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\!\x1b[0m"
        );
        assert_eq!(cursor, Some(8));
        let (line, _) = parse_line("\x1b[31mred\x1b[39m \x1b[92mgreen\x1b[22m\x1b[2mdim\x1b[K");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert_eq!(line.spans[1].style.fg, None);
        assert_eq!(line.spans[2].style.fg, Some(Color::LightGreen));
        assert!(line.spans[3].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(crate::lines::width(&line), 12);
    }

    #[test]
    fn links_survive_resets_and_wrapping() {
        let (line, _) =
            parse_line("\x1b]8;;https://a\x1b\\\x1b[1mone\x1b[0m two\x1b]8;;\x1b\\ three");
        let lines = crate::lines::wrap(&line, 4);
        let ansi: Vec<String> = lines.iter().map(line_to_ansi).collect();
        assert_eq!(
            ansi,
            [
                "\x1b]8;;https://a\x1b\\\x1b[0m\x1b[1mone\x1b]8;;\x1b\\\x1b[0m",
                "\x1b]8;;https://a\x1b\\two\x1b]8;;\x1b\\\x1b[0m",
                "thre\x1b[0m",
                "e\x1b[0m",
            ]
        );
        assert_eq!(crate::lines::width(&line), 13);
    }

    #[test]
    fn serializes_styles() {
        let line = Line::from(vec![
            Span::raw("a"),
            Span::styled(
                "b",
                Style::new()
                    .fg(Color::Rgb(1, 2, 3))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("c", Style::new().bg(Color::Indexed(4))),
        ]);
        assert_eq!(
            line_to_ansi(&line),
            "a\x1b[0m\x1b[1;38;2;1;2;3mb\x1b[0m\x1b[48;5;4mc\x1b[0m"
        );
    }
}
