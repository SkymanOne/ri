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
use ri_types::rpc::SourceInfo;

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
    let coral = brand(228.0, 138.0, 122.0, mode);
    let yellow = brand(234.0, 182.0, 93.0, mode);
    let version = Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), theme.fg("dim"));
    // pi's layout: a two-row, four-cell logo with the version beside its top
    // row and the hints beside its bottom row; Apple Terminal, which draws
    // half blocks with gaps, gets the wordmark above the hints instead.
    let logo = std::env::var("TERM_PROGRAM").ok().as_deref() != Some("Apple_Terminal");
    let mut content: Vec<StyledLine> = vec![if logo {
        Line::from(vec![
            Span::styled("█▀", coral),
            Span::raw(" "),
            Span::styled("▀", yellow),
            Span::raw(" "),
            version,
        ])
    } else {
        Line::from(vec![
            Span::styled("r", coral),
            Span::styled("i", yellow),
            Span::raw(" "),
            version,
        ])
    }];
    let logo_bottom = || {
        vec![
            Span::styled("█", coral),
            Span::raw("  "),
            Span::styled("█", yellow),
            Span::raw(" "),
        ]
    };
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
    if logo && let Some(first) = content.get_mut(1) {
        let mut spans = logo_bottom();
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
                    None => match ri_core::packages::source::parse(&source.source) {
                        ri_core::packages::source::Source::Git { path, .. } if !path.is_empty() => {
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

/// pi's expanded `[Extensions]` body: files and packages by scope.
fn extension_groups(
    extensions: &[SourceInfo],
    home: Option<&Path>,
    theme: &Theme,
) -> Vec<StyledLine> {
    let group_of = |source: &SourceInfo| match (source.source.as_str(), source.scope.as_str()) {
        ("cli", _) | (_, "temporary") => "path",
        (_, "user") => "user",
        (_, "project") => "project",
        _ => "path",
    };
    let display = |source: &SourceInfo| {
        let path = display_path(&source.path, home);
        path.strip_suffix("/index.ts")
            .or_else(|| path.strip_suffix("/index.js"))
            .unwrap_or(&path)
            .to_owned()
    };
    let compare = |a: &str, b: &str| ri_types::collate::locale_compare(a, b);
    let mut out = Vec::new();
    for group in ["project", "user", "path"] {
        let members: Vec<&SourceInfo> = extensions
            .iter()
            .filter(|source| group_of(source) == group)
            .collect();
        if members.is_empty() {
            continue;
        }
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(group, theme.fg("accent")),
        ]));
        let mut paths: Vec<&SourceInfo> =
            members.iter().copied().filter(|s| !is_package(s)).collect();
        paths.sort_by(|a, b| compare(&a.path, &b.path));
        for source in paths {
            out.push(lines::styled(
                format!("    {}", display(source)),
                theme.fg("dim"),
            ));
        }
        let mut packages: Vec<&str> = members
            .iter()
            .filter(|source| is_package(source))
            .map(|source| source.source.as_str())
            .collect();
        packages.sort_by(|a, b| compare(a, b));
        packages.dedup();
        for package in packages {
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(package.to_owned(), theme.fg("mdLink")),
            ]));
            let mut files: Vec<&SourceInfo> = members
                .iter()
                .copied()
                .filter(|source| source.source == package)
                .collect();
            files.sort_by(|a, b| compare(&a.path, &b.path));
            for source in files {
                let short = short_path(source, home);
                let short = short
                    .strip_suffix("/index.ts")
                    .or_else(|| short.strip_suffix("/index.js"))
                    .unwrap_or(&short);
                out.push(lines::styled(format!("      {short}"), theme.fg("dim")));
            }
        }
    }
    out
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
    let mut skills: Vec<String> = resources
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    skills.sort_by(|a, b| ri_types::collate::locale_compare(a, b));
    section(&mut out, "Skills", skills);
    let mut prompts: Vec<String> = resources
        .templates
        .iter()
        .map(|template| format!("/{}", template.name))
        .collect();
    prompts.sort_by(|a, b| ri_types::collate::locale_compare(a, b));
    section(&mut out, "Prompts", prompts);
    if !extensions.is_empty() {
        let mut content = vec![heading("Extensions")];
        if expanded {
            content.extend(extension_groups(extensions, home, theme));
        } else {
            let mut labels = extension_labels(extensions, home);
            labels.sort_by(|a, b| ri_types::collate::locale_compare(a, b));
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

/// pi's `[Extension issues]` section: each issue under the extension it
/// concerns. Empty without issues.
pub fn extension_issues(
    theme: &Theme,
    issues: &[(SourceInfo, String)],
    home: Option<&Path>,
    width: usize,
) -> Vec<StyledLine> {
    if issues.is_empty() {
        return Vec::new();
    }
    let warning = theme.fg("warning");
    let mut content = vec![lines::styled("[Extension issues]", warning)];
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

/// pi's `[Theme conflicts]` section: names declared twice, grouped by name,
/// then paths that failed to load. Empty without diagnostics.
pub fn theme_conflicts(
    theme: &Theme,
    diagnostics: &[super::themes::Diagnostic],
    home: Option<&Path>,
    width: usize,
) -> Vec<StyledLine> {
    use super::themes::Diagnostic;
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let display = |path: &Path| display_path(&path.to_string_lossy(), home);
    let warning = theme.fg("warning");
    let dim = theme.fg("dim");
    let mut content = vec![lines::styled("[Theme conflicts]", warning)];
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
                    Span::styled(format!(" {}", path_with_source(winner, home)), dim),
                ]));
            }
            content.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled("✗", warning),
                Span::styled(format!(" {} (skipped)", display(loser)), dim),
            ]));
        }
    }
    for diagnostic in diagnostics {
        if let Diagnostic::Warning { message, path } = diagnostic {
            content.push(lines::styled(format!("  {}", display(path)), warning));
            // Only the first line of a message is indented, as in pi's text.
            for (index, line) in message.split('\n').enumerate() {
                let indent = if index == 0 { "    " } else { "" };
                content.push(lines::styled(format!("{indent}{line}"), warning));
            }
        }
    }
    let mut out = lines::text(&content, width, 0, 0, None);
    out.extend(lines::spacer(1));
    out
}
