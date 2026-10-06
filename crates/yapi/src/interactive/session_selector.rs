//! The `/resume` session selector.
//!
//! Port of `components/session-selector.ts` and `session-selector-search.ts`
//! in `packages/coding-agent/src/modes/interactive` in pi `v1.0.0`. Sessions
//! load synchronously when a scope is first shown.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use yapi_core::session::{SessionManager, SessionSummary};
use yapi_tui::fuzzy::fuzzy_match;
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::visible_range;
use yapi_tui::text::{truncate_to_width, visible_width};
use yapi_tui::text_input::{InputEvent, TextInput};

use super::selectors::{Action, Outcome, Ui};

const MAX_VISIBLE: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Current,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    Threaded,
    Recent,
    Relevance,
}

/// Where sessions come from.
pub struct Sources {
    /// The current project's session directory.
    pub dir: PathBuf,
    /// Only sessions from this directory count as the current project's.
    pub cwd_filter: Option<PathBuf>,
    /// A custom session directory shared by every project, if any.
    pub custom: Option<PathBuf>,
    /// The root holding every project's session directory.
    pub root: PathBuf,
}

impl Sources {
    fn load(&self, scope: Scope) -> Vec<SessionSummary> {
        match scope {
            Scope::Current => yapi_core::session::list(&self.dir, self.cwd_filter.as_deref()),
            Scope::All => match &self.custom {
                Some(custom) => yapi_core::session::list(custom, None),
                None => yapi_core::session::list_all(&self.root),
            },
        }
    }
}

struct Row {
    session: SessionSummary,
    depth: usize,
    last: bool,
    continues: Vec<bool>,
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// pi's `formatSessionDate`.
/// The session pi loads first: in one folder, the file named last; across
/// folders, the file modified last.
fn first_loaded(sessions: &[SessionSummary], scope: Scope) -> Option<&SessionSummary> {
    let name = |session: &SessionSummary| session.path.file_name().map(ToOwned::to_owned);
    match scope {
        Scope::Current => sessions.iter().max_by_key(|session| name(session)),
        Scope::All => sessions.iter().max_by_key(|session| {
            let modified = std::fs::metadata(&session.path)
                .and_then(|metadata| metadata.modified())
                .ok();
            (modified, name(session))
        }),
    }
}

fn age(modified_ms: u64, now_ms: u64) -> String {
    let diff = now_ms.saturating_sub(modified_ms);
    let minutes = diff / 60_000;
    let hours = diff / 3_600_000;
    let days = diff / 86_400_000;
    if minutes < 1 {
        "now".into()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

enum Token {
    Fuzzy(String),
    Phrase(String),
}

enum Query {
    Tokens(Vec<Token>),
    Regex(regex_lite::Regex),
    Invalid,
}

fn normalize_lower(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// pi's `parseSearchQuery`.
fn parse_query(query: &str) -> Query {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Query::Tokens(Vec::new());
    }
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Query::Invalid;
        }
        return match regex_lite::Regex::new(&format!("(?i){pattern}")) {
            Ok(regex) => Query::Regex(regex),
            Err(_) => Query::Invalid,
        };
    }
    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut quoted = false;
    let flush = |buffer: &mut String, tokens: &mut Vec<Token>, phrase: bool| {
        let value = buffer.trim().to_owned();
        buffer.clear();
        if !value.is_empty() {
            tokens.push(if phrase {
                Token::Phrase(value)
            } else {
                Token::Fuzzy(value)
            });
        }
    };
    for c in trimmed.chars() {
        if c == '"' {
            flush(&mut buffer, &mut tokens, quoted);
            quoted = !quoted;
            continue;
        }
        if !quoted && c.is_whitespace() {
            flush(&mut buffer, &mut tokens, false);
            continue;
        }
        buffer.push(c);
    }
    if quoted {
        return Query::Tokens(
            trimmed
                .split_whitespace()
                .map(|token| Token::Fuzzy(token.to_owned()))
                .collect(),
        );
    }
    flush(&mut buffer, &mut tokens, false);
    Query::Tokens(tokens)
}

/// pi's `matchSession`: the score when `session` matches, lower is better.
fn match_session(session: &SessionSummary, query: &Query) -> Option<f64> {
    let text = format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or_default(),
        session.all_messages_text,
        session.cwd
    );
    match query {
        Query::Invalid => None,
        Query::Regex(regex) => regex
            .find(&text)
            .map(|found| yapi_types::js::len(&text[..found.start()]) as f64 * 0.1),
        Query::Tokens(tokens) => {
            let mut total = 0.0;
            let mut normalized: Option<String> = None;
            for token in tokens {
                match token {
                    Token::Phrase(phrase) => {
                        let normalized = normalized.get_or_insert_with(|| normalize_lower(&text));
                        let phrase = normalize_lower(phrase);
                        if phrase.is_empty() {
                            continue;
                        }
                        let index = normalized.find(&phrase)?;
                        total += yapi_types::js::len(&normalized[..index]) as f64 * 0.1;
                    }
                    Token::Fuzzy(value) => {
                        let result = fuzzy_match(value, &text);
                        if !result.matches {
                            return None;
                        }
                        total += result.score;
                    }
                }
            }
            Some(total)
        }
    }
}

