//! The startup header and the loaded-resources listing.
//!
//! Port of the header and listing in `interactive-mode.ts` in pi `v1.0.0`.

use std::path::Path;

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};
use yapi_core::agent_session::Resources;
use yapi_core::resources::Diagnostic;
use yapi_tui::color::{Color, ColorMode};
use yapi_tui::keybindings::Keybindings;
use yapi_tui::lines::{self, StyledLine};
use yapi_tui::theme::Theme;
use yapi_types::rpc::SourceInfo;

use super::keybindings::keys_text;
use super::selectors::key_hint as hint;

/// The logo's colors, as in `assets/logo.svg`: orange, red and tan.
const ORANGE: (f64, f64, f64) = (247.0, 76.0, 0.0);
const RED: (f64, f64, f64) = (206.0, 66.0, 43.0);
const TAN: (f64, f64, f64) = (222.0, 165.0, 132.0);

/// The "YaPi" logo in square pixels, `o` orange, `r` red, `t` tan.
const LOGO: [&str; 4] = [
    "o.o.....ooo.",
    "ooo.tt..r.o.",
    ".r.t.t..rr.t",
    ".r.tttt.r..t",
];

fn terminal((r, g, b): (f64, f64, f64), mode: ColorMode) -> ratatui_core::style::Color {
    Color::Rgb(r, g, b).to_terminal(mode)
}

fn brand(color: (f64, f64, f64), mode: ColorMode) -> Style {
    Style::new().fg(terminal(color, mode))
}

/// The logo as two rows of half blocks, two pixels per cell as in pi's
/// `piLogoLines`: a cell with two colors draws the top one over the bottom
/// one's background. The colors stay fixed across themes.
fn logo_rows(mode: ColorMode) -> [Vec<Span<'static>>; 2] {
    let color = |pixel: u8| match pixel {
        b'o' => Some(ORANGE),
        b'r' => Some(RED),
        b't' => Some(TAN),
        _ => None,
    };
    let row = |top: &str, bottom: &str| {
        top.bytes()
            .zip(bottom.bytes())
            .map(|(top, bottom)| match (color(top), color(bottom)) {
                (None, None) => Span::raw(" "),
                (Some(top), None) => Span::styled("▀", brand(top, mode)),
                (None, Some(bottom)) => Span::styled("▄", brand(bottom, mode)),
                (Some(top), Some(bottom)) if top == bottom => Span::styled("█", brand(top, mode)),
                (Some(top), Some(bottom)) => {
                    Span::styled("▀", brand(top, mode).bg(terminal(bottom, mode)))
                }
            })
            .collect()
    };
    [row(LOGO[0], LOGO[1]), row(LOGO[2], LOGO[3])]
}

/// The header's last line, pi's "Pi can explain its own features" tip: the
/// model reads yapi's and pi's docs, locally or online.
const TIP: &str =
    "yapi can explain its own features and look up its docs. Ask it how to use or extend yapi.";

