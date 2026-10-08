//! `/overlay-test` shows an overlay with inline text inputs, and lines of
//! wide characters, styled text and emoji that test how overlays are drawn
//! over the screen.
//!
//! A port of pi's `overlay-test.ts`. The overlay takes the component's
//! width, 70 columns, as no overlay options are given.

use yapi_extension_api::widgets::tui::ansi::CURSOR_MARKER;
use yapi_extension_api::widgets::visible_width;
use yapi_extension_api::{Api, Component, CustomOptions, Done, notify, parse_key, theme};

struct Item {
    label: &'static str,
    has_input: bool,
    text: Vec<char>,
    cursor: usize,
}

/// What the overlay finishes with: the action and its input.
type Choice = Option<(String, Option<String>)>;

struct OverlayTest {
    selected: usize,
    items: Vec<Item>,
    done: Done<Choice>,
}

const WIDTH: usize = 70;

impl Component for OverlayTest {
    fn width(&self) -> Option<usize> {
        Some(WIDTH)
    }

    fn handle_input(&mut self, data: &str) {
        let key = parse_key(data);
        if key.as_deref() == Some("escape") {
            return self.done.finish(None);
        }
        let current = &mut self.items[self.selected];
        if key.as_deref() == Some("enter") {
            let query = current.has_input.then(|| current.text.iter().collect());
            return self.done.finish(Some((current.label.to_owned(), query)));
        }
        match key.as_deref() {
            Some("up") => self.selected = self.selected.saturating_sub(1),
            Some("down") => self.selected = (self.selected + 1).min(self.items.len() - 1),
            _ if !current.has_input => {}
            Some("backspace") => {
                if current.cursor > 0 {
                    current.cursor -= 1;
                    current.text.remove(current.cursor);
                }
            }
            Some("left") => current.cursor = current.cursor.saturating_sub(1),
            Some("right") => current.cursor = (current.cursor + 1).min(current.text.len()),
            _ => {
                let mut chars = data.chars();
                if let (Some(char), None) = (chars.next(), chars.next())
                    && char >= ' '
                    && char.len_utf16() == 1
                {
                    current.text.insert(current.cursor, char);
                    current.cursor += 1;
                }
            }
        }
    }

    fn render(&mut self, _width: usize) -> Vec<String> {
        let th = theme();
        let inner = WIDTH - 2;
        let pad = |text: String| {
            let fill = inner.saturating_sub(visible_width(&text));
            text + &" ".repeat(fill)
        };
        let row = |content: String| {
            format!(
                "{}{}{}",
                th.fg("border", "│"),
                pad(content),
                th.fg("border", "│")
            )
        };
        let mut lines = vec![th.fg("border", &format!("╭{}╮", "─".repeat(inner)))];
        lines.push(row(format!(" {}", th.fg("accent", "🧪 Overlay Test"))));
        lines.push(row(String::new()));
        lines.push(row(format!(
            " {}",
            th.fg("dim", "─── Edge Cases (borders should align) ───")
        )));
        lines.push(row(format!(
            " Wide: {}",
            th.fg(
                "warning",
                "中文日本語한글テスト漢字繁體简体ひらがなカタカナ가나다라마바"
            )
        )));
        lines.push(row(format!(
            " Styled: {} {} {} {} {} {} {}",
            th.fg("error", "RED"),
            th.fg("success", "GREEN"),
            th.fg("warning", "YELLOW"),
            th.fg("accent", "ACCENT"),
            th.fg("dim", "DIM"),
            th.fg("error", "more"),
            th.fg("success", "colors")
        )));
        lines.push(row(
            " Emoji: 👨‍👩‍👧‍👦 🇯🇵 🚀 💻 🎉 🔥 😀 🎯 🌟 💡 🎨 🔧 📦 🏆 🌈 🎪 🎭 🎬 🎮 🎲".to_owned(),
        ));
        lines.push(row(String::new()));
        lines.push(row(format!(" {}", th.fg("dim", "─── Actions ───"))));
        for (index, item) in self.items.iter().enumerate() {
            let selected = index == self.selected;
            let prefix = if selected { " ▶ " } else { "   " };
            let color = if selected { "accent" } else { "text" };
            let content = if item.has_input {
                let label = th.fg(color, &format!("{}:", item.label));
                let mut input: String = item.text.iter().collect();
                if selected {
                    let before: String = item.text[..item.cursor].iter().collect();
                    let under = item.text.get(item.cursor).copied().unwrap_or(' ');
                    let after: String = item.text.iter().skip(item.cursor + 1).collect();
                    // The terminal's cursor goes where the input's is.
                    input = format!("{before}{CURSOR_MARKER}\x1b[7m{under}\x1b[27m{after}");
                }
                format!("{prefix}{label} {input}")
            } else {
                format!("{prefix}{}", th.fg(color, item.label))
            };
            lines.push(row(content));
        }
        lines.push(row(String::new()));
        lines.push(row(format!(
            " {}",
            th.fg(
                "dim",
                "↑↓ navigate • type to input • Enter select • Esc cancel"
            )
        )));
        lines.push(th.fg("border", &format!("╰{}╯", "─".repeat(inner))));
        lines
    }
}

fn init(api: &mut Api) {
    api.register_command(
        "overlay-test",
        "Test overlay rendering with edge cases",
        |_args, ctx| async move {
            let item = |label, has_input| Item {
                label,
                has_input,
                text: Vec::new(),
                cursor: 0,
            };
            let items = vec![
                item("Search", true),
                item("Run", true),
                item("Settings", false),
                item("Cancel", false),
            ];
            let options = CustomOptions {
                overlay: true,
                ..CustomOptions::default()
            };
            let shown = ctx.custom(
                |done| OverlayTest {
                    selected: 0,
                    items,
                    done,
                },
                options,
            );
            if let Some(Some((action, query))) = shown.await {
                let message = match query.filter(|query| !query.is_empty()) {
                    Some(query) => format!("{action}: \"{query}\""),
                    None => action,
                };
                notify(&message, "info");
            }
            Ok(())
        },
    );
}

yapi_extension_api::extension!(init);