/// pi's `buildSessionTree` and `flattenSessionTree`.
fn threaded(sessions: Vec<SessionSummary>) -> Vec<Row> {
    let paths: Vec<PathBuf> = sessions
        .iter()
        .map(|session| canonical(&session.path))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); sessions.len()];
    let mut roots = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let parent = session
            .parent_session
            .as_deref()
            .map(|parent| canonical(Path::new(parent)))
            .and_then(|parent| paths.iter().position(|path| *path == parent));
        match parent {
            Some(parent) if parent != index => children[parent].push(index),
            _ => roots.push(index),
        }
    }
    let mut latest: Vec<u64> = sessions.iter().map(|session| session.modified_ms).collect();
    fn update(node: usize, children: &[Vec<usize>], latest: &mut [u64]) -> u64 {
        let mut value = latest[node];
        for &child in &children[node] {
            value = value.max(update(child, children, latest));
        }
        latest[node] = value;
        value
    }
    for &root in &roots {
        update(root, &children, &mut latest);
    }
    roots.sort_by_key(|&index| std::cmp::Reverse(latest[index]));
    for list in &mut children {
        list.sort_by_key(|&index| std::cmp::Reverse(latest[index]));
    }
    let mut order: Vec<(usize, usize, bool, Vec<bool>)> = Vec::new();
    fn walk(
        node: usize,
        depth: usize,
        continues: Vec<bool>,
        last: bool,
        children: &[Vec<usize>],
        order: &mut Vec<(usize, usize, bool, Vec<bool>)>,
    ) {
        order.push((node, depth, last, continues.clone()));
        let count = children[node].len();
        for (position, &child) in children[node].iter().enumerate() {
            let mut next = continues.clone();
            next.push(if depth > 0 { !last } else { false });
            walk(
                child,
                depth + 1,
                next,
                position + 1 == count,
                children,
                order,
            );
        }
    }
    let count = roots.len();
    for (position, &root) in roots.iter().enumerate() {
        walk(
            root,
            0,
            Vec::new(),
            position + 1 == count,
            &children,
            &mut order,
        );
    }
    let mut sessions: Vec<Option<SessionSummary>> = sessions.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|(index, depth, last, continues)| {
            sessions[index].take().map(|session| Row {
                session,
                depth,
                last,
                continues,
            })
        })
        .collect()
}

/// The `/resume` selector.
pub struct SessionSelector {
    sources: Sources,
    current_file: Option<PathBuf>,
    home: Option<String>,
    scope: Scope,
    sort: Sort,
    named_only: bool,
    show_path: bool,
    current: Option<Vec<SessionSummary>>,
    all: Option<Vec<SessionSummary>>,
    rows: Vec<Row>,
    selected: usize,
    touched: bool,
    input: TextInput,
    confirming_delete: Option<PathBuf>,
    status: Option<(bool, String, Instant)>,
    rename: Option<(PathBuf, TextInput)>,
    show_rename_hint: bool,
}