/// The header rows: a blank row, the wordmark with the version, key hints, the
/// docs tip and a blank row. `expanded` lists every hint (`ctrl+o`).
pub fn render(
    theme: &Theme,
    keys: &Keybindings,
    expanded: bool,
    show_details: bool,
    width: usize,
) -> Vec<StyledLine> {
    let mode = theme.mode();
    let version = Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), theme.fg("dim"));
    // pi's layout: a two-row logo with the version beside its top row and the
    // hints beside its bottom row; Apple Terminal, which draws half blocks
    // with gaps, gets the wordmark above the hints instead.
    let logo = std::env::var("TERM_PROGRAM").ok().as_deref() != Some("Apple_Terminal");
    let [logo_top, logo_bottom] = logo_rows(mode);
    let mut content: Vec<StyledLine> = vec![if logo {
        let mut spans = logo_top;
        spans.extend([Span::raw(" "), version]);
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("Ya", brand(ORANGE, mode)),
            Span::styled("Pi", brand(TAN, mode)),
            Span::raw(" "),
            version,
        ])
    }];
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
        content.push(Line::from(
            parts.join(&Span::styled(" · ", theme.fg("muted"))),
        ));
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
    // pi's onboarding line, under both forms of the header.
    content.push(Line::default());
    content.push(lines::styled(TIP, theme.fg("dim")));
    if logo && let Some(first) = content.get_mut(1) {
        let mut spans = logo_bottom;
        spans.push(Span::raw(" "));
        spans.append(&mut first.spans);
        *first = Line::from(spans);
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

/// pi's `formatDisplayPath`: the home directory as `~`.
fn display_path(path: &str, home: Option<&Path>) -> String {
    match home.and_then(|home| Path::new(path).strip_prefix(home).ok()) {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.to_owned(),
    }
}

fn is_package(source: &SourceInfo) -> bool {
    source.source.starts_with("npm:") || source.source.starts_with("git:")
}

/// pi's `getShortPath`: a package file relative to its package.
fn short_path(source: &SourceInfo, home: Option<&Path>) -> String {
    let full = source.path.replace('\\', "/");
    if is_package(source)
        && let Some(base) = &source.base_dir
        && let Ok(relative) = Path::new(&full).strip_prefix(base)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().replace('\\', "/");
    }
    // pi's patterns: `node_modules/(@?[^/]+(?:/[^/]+)?)/(.*)` and
    // `git/[^/]+/[^/]+/(.*)`.
    let rest_after = |marker: &str| full.find(marker).map(|start| &full[start + marker.len()..]);
    if source.source.starts_with("npm:")
        && let Some(rest) = rest_after("node_modules/")
    {
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.len() {
            0 | 1 => {}
            2 => return parts[1].to_owned(),
            _ => return parts[2..].join("/"),
        }
    }
    if source.source.starts_with("git:")
        && let Some(rest) = rest_after("git/")
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() > 3 || (parts.len() == 3 && !parts[2].is_empty()) {
            return parts[2..].join("/");
        }
    }
    display_path(&source.path, home)
}

/// pi's compact `[Extensions]` labels: the shortest unique path suffix of a
/// file, without `index.ts`; for packages, the package and the file in it.
fn extension_labels(extensions: &[SourceInfo], home: Option<&Path>) -> Vec<String> {
    let segments = |path: &str| -> Vec<String> {
        display_path(path, home)
            .replace('\\', "/")
            .split('/')
            .filter(|segment| !segment.is_empty() && *segment != "~")
            .map(str::to_owned)
            .collect()
    };
    let files: Vec<(&str, Vec<String>)> = extensions
        .iter()
        .filter(|source| !is_package(source))
        .map(|source| {
            let mut parts = segments(&source.path);
            if parts.len() > 1
                && matches!(
                    parts.last().map(String::as_str),
                    Some("index.ts" | "index.js")
                )
            {
                parts.pop();
            }
            (source.path.as_str(), parts)
        })
        .collect();
    extensions
        .iter()
        .map(|source| {
            if is_package(source) {
                let label = match source.source.strip_prefix("npm:") {
                    Some(name) if !name.is_empty() => name.to_owned(),
                    Some(_) => source.source.clone(),
                    None => match yapi_core::packages::source::parse(&source.source) {
                        yapi_core::packages::source::Source::Git { path, .. }
                            if !path.is_empty() =>
                        {
                            path
                        }
                        _ => source.source.clone(),
                    },
                };
                let short = short_path(source, home);
                let inner = short.strip_prefix("extensions/").unwrap_or(&short);
                let path = Path::new(inner);
                return if path.file_stem().is_some_and(|stem| stem == "index") {
                    match path.parent().map(|dir| dir.to_string_lossy().into_owned()) {
                        Some(dir) if !dir.is_empty() && dir != "." => format!("{label}:{dir}"),
                        _ => label,
                    }
                } else {
                    format!("{label}:{inner}")
                };
            }
            let Some(index) = files.iter().position(|(path, _)| *path == source.path) else {
                return source.path.clone();
            };
            let parts = &files[index].1;
            for count in 1..=parts.len() {
                let candidate = parts[parts.len() - count..].join("/");
                let unique = files.iter().enumerate().all(|(other, (_, segments))| {
                    other == index
                        || segments[segments.len().saturating_sub(count)..].join("/") != candidate
                });
                if unique {
                    return candidate;
                }
            }
            parts.join("/")
        })
        .collect()
}

