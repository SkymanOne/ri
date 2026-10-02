//! Styled lines to terminal escape sequences.

use std::fmt::Write;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

/// Resets all attributes at the end of every line, as pi does, so styles never
/// leak across lines.
pub const LINE_RESET: &str = "\x1b[0m";

fn color_code(out: &mut String, color: Color, background: bool) {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    let _ = match color {
        Color::Reset => write!(out, "{}", if background { 49 } else { 39 }),
        Color::Black => write!(out, "{base}"),
        Color::Red => write!(out, "{}", base + 1),
        Color::Green => write!(out, "{}", base + 2),
        Color::Yellow => write!(out, "{}", base + 3),
        Color::Blue => write!(out, "{}", base + 4),
        Color::Magenta => write!(out, "{}", base + 5),
        Color::Cyan => write!(out, "{}", base + 6),
        Color::Gray => write!(out, "{}", base + 7),
        Color::DarkGray => write!(out, "{bright}"),
        Color::LightRed => write!(out, "{}", bright + 1),
        Color::LightGreen => write!(out, "{}", bright + 2),
        Color::LightYellow => write!(out, "{}", bright + 3),
        Color::LightBlue => write!(out, "{}", bright + 4),
        Color::LightMagenta => write!(out, "{}", bright + 5),
        Color::LightCyan => write!(out, "{}", bright + 6),
        Color::White => write!(out, "{}", bright + 7),
        Color::Indexed(index) => write!(out, "{};5;{index}", base + 8),
        Color::Rgb(r, g, b) => write!(out, "{};2;{r};{g};{b}", base + 8),
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

/// `line` as text with escape sequences, ending with a full reset.
pub fn line_to_ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    let mut current = Style::default();
    for span in &line.spans {
        if span.content.is_empty() {
            continue;
        }
        let style = line.style.patch(span.style);
        if style != current {
            out.push_str(LINE_RESET);
            out.push_str(&sgr(style));
            current = style;
        }
        out.push_str(&span.content.replace('\t', "   "));
    }
    out.push_str(LINE_RESET);
    out
}

#[cfg(test)]
mod tests {
    use ratatui::text::Span;

    use super::*;

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