impl SessionSelector {
    /// A selector over `sources`; `current_file` is the open session.
    /// `rename` enables renaming.
    pub fn new(
        sources: Sources,
        current_file: Option<PathBuf>,
        home: Option<String>,
        rename: bool,
    ) -> SessionSelector {
        let mut input = TextInput::default();
        input.focused = true;
        let mut selector = SessionSelector {
            sources,
            current_file: current_file.map(|path| canonical(&path)),
            home,
            scope: Scope::Current,
            sort: Sort::Threaded,
            named_only: false,
            show_path: false,
            current: None,
            all: None,
            rows: Vec::new(),
            selected: 0,
            touched: false,
            input,
            confirming_delete: None,
            status: None,
            rename: None,
            show_rename_hint: rename,
        };
        selector.load(Scope::Current);
        selector
    }

    fn load(&mut self, scope: Scope) {
        let sessions = self.sources.load(scope);
        // pi lists sessions as they load and first shows only the one loaded
        // first, which a moved selection lands on and then follows.
        if scope == self.scope
            && let Some(first) = first_loaded(&sessions, scope)
        {
            self.store(scope, vec![first.clone()]);
            self.set_sessions();
        }
        self.store(scope, sessions);
        self.set_sessions();
    }

    fn store(&mut self, scope: Scope, sessions: Vec<SessionSummary>) {
        match scope {
            Scope::Current => self.current = Some(sessions),
            Scope::All => self.all = Some(sessions),
        }
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        match self.scope {
            Scope::Current => self.current.clone().unwrap_or_default(),
            Scope::All => self.all.clone().unwrap_or_default(),
        }
    }

    fn set_sessions(&mut self) {
        let selected = self
            .touched
            .then(|| {
                self.rows
                    .get(self.selected)
                    .map(|row| row.session.path.clone())
            })
            .flatten();
        self.filter();
        if !self.touched {
            self.selected = 0;
        } else if let Some(path) = selected
            && let Some(index) = self.rows.iter().position(|row| row.session.path == path)
        {
            self.selected = index;
        }
    }