/// One resource in pi's expanded listing: its source, and how it shows
/// outside a package and inside one.
struct Listed<'a> {
    source: &'a SourceInfo,
    label: String,
    package_label: String,
}

/// pi's `buildScopeGroups` and `formatScopeGroups`: resources by scope
/// (project, user, path), loose files by path first, then each package's.
fn scope_groups(items: &[Listed<'_>], theme: &Theme) -> Vec<StyledLine> {
    let group_of = |source: &SourceInfo| match (source.source.as_str(), source.scope.as_str()) {
        ("cli", _) | (_, "temporary") => "path",
        (_, "user") => "user",
        (_, "project") => "project",
        _ => "path",
    };
    let compare = |a: &str, b: &str| yapi_types::collate::locale_compare(a, b);
    let mut out = Vec::new();
    for group in ["project", "user", "path"] {
        let members: Vec<&Listed<'_>> = items
            .iter()
            .filter(|item| group_of(item.source) == group)
            .collect();
        if members.is_empty() {
            continue;
        }
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(group, theme.fg("accent")),
        ]));
        let mut paths: Vec<&Listed<'_>> = members
            .iter()
            .copied()
            .filter(|item| !is_package(item.source))
            .collect();
        paths.sort_by(|a, b| compare(&a.source.path, &b.source.path));
        for item in paths {
            out.push(lines::styled(
                format!("    {}", item.label),
                theme.fg("dim"),
            ));
        }
        let mut packages: Vec<&str> = members
            .iter()
            .filter(|item| is_package(item.source))
            .map(|item| item.source.source.as_str())
            .collect();
        packages.sort_by(|a, b| compare(a, b));
        packages.dedup();
        for package in packages {
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(package.to_owned(), theme.fg("mdLink")),
            ]));
            let mut files: Vec<&Listed<'_>> = members
                .iter()
                .copied()
                .filter(|item| item.source.source == package)
                .collect();
            files.sort_by(|a, b| compare(&a.source.path, &b.source.path));
            for item in files {
                out.push(lines::styled(
                    format!("      {}", item.package_label),
                    theme.fg("dim"),
                ));
            }
        }
    }
    out
}

/// pi's expanded `[Extensions]` body: files and packages by scope.
fn extension_groups(
    extensions: &[SourceInfo],
    home: Option<&Path>,
    theme: &Theme,
) -> Vec<StyledLine> {
    let without_index = |path: String| {
        path.strip_suffix("/index.ts")
            .or_else(|| path.strip_suffix("/index.js"))
            .map_or_else(|| path.clone(), str::to_owned)
    };
    let items: Vec<Listed<'_>> = extensions
        .iter()
        .map(|source| Listed {
            source,
            label: without_index(display_path(&source.path, home)),
            package_label: without_index(short_path(source, home)),
        })
        .collect();
    scope_groups(&items, theme)
}

