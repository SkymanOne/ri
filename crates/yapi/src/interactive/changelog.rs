//! yapi's changelog, read as pi's `utils/changelog.ts` reads pi's: for
//! `/changelog`, and for the notice of what changed since the version that
//! last started.

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use yapi_core::agent_session::AgentSession;
use yapi_tui::lines::{self, styled};
use yapi_tui::markdown::{self, MarkdownOptions};

use super::chat::Item;

/// The repository's CHANGELOG.md.
const CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");

type Version = (u64, u64, u64);

/// A `## [x.y.z]` section: its version, and its markdown from the heading on.
struct Entry {
    version: Version,
    content: String,
}

/// The sections of `text` whose heading names a version, in file order.
/// Others, such as `## [Unreleased]`, are skipped.
fn parse(text: &str) -> Vec<Entry> {
    let entry = |(version, lines): (Version, Vec<&str>)| Entry {
        version,
        content: lines.join("\n").trim().to_owned(),
    };
    let mut entries = Vec::new();
    let mut current: Option<(Version, Vec<&str>)> = None;
    for line in text.split('\n') {
        if line.starts_with("## ") {
            entries.extend(current.take().map(entry));
            current = heading_version(line).map(|version| (version, vec![line]));
        } else if let Some((_, lines)) = &mut current {
            lines.push(line);
        }
    }
    entries.extend(current.map(entry));
    entries
}

/// The version a heading starts with, as in `## 1.2.3` or `## [1.2.3] - date`.
fn heading_version(line: &str) -> Option<Version> {
    let rest = line.strip_prefix("##")?.trim_start();
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    let (major, rest) = number(rest)?;
    let (minor, rest) = number(rest.strip_prefix('.')?)?;
    let (patch, _) = number(rest.strip_prefix('.')?)?;
    Some((major, minor, patch))
}

/// The number `text` starts with, and the rest.
fn number(text: &str) -> Option<(u64, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    Some((text[..end].parse().ok()?, &text[end..]))
}

/// The entries newer than `last`, a version as `lastChangelogVersion` holds
/// it. Parts that are missing or not numbers count as 0.
fn newer(entries: Vec<Entry>, last: &str) -> Vec<Entry> {
    let mut parts = last.split('.').map(|part| part.trim().parse().unwrap_or(0));
    let mut part = || parts.next().unwrap_or(0);
    let last = (part(), part(), part());
    entries
        .into_iter()
        .filter(|entry| entry.version > last)
        .collect()
}

fn join(entries: impl Iterator<Item = Entry>) -> String {
    entries
        .map(|entry| entry.content)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `/changelog`'s markdown: every entry, the newest last.
pub fn all() -> String {
    let entries = parse(CHANGELOG);
    if entries.is_empty() {
        return "No changelog entries found.".into();
    }
    join(entries.into_iter().rev())
}

/// pi's `getChangelogForDisplay`: the markdown of the entries newer than the
/// `lastChangelogVersion` setting, which then records this version. The first
/// start only records it, and a resumed session shows nothing.
pub fn since_last_start(session: &AgentSession) -> Option<String> {
    if !session.messages().is_empty() {
        return None;
    }
    let record = || {
        let version = env!("CARGO_PKG_VERSION");
        let _ = session.set_global_setting("lastChangelogVersion", Some(version.into()));
    };
    let Some(last) = session.settings().last_changelog_version else {
        record();
        return None;
    };
    let entries = newer(parse(CHANGELOG), &last);
    if entries.is_empty() {
        return None;
    }
    record();
    Some(join(entries.into_iter()))
}

/// pi's startup notice of the entries in `markdown`, after a spacer when
/// `spacer`: in full, or in one line when `collapse`.
pub fn notice(markdown: String, collapse: bool, spacer: bool) -> Item {
    Item::Render(Box::new(move |width, ctx| {
        let border = lines::border(width, ctx.theme.fg("border"));
        let mut out = if spacer { lines::spacer(1) } else { Vec::new() };
        out.push(border.clone());
        if collapse {
            let latest = markdown
                .lines()
                .find_map(heading_version)
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
            let line = Line::from(vec![
                Span::raw(format!("Updated to v{latest}. Use ")),
                Span::styled("/changelog", Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(" to view full changelog."),
            ]);
            out.extend(lines::text(&[line], width, 1, 0, None));
        } else {
            out.extend(lines::text_row(
                styled(
                    "What's New",
                    ctx.theme.fg("accent").add_modifier(Modifier::BOLD),
                ),
                width,
                1,
            ));
            out.extend(lines::spacer(1));
            out.extend(markdown::render(
                markdown.trim(),
                width,
                1,
                0,
                ctx.markdown,
                MarkdownOptions {
                    hyperlinks: ctx.hyperlinks,
                    ..MarkdownOptions::default()
                },
            ));
            out.extend(lines::spacer(1));
        }
        out.push(border);
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(entries: &[Entry]) -> Vec<Version> {
        entries.iter().map(|entry| entry.version).collect()
    }

    #[test]
    fn the_newest_entry_is_this_version() {
        let entries = parse(CHANGELOG);
        let (major, minor, patch) = entries[0].version;
        assert_eq!(
            format!("{major}.{minor}.{patch}"),
            env!("CARGO_PKG_VERSION")
        );
        assert!(entries[0].content.starts_with("## ["));
    }

    #[test]
    fn sections_without_a_version_are_skipped() {
        let text = "# Changelog\n\nIntro.\n\n## [Unreleased]\n\n- Next.\n\n## [1.2.0] - 2026-01-02\n\n### Added\n\n- New.\n\n## 1.1.10\n\n- Old.\n\n[1.2.0]: https://example.com\n";
        let entries = parse(text);
        assert_eq!(versions(&entries), [(1, 2, 0), (1, 1, 10)]);
        assert_eq!(
            entries[0].content,
            "## [1.2.0] - 2026-01-02\n\n### Added\n\n- New."
        );
        assert_eq!(
            entries[1].content,
            "## 1.1.10\n\n- Old.\n\n[1.2.0]: https://example.com"
        );
        assert!(parse("## Notes\n\n- None.").is_empty());
    }

    #[test]
    fn newer_entries_follow_the_recorded_version() {
        let entries = || parse("## [2.0.0]\n\n## [1.10.0]\n\n## [1.9.3]\n\n## [1.9.2]\n");
        assert_eq!(
            versions(&newer(entries(), "1.9.2")),
            [(2, 0, 0), (1, 10, 0), (1, 9, 3)]
        );
        assert_eq!(versions(&newer(entries(), "1.10.0")), [(2, 0, 0)]);
        assert!(newer(entries(), "2.0.0").is_empty());
        // Missing or unreadable parts count as 0, as pi's `Number(part) || 0`.
        assert!(newer(entries(), "2").is_empty());
        assert_eq!(versions(&newer(entries(), "1.x")).len(), 4);
    }
}