    fn filter(&mut self) {
        let query = self.input.value().to_owned();
        let sessions: Vec<SessionSummary> = self
            .sessions()
            .into_iter()
            .filter(|session| {
                !self.named_only
                    || session
                        .name
                        .as_deref()
                        .is_some_and(|name| !name.trim().is_empty())
            })
            .collect();
        self.rows = if self.sort == Sort::Threaded && query.trim().is_empty() {
            threaded(sessions)
        } else {
            let flat = |session| Row {
                session,
                depth: 0,
                last: true,
                continues: Vec::new(),
            };
            if query.trim().is_empty() {
                sessions.into_iter().map(flat).collect()
            } else {
                let parsed = parse_query(&query);
                if self.sort == Sort::Recent {
                    sessions
                        .into_iter()
                        .filter(|session| match_session(session, &parsed).is_some())
                        .map(flat)
                        .collect()
                } else {
                    let mut scored: Vec<(SessionSummary, f64)> = sessions
                        .into_iter()
                        .filter_map(|session| {
                            match_session(&session, &parsed).map(|score| (session, score))
                        })
                        .collect();
                    scored.sort_by(|a, b| {
                        a.1.total_cmp(&b.1)
                            .then_with(|| b.0.modified_ms.cmp(&a.0.modified_ms))
                    });
                    scored
                        .into_iter()
                        .map(|(session, _)| flat(session))
                        .collect()
                }
            }
        };
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn set_status(&mut self, error: bool, message: String, duration: Duration) {
        self.status = Some((error, message, Instant::now() + duration));
    }

    /// Whether a timed status message is showing, so frames must keep coming.
    pub fn has_timed_status(&self) -> bool {
        self.status.is_some()
    }

    /// Expires the status message.
    pub fn tick(&mut self) {
        if self
            .status
            .as_ref()
            .is_some_and(|(_, _, until)| Instant::now() >= *until)
        {
            self.status = None;
        }
    }

    fn shorten(&self, path: &str) -> String {
        super::tools::shorten_home(path, self.home.as_deref())
    }

    fn is_current(&self, path: &Path) -> bool {
        self.current_file
            .as_ref()
            .is_some_and(|current| *current == canonical(path))
    }

    fn header(&self, width: usize, ui: &Ui<'_>) -> Vec<StyledLine> {
        let theme = ui.theme;
        let muted = theme.fg("muted");
        let accent = theme.fg("accent");
        let title = match self.scope {
            Scope::Current => "Resume Session (Current Folder)",
            Scope::All => "Resume Session (All)",
        };
        let mut right: Vec<Span<'static>> = match self.scope {
            Scope::Current => vec![
                Span::styled("◉ Current Folder", accent),
                Span::styled(" | ○ All", muted),
            ],
            Scope::All => vec![
                Span::styled("○ Current Folder | ", muted),
                Span::styled("◉ All", accent),
            ],
        };
        right.extend([
            Span::raw("  "),
            Span::styled("Name: ", muted),
            Span::styled(if self.named_only { "Named" } else { "All" }, accent),
            Span::raw("  "),
            Span::styled("Sort: ", muted),
            Span::styled(
                match self.sort {
                    Sort::Threaded => "Threaded",
                    Sort::Recent => "Recent",
                    Sort::Relevance => "Fuzzy",
                },
                accent,
            ),
        ]);
        let right = lines::truncate(&Line::from(right), width, "");
        let available = width.saturating_sub(lines::width(&right) + 1);
        let left = lines::truncate(
            &styled(title, Style::new().add_modifier(Modifier::BOLD)),
            available,
            "",
        );
        let spacing = width.saturating_sub(lines::width(&left) + lines::width(&right));
        let mut first = left;
        first.spans.push(Span::raw(" ".repeat(spacing)));
        first.spans.extend(right.spans);

        let (hint1, hint2) = if self.confirming_delete.is_some() {
            // pi colors the whole hint as an error, but the key hints' own
            // colors and resets win inside it.
            let mut hint = vec![Span::styled("Delete session? ", theme.fg("error"))];
            hint.extend(
                [
                    ui.key_hint("tui.select.confirm", "confirm"),
                    ui.key_hint("tui.select.cancel", "cancel"),
                ]
                .join(&Span::raw(" · ")),
            );
            (
                lines::truncate(&Line::from(hint), width, "…"),
                Line::default(),
            )
        } else if let Some((error, message, _)) = &self.status {
            (
                styled(
                    truncate_to_width(message, width, "…", false),
                    theme.fg(if *error { "error" } else { "accent" }),
                ),
                Line::default(),
            )
        } else {
            let separator = Span::styled(" · ", muted);
            let mut hint1 = ui.key_hint("tui.input.tab", "scope");
            hint1.push(separator.clone());
            hint1.push(Span::styled("re:<pattern> regex · \"phrase\" exact", muted));
            let path_state = if self.show_path { "(on)" } else { "(off)" };
            let mut parts = vec![
                ui.key_hint("app.session.toggleSort", "sort"),
                ui.key_hint("app.session.toggleNamedFilter", "named"),
                ui.key_hint("app.session.delete", "delete"),
                ui.key_hint("app.session.togglePath", &format!("path {path_state}")),
            ];
            if self.show_rename_hint {
                parts.push(ui.key_hint("app.session.rename", "rename"));
            }
            let hint2 = parts.join(&separator);
            (
                lines::truncate(&Line::from(hint1), width, "…"),
                lines::truncate(&Line::from(hint2), width, "…"),
            )
        };
        vec![first, hint1, hint2]
    }