/// The `[Context]`, `[Skills]`, `[Prompts]` and `[Extensions]` sections.
pub fn listing(
    theme: &Theme,
    resources: &Resources,
    extensions: &[SourceInfo],
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
    // Expanded, skills and prompts are grouped by scope as extensions are.
    let grouped = |out: &mut Vec<StyledLine>, name: &str, items: Vec<Listed<'_>>| {
        if items.is_empty() {
            return;
        }
        let mut content = vec![heading(name)];
        content.extend(scope_groups(&items, theme));
        out.extend(lines::text(&content, width, 0, 0, None));
        out.extend(lines::spacer(1));
    };
    if expanded {
        let skills = resources
            .skills
            .iter()
            .map(|skill| Listed {
                source: &skill.source,
                label: display_path(&skill.source.path, home),
                package_label: short_path(&skill.source, home),
            })
            .collect();
        grouped(&mut out, "Skills", skills);
        let prompts = resources
            .templates
            .iter()
            .map(|template| Listed {
                source: &template.source,
                label: format!("/{}", template.name),
                package_label: format!("/{}", template.name),
            })
            .collect();
        grouped(&mut out, "Prompts", prompts);
    } else {
        let mut skills: Vec<String> = resources
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        skills.sort_by(|a, b| yapi_types::collate::locale_compare(a, b));
        section(&mut out, "Skills", skills);
        let mut prompts: Vec<String> = resources
            .templates
            .iter()
            .map(|template| format!("/{}", template.name))
            .collect();
        prompts.sort_by(|a, b| yapi_types::collate::locale_compare(a, b));
        section(&mut out, "Prompts", prompts);
    }
    if !extensions.is_empty() {
        let mut content = vec![heading("Extensions")];
        if expanded {
            content.extend(extension_groups(extensions, home, theme));
        } else {
            let mut labels = extension_labels(extensions, home);
            labels.sort_by(|a, b| yapi_types::collate::locale_compare(a, b));
            content.push(lines::styled(format!("  {}", labels.join(", ")), dim));
        }
        out.extend(lines::text(&content, width, 0, 0, None));
        out.extend(lines::spacer(1));
    }
    out
}

/// pi's `formatPathWithSource`: the source's label and scope, then its short
/// path.
fn path_with_source(source: &SourceInfo, home: Option<&Path>) -> String {
    let scope = match source.scope.as_str() {
        "user" => Some("user"),
        "project" => Some("project"),
        "temporary" => Some("temp"),
        _ => None,
    };
    let (label, scope) = match source.source.as_str() {
        "local" => match source.scope.as_str() {
            "user" => ("user", None),
            "project" => ("project", None),
            "temporary" => ("path", Some("temp")),
            _ => ("path", None),
        },
        "cli" => ("path", scope.filter(|scope| *scope == "temp")),
        other => (other, scope),
    };
    let label = match scope {
        Some(scope) => format!("{label} ({scope})"),
        None => label.to_owned(),
    };
    format!("{label} {}", short_path(source, home))
}

/// pi's `[Extension issues]` section: the warnings about extensions that
/// were not loaded under their paths, then each issue under the extension it
/// concerns. Empty without either.
pub fn extension_issues(
    theme: &Theme,
    warnings: &[(String, String)],
    issues: &[(SourceInfo, String)],
    home: Option<&Path>,
    width: usize,
) -> Vec<StyledLine> {
    if warnings.is_empty() && issues.is_empty() {
        return Vec::new();
    }
    let warning = theme.fg("warning");
    let mut content = vec![lines::styled("[Extension issues]", warning)];
    for (path, message) in warnings {
        content.push(lines::styled(
            format!("  {}", display_path(path, home)),
            warning,
        ));
        content.push(lines::styled(format!("    {message}"), warning));
    }
    for (source, message) in issues {
        content.push(lines::styled(
            format!("  {}", path_with_source(source, home)),
            warning,
        ));
        content.push(lines::styled(format!("    {message}"), warning));
    }
    let mut out = lines::text(&content, width, 0, 0, None);
    out.extend(lines::spacer(1));
    out
}

/// pi's `findSourceInfoForPath`: the source of the loaded resource at `path`
/// or, failing that, at its nearest ancestor, shown for `path`.
fn source_for(path: &Path, loaded: &[&SourceInfo]) -> Option<SourceInfo> {
    let text = path.to_string_lossy();
    let mut current: &str = &text;
    loop {
        if let Some(source) = loaded.iter().find(|source| source.path == current) {
            return Some(SourceInfo {
                path: text.into_owned(),
                ..(*source).clone()
            });
        }
        current = &current[..current.rfind('/')?];
    }
}

