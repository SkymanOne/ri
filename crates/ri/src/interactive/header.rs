//! The startup header and the loaded-resources listing.
//!
//! Port of the header and listing in `interactive-mode.ts` in pi `v1.0.0`.

use std::path::Path;

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};
use ri_core::agent_session::Resources;
use ri_tui::color::{Color, ColorMode};
use ri_tui::keybindings::Keybindings;
use ri_tui::lines::{self, StyledLine};
use ri_tui::theme::Theme;

use super::keybindings::keys_text;

fn brand(r: f64, g: f64, b: f64, mode: ColorMode) -> Style {
    Style::new().fg(Color::Rgb(r, g, b).to_terminal(mode))
}

fn hint(theme: &Theme, key: &str, text: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_owned(), theme.fg("dim")),
        Span::styled(format!(" {text}"), theme.fg("muted")),
    ]
}

/// The header rows: a blank row, the wordmark with the version, key hints and
/// a blank row. `expanded` lists every hint (`ctrl+o`).
pub fn render(
    theme: &Theme,
    keys: &Keybindings,
    expanded: bool,
    show_details: bool,
    width: usize,
) -> Vec<StyledLine> {
    let mode = theme.mode();
    let mut content: Vec<StyledLine> = vec![Line::from(vec![
        Span::styled("r", brand(228.0, 138.0, 122.0, mode)),
        Span::styled("i", brand(234.0, 182.0, 93.0, mode)),
        Span::raw(" "),
        Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), theme.fg("dim")),
    ])];
    let key = |action: &str| keys_text(keys, action);
    if expanded {
        let hints: Vec<(String, &str)> = vec![
            (key("app.interrupt"), "to interrupt"),
            (key("app.clear"), "to clear"),
            (format!("{} twice", key("app.clear")), "to exit"),
            (key("app.exit"), "to exit (empty)"),
            (key("app.suspend"), "to suspend"),
            (key("tui.editor.deleteToLineEnd"), "to delete to end"),
            (key("app.thinking.cycle"), "to cycle thinking level"),
            (
                format!(
                    "{}/{}",
                    key("app.model.cycleForward"),
                    key("app.model.cycleBackward")
                ),
                "to cycle models",
            ),
            (key("app.model.select"), "to select model"),
            (key("app.tools.expand"), "to expand tools"),
            (key("app.thinking.toggle"), "to expand thinking"),
            (key("app.editor.external"), "for external editor"),
            ("/".to_owned(), "for commands"),
            ("!".to_owned(), "to run bash"),
            ("!!".to_owned(), "to run bash (no context)"),
            (key("app.message.followUp"), "to queue follow-up"),
            (key("app.message.dequeue"), "to edit all queued messages"),
            (
                key("app.clipboard.pasteImage"),
                "to paste files on macOS, images, or text",
            ),
            ("drop files".to_owned(), "to attach"),
        ];
        for (key, text) in hints {
            content.push(Line::from(hint(theme, &key, text)));
        }
    } else {
        let parts = [
            hint(theme, &key("app.interrupt"), "interrupt"),
            hint(
                theme,
                &format!("{}/{}", key("app.clear"), key("app.exit")),
                "clear/exit",
            ),
            hint(theme, "/", "commands"),
            hint(theme, "!", "bash"),
            hint(theme, &key("app.tools.expand"), "more"),
        ];
        let mut spans = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", theme.fg("muted")));
            }
            spans.extend(part);
        }
        content.push(Line::from(spans));
        content.push(lines::styled(
            format!(
                "Press {} to show full startup help{}.",
                key("app.tools.expand"),
                if show_details {
                    " and loaded resources"
                } else {
                    ""
                }
            ),
            theme.fg("dim"),
        ));
    }
    let mut out = lines::spacer(1);
    out.extend(lines::text(&content, width, 1, 0, None));
    out.extend(lines::spacer(1));
    out
}

fn relative_label(path: &Path, cwd: &Path, home: Option<&Path>) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return relative.display().to_string();
    }
    if let Some(home) = home
        && let Ok(relative) = path.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

/// The `[Context]`, `[Skills]` and `[Prompts]` sections.
pub fn listing(
    theme: &Theme,
    resources: &Resources,
    cwd: &Path,
    home: Option<&Path>,
    expanded: bool,
    width: usize,
) -> Vec<StyledLine> {
    let heading = |name: &str| lines::styled(format!("[{name}]"), theme.fg("mdHeading"));
    let dim = theme.fg("dim");
    let mut out = Vec::new();
    let section = |out: &mut Vec<StyledLine>, name: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        let mut content = vec![heading(name)];
        if expanded {
            content.extend(
                items
                    .iter()
                    .map(|item| lines::styled(format!("  {item}"), dim)),
            );
        } else {
            content.push(lines::styled(format!("  {}", items.join(", ")), dim));
        }
        out.extend(lines::text(&content, width, 0, 0, None));
        out.extend(lines::spacer(1));
    };
    let context: Vec<String> = resources
        .context_files
        .iter()
        .map(|file| {
            if expanded {
                match home.and_then(|home| file.path.strip_prefix(home).ok()) {
                    Some(relative) => format!("~/{}", relative.display()),
                    None => file.path.display().to_string(),
                }
            } else {
                relative_label(&file.path, cwd, home)
            }
        })
        .collect();
    if !context.is_empty() {
        out.extend(lines::spacer(1));
    }
    section(&mut out, "Context", context);
    let mut skills: Vec<String> = resources
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    skills.sort_by(|a, b| ri_core::collate::locale_compare(a, b));
    section(&mut out, "Skills", skills);
    let mut prompts: Vec<String> = resources
        .templates
        .iter()
        .map(|template| format!("/{}", template.name))
        .collect();
    prompts.sort_by(|a, b| ri_core::collate::locale_compare(a, b));
    section(&mut out, "Prompts", prompts);
    out
}