    fn list(&mut self, width: usize, ui: &Ui<'_>) -> (Vec<StyledLine>, Option<usize>) {
        let theme = ui.theme;
        let mut out = vec![self.input.render(width), Line::default()];
        let cursor = self.input.cursor_column();
        if self.rows.is_empty() {
            let toggle = super::keybindings::keys_text(ui.keys, "app.session.toggleNamedFilter");
            let all = self.scope == Scope::All;
            let message = match (self.named_only, all) {
                (true, true) => format!("  No named sessions found. Press {toggle} to show all."),
                (true, false) => format!(
                    "  No named sessions in current folder. Press {toggle} to show all, or Tab to view all."
                ),
                (false, true) => "  No sessions found".to_owned(),
                (false, false) => {
                    "  No sessions in current folder. Press Tab to view all.".to_owned()
                }
            };
            out.push(styled(
                truncate_to_width(&message, width, "…", false),
                theme.fg("muted"),
            ));
            return (out, cursor);
        }
        let now = yapi_core::time::now_ms();
        let count = self.rows.len();
        let (start, end) = visible_range(self.selected, count, MAX_VISIBLE);
        for position in start..end {
            let row = &self.rows[position];
            let session = &row.session;
            let selected = position == self.selected;
            let deleting = self.confirming_delete.as_deref() == Some(session.path.as_path());
            let prefix = if row.depth == 0 {
                String::new()
            } else {
                let mut prefix: String = row
                    .continues
                    .iter()
                    .map(|continues| if *continues { "│  " } else { "   " })
                    .collect();
                prefix.push_str(if row.last { "└─ " } else { "├─ " });
                prefix
            };
            let display = session.name.as_deref().unwrap_or(&session.first_message);
            let normalized: String = display
                .chars()
                .map(|c| {
                    if (c as u32) < 0x20 || c == '\x7f' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            let normalized = normalized.trim();
            let mut right = format!(
                "{} {}",
                session.message_count,
                age(session.modified_ms, now)
            );
            if self.scope == Scope::All && !session.cwd.is_empty() {
                right = format!("{} {right}", self.shorten(&session.cwd));
            }
            if self.show_path {
                right = format!(
                    "{} {right}",
                    self.shorten(&session.path.display().to_string())
                );
            }
            let available = width as isize
                - 2
                - visible_width(&prefix) as isize
                - (visible_width(&right) + 2) as isize;
            let message = truncate_to_width(normalized, available.max(10) as usize, "…", false);
            let mut message_style = if deleting {
                theme.fg("error")
            } else if self.is_current(&session.path) {
                theme.fg("accent")
            } else if session.name.is_some() {
                theme.fg("warning")
            } else {
                Style::new()
            };
            if selected {
                message_style = message_style.add_modifier(Modifier::BOLD);
            }
            let mut spans = vec![
                if selected {
                    Span::styled("› ", theme.fg("accent"))
                } else {
                    Span::raw("  ")
                },
                Span::styled(prefix, theme.fg("dim")),
                Span::styled(message, message_style),
            ];
            let left_width: usize = spans.iter().map(|span| visible_width(&span.content)).sum();
            let spacing = width
                .saturating_sub(left_width + visible_width(&right))
                .max(1);
            spans.push(Span::raw(" ".repeat(spacing)));
            spans.push(Span::styled(
                right,
                theme.fg(if deleting { "error" } else { "dim" }),
            ));
            let mut line = Line::from(spans);
            if selected {
                let bg = theme.bg("selectedBg");
                for span in &mut line.spans {
                    span.style = bg.patch(span.style);
                }
            }
            out.push(lines::truncate(&line, width, "..."));
        }
        if start > 0 || end < count {
            out.push(styled(
                truncate_to_width(
                    &format!("  ({}/{count})", self.selected + 1),
                    width,
                    "",
                    false,
                ),
                theme.fg("muted"),
            ));
        }
        (out, cursor)
    }

    /// The selector's rows and cursor.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        self.tick();
        let accent_border = lines::border(width, ui.theme.fg("accent"));
        let mut out = lines::spacer(1);
        out.push(accent_border.clone());
        out.extend(lines::spacer(1));
        let cursor;
        if let Some((_, input)) = &mut self.rename {
            out.extend(lines::text_row(
                styled("Rename Session", Style::new().add_modifier(Modifier::BOLD)),
                width,
                1,
            ));
            out.extend(lines::spacer(1));
            let row = input.render(width);
            cursor = input.cursor_column().map(|col| (out.len(), col));
            out.push(row);
            out.extend(lines::spacer(1));
            out.extend(lines::text_row(
                styled(
                    format!(
                        "{} to save · {} to cancel",
                        super::keybindings::keys_text(ui.keys, "tui.select.confirm"),
                        super::keybindings::keys_text(ui.keys, "tui.select.cancel")
                    ),
                    ui.theme.fg("muted"),
                ),
                width,
                1,
            ));
        } else {
            out.extend(self.header(width, ui));
            out.extend(lines::spacer(1));
            let offset = out.len();
            let (list, column) = self.list(width, ui);
            cursor = column.map(|col| (offset, col));
            out.extend(list);
        }
        out.extend(lines::spacer(1));
        out.push(accent_border);
        (out, cursor)
    }

    fn start_delete(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if self.is_current(&row.session.path) {
            self.set_status(
                true,
                "Cannot delete the currently active session".into(),
                Duration::from_secs(3),
            );
            return;
        }
        self.confirming_delete = Some(row.session.path.clone());
    }

    fn delete(&mut self, path: &Path) {
        let mut command = std::process::Command::new("trash");
        if path.to_string_lossy().starts_with('-') {
            command.arg("--");
        }
        let trash = command
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output();
        let trashed = trash.as_ref().is_ok_and(|output| output.status.success()) || !path.exists();
        let result = if trashed {
            Ok("Session moved to trash")
        } else {
            std::fs::remove_file(path)
                .map(|()| "Session deleted")
                .map_err(|error| {
                    let hint = match &trash {
                        Ok(output) => String::from_utf8_lossy(&output.stderr)
                            .lines()
                            .next()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(|line| {
                                format!("trash: {}", line.chars().take(200).collect::<String>())
                            }),
                        Err(error) => Some(format!("trash: {error}")),
                    };
                    match hint {
                        Some(hint) => format!("{error} ({hint})"),
                        None => error.to_string(),
                    }
                })
        };
        match result {
            Ok(message) => {
                for sessions in [&mut self.current, &mut self.all].into_iter().flatten() {
                    sessions.retain(|session| session.path != path);
                }
                self.set_sessions();
                self.set_status(false, message.into(), Duration::from_secs(2));
                self.reload();
            }
            Err(error) => self.set_status(
                true,
                format!("Failed to delete: {error}"),
                Duration::from_secs(3),
            ),
        }
    }

    fn reload(&mut self) {
        self.current = None;
        self.all = None;
        self.load(self.scope);
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if let Some((path, input)) = &mut self.rename {
            if kb.matches(data, "tui.select.cancel") {
                self.rename = None;
                return Outcome::None;
            }
            if let InputEvent::Submit(value) = input.handle_input(data, kb) {
                let name = value.trim().to_owned();
                if name.is_empty() {
                    return Outcome::None;
                }
                let path = path.clone();
                if let Ok(mut session) = SessionManager::open(&path, None, None) {
                    let _ = session.append_session_info(&name);
                }
                self.rename = None;
                self.reload();
            }
            return Outcome::None;
        }
        if let Some(path) = self.confirming_delete.clone() {
            if kb.matches(data, "tui.select.confirm") {
                self.confirming_delete = None;
                self.delete(&path);
            } else if kb.matches(data, "tui.select.cancel") {
                self.confirming_delete = None;
            }
            return Outcome::None;
        }
        if kb.matches(data, "tui.input.tab") {
            self.scope = match self.scope {
                Scope::Current => Scope::All,
                Scope::All => Scope::Current,
            };
            let loaded = match self.scope {
                Scope::Current => self.current.is_some(),
                Scope::All => self.all.is_some(),
            };
            if loaded {
                self.set_sessions();
            } else {
                self.load(self.scope);
            }
            return Outcome::None;
        }
        if kb.matches(data, "app.session.toggleSort") {
            self.sort = match self.sort {
                Sort::Threaded => Sort::Recent,
                Sort::Recent => Sort::Relevance,
                Sort::Relevance => Sort::Threaded,
            };
            self.filter();
            return Outcome::None;
        }
        if kb.matches(data, "app.session.toggleNamedFilter") {
            self.named_only = !self.named_only;
            self.filter();
            return Outcome::None;
        }
        if kb.matches(data, "app.session.togglePath") {
            self.show_path = !self.show_path;
            return Outcome::None;
        }
        if kb.matches(data, "app.session.delete") {
            self.start_delete();
            return Outcome::None;
        }
        if kb.matches(data, "app.session.rename") {
            if self.show_rename_hint
                && let Some(row) = self.rows.get(self.selected)
            {
                let mut input = TextInput::default();
                input.focused = true;
                input.set_value(row.session.name.as_deref().unwrap_or_default());
                self.rename = Some((row.session.path.clone(), input));
            }
            return Outcome::None;
        }
        if kb.matches(data, "app.session.deleteNoninvasive") {
            if self.input.value().is_empty() {
                self.start_delete();
            } else {
                self.input.handle_input(data, kb);
                self.filter();
            }
            return Outcome::None;
        }
        self.touched = true;
        let count = self.rows.len();
        if kb.matches(data, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") {
            self.selected = (self.selected + 1).min(count.saturating_sub(1));
        } else if kb.matches(data, "tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(MAX_VISIBLE);
        } else if kb.matches(data, "tui.select.pageDown") {
            self.selected = (self.selected + MAX_VISIBLE).min(count.saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") {
            if let Some(row) = self.rows.get(self.selected) {
                return Outcome::Done(Action::Resume(row.session.path.clone()));
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        } else {
            if let InputEvent::Submit(_) = self.input.handle_input(data, kb)
                && let Some(row) = self.rows.get(self.selected)
            {
                return Outcome::Done(Action::Resume(row.session.path.clone()));
            }
            self.filter();
        }
        Outcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, parent: Option<&str>, modified_ms: u64) -> SessionSummary {
        SessionSummary {
            path: PathBuf::from(format!("/s/{name}.jsonl")),
            id: name.to_owned(),
            cwd: "/w".into(),
            name: None,
            first_message: name.to_owned(),
            all_messages_text: format!("{name} text"),
            parent_session: parent.map(|parent| format!("/s/{parent}.jsonl")),
            message_count: 1,
            modified_ms,
        }
    }

    #[test]
    fn threads_forks_under_parents() {
        let rows = threaded(vec![
            summary("a", None, 10),
            summary("b", Some("a"), 30),
            summary("c", None, 20),
            summary("d", Some("a"), 5),
        ]);
        let order: Vec<(&str, usize, bool)> = rows
            .iter()
            .map(|row| (row.session.id.as_str(), row.depth, row.last))
            .collect();
        assert_eq!(
            order,
            [
                ("a", 0, false),
                ("b", 1, false),
                ("d", 1, true),
                ("c", 0, true)
            ]
        );
    }

    #[test]
    fn parses_queries() {
        let session = summary("alpha", None, 0);
        assert!(match_session(&session, &parse_query("alp")).is_some());
        assert!(match_session(&session, &parse_query("\"alpha text\"")).is_some());
        assert!(match_session(&session, &parse_query("\"text alpha\"")).is_none());
        assert!(match_session(&session, &parse_query("re:^ALP")).is_some());
        assert!(match_session(&session, &parse_query("re:(")).is_none());
        assert_eq!(age(0, 59_000), "now");
        assert_eq!(age(0, 3 * 86_400_000), "3d");
        assert_eq!(age(0, 40 * 86_400_000), "1mo");
    }
}