/// pi's `formatDiagnostics` under `title`, such as `[Skill conflicts]`:
/// names taken twice, grouped by name, then the other problems in order.
/// Paths show the source of the loaded resource they belong to, as in pi.
/// Empty without diagnostics.
pub fn conflicts(
    title: &str,
    theme: &Theme,
    diagnostics: &[Diagnostic],
    loaded: &[&SourceInfo],
    home: Option<&Path>,
    width: usize,
) -> Vec<StyledLine> {
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let display = |path: &Path| match source_for(path, loaded) {
        Some(source) => path_with_source(&source, home),
        None => display_path(&path.to_string_lossy(), home),
    };
    let warning = theme.fg("warning");
    let dim = theme.fg("dim");
    let mut content = vec![lines::styled(title, warning)];
    let mut names: Vec<&str> = Vec::new();
    for diagnostic in diagnostics {
        if let Diagnostic::Collision { name, .. } = diagnostic
            && !names.contains(&name.as_str())
        {
            names.push(name);
        }
    }
    for name in names {
        content.push(lines::styled(format!("  \"{name}\" collision:"), warning));
        let mut winner_shown = false;
        for diagnostic in diagnostics {
            let Diagnostic::Collision {
                name: declared,
                winner,
                loser,
            } = diagnostic
            else {
                continue;
            };
            if declared != name {
                continue;
            }
            if !winner_shown {
                winner_shown = true;
                content.push(Line::from(vec![
                    Span::styled("    ", dim),
                    Span::styled("✓", theme.fg("success")),
                    // pi nests the mark's color in the dim line, whose own
                    // color the mark's reset ends.
                    Span::raw(format!(" {}", path_with_source(winner, home))),
                ]));
            }
            content.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled("✗", warning),
                Span::raw(format!(" {} (skipped)", display(loser))),
            ]));
        }
    }
    for diagnostic in diagnostics {
        let (message, path, style) = match diagnostic {
            Diagnostic::Warning { message, path } => (message, path, warning),
            Diagnostic::Error { message, path } => (message, path, theme.fg("error")),
            Diagnostic::Collision { .. } => continue,
        };
        content.push(lines::styled(format!("  {}", display(path)), style));
        // Only the first line of a message is indented, as in pi's text.
        for (index, line) in message.split('\n').enumerate() {
            let indent = if index == 0 { "    " } else { "" };
            content.push(lines::styled(format!("{indent}{line}"), style));
        }
    }
    let mut out = lines::text(&content, width, 0, 0, None);
    out.extend(lines::spacer(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_draws_yapi_in_two_rows_of_half_blocks() {
        let text = |row: &[Span<'_>]| {
            row.iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let [top, bottom] = logo_rows(ColorMode::TrueColor);
        assert_eq!(text(&top), "█▄█ ▄▄  ▀▀█ ");
        assert_eq!(text(&bottom), " █ █▄█▄ █▀ █");
        // The P's corner holds two colors: orange over red.
        assert_eq!(
            top[8].style,
            brand(ORANGE, ColorMode::TrueColor).bg(terminal(RED, ColorMode::TrueColor))
        );
    }

    #[test]
    fn ends_with_the_docs_tip() {
        let theme = Theme::builtin("dark", ColorMode::TrueColor).unwrap();
        let keys = Keybindings::new(
            yapi_tui::keys::Keys::default(),
            super::super::keybindings::definitions(),
            &yapi_tui::keybindings::UserBindings::new(),
        );
        for expanded in [false, true] {
            let rows: Vec<String> = render(&theme, &keys, expanded, false, 120)
                .iter()
                .map(|row| lines::plain(row).trim_end().to_owned())
                .collect();
            let end = &rows[rows.len() - 3..];
            assert_eq!(end, ["", &format!(" {TIP}"), ""], "{rows:#?}");
        }
    }
}
