//! The editor's completion source.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use yapi_types::collate::locale_compare;
use yapi_types::sync::lock;

use crate::fuzzy::fuzzy_filter;
use crate::select_list::SelectItem;
use crate::text::{is_autocomplete_separator, is_js_whitespace};

/// Completions for the text before the cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Suggestions {
    /// Candidates, best first.
    pub items: Vec<SelectItem>,
    /// The text being completed, such as `/mo` or `@src/ma`.
    pub prefix: String,
}

/// Editor content after applying a completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// The new lines.
    pub lines: Vec<String>,
    /// The cursor line.
    pub cursor_line: usize,
    /// The cursor's byte column.
    pub cursor_col: usize,
}

/// Supplies and applies completions. Cursor columns are byte offsets.
pub trait AutocompleteProvider {
    /// Completions at the cursor; `force` is set for an explicit Tab outside a
    /// trigger context.
    fn suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<Suggestions>;

    /// The editor content with `item` replacing `prefix` at the cursor.
    fn apply(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> Completion;

    /// Whether Tab should offer file completion at the cursor.
    fn should_trigger_file_completion(&self, _lines: &[String], _line: usize, _col: usize) -> bool {
        true
    }

    /// Characters besides `@` and `#` that open completion at a token boundary.
    fn trigger_characters(&self) -> Vec<char> {
        Vec::new()
    }

    /// Whether the last [`AutocompleteProvider::suggestions`] call is still
    /// working in the background; the host asks again when it is done.
    fn pending(&self) -> bool {
        false
    }
}

/// Completes a command's argument text; `None` or empty shows no list.
pub type ArgumentCompleter = Box<dyn Fn(&str) -> Option<Vec<SelectItem>>>;

/// A command offered after `/`.
pub struct SlashCommand {
    /// The name without the slash.
    pub name: String,
    /// What it does.
    pub description: Option<String>,
    /// Argument syntax, shown before the description.
    pub argument_hint: Option<String>,
    /// Argument completion.
    pub complete: Option<ArgumentCompleter>,
}

/// Slash commands, `@` file search and path completion.
///
/// Port of `CombinedAutocompleteProvider` in `packages/tui/src/autocomplete.ts`
/// in pi `v1.0.0`. File search runs `fd` when one is configured.
pub struct CombinedProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
    fd_path: Option<PathBuf>,
    home: Option<PathBuf>,
    /// The latest background `fd` search; see [`CombinedProvider::notify_with`].
    search: Arc<Mutex<Option<FileSearch>>>,
    /// Called from the search thread when its results are ready.
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    pending: AtomicBool,
}

/// An `fd` search for one base directory and query, as pi runs it for `@`
/// completion: in the background, killed when the query changes.
struct FileSearch {
    key: (PathBuf, String),
    /// Its entries once both passes finished.
    entries: Option<Vec<FoundPath>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for CombinedProvider {
    fn drop(&mut self) {
        if let Some(search) = lock(&self.search).as_ref() {
            search.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

const PATH_WRAPPERS: [(char, char); 5] =
    [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

fn is_path_delimiter(c: char) -> bool {
    matches!(c, ' ' | '\t' | '"' | '\'' | '=')
}

fn closer(c: char) -> Option<char> {
    PATH_WRAPPERS
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
}

/// The text is empty or ends at a token boundary.
fn at_token_start(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_none_or(is_autocomplete_separator)
}

/// Byte offset just past the last delimiter, or 0.
fn after_last_delimiter(text: &str) -> usize {
    text.char_indices()
        .rfind(|(_, c)| is_path_delimiter(*c) || is_autocomplete_separator(*c))
        .map_or(0, |(index, c)| index + c.len_utf8())
}

fn strip_leading_wrappers(token: &str) -> &str {
    let mut result = token;
    while let Some(first) = result.chars().next() {
        match closer(first) {
            Some(close) if !result[first.len_utf8()..].contains(close) => {
                result = &result[first.len_utf8()..];
            }
            _ => break,
        }
    }
    result
}

fn is_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while let Some(previous) = text[..start].chars().next_back() {
        if closer(previous).is_none() {
            break;
        }
        start -= previous.len_utf8();
    }
    text[..start]
        .chars()
        .next_back()
        .is_some_and(is_path_delimiter)
        || at_token_start(&text[..start])
}

fn extract_quoted_prefix(text: &str) -> Option<&str> {
    let mut quote_start = None;
    for (index, c) in text.char_indices() {
        if c == '"' {
            quote_start = match quote_start {
                Some(_) => None,
                None => Some(index),
            };
        }
    }
    let quote_start = quote_start?;
    if quote_start > 0 && text[..quote_start].ends_with('@') {
        return is_token_start(text, quote_start - 1).then(|| &text[quote_start - 1..]);
    }
    is_token_start(text, quote_start).then(|| &text[quote_start..])
}

struct PathPrefix<'a> {
    raw: &'a str,
    at: bool,
    quoted: bool,
}

fn parse_path_prefix(prefix: &str) -> PathPrefix<'_> {
    if let Some(raw) = prefix.strip_prefix("@\"") {
        PathPrefix {
            raw,
            at: true,
            quoted: true,
        }
    } else if let Some(raw) = prefix.strip_prefix('"') {
        PathPrefix {
            raw,
            at: false,
            quoted: true,
        }
    } else if let Some(raw) = prefix.strip_prefix('@') {
        PathPrefix {
            raw,
            at: true,
            quoted: false,
        }
    } else {
        PathPrefix {
            raw: prefix,
            at: false,
            quoted: false,
        }
    }
}

fn completion_value(path: &str, at: bool, quoted: bool) -> String {
    let at = if at { "@" } else { "" };
    if quoted || path.chars().any(is_autocomplete_separator) {
        format!("{at}\"{path}\"")
    } else {
        format!("{at}{path}")
    }
}

/// Node's `path.basename` for `/`-separated paths.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// Node's `path.dirname` for `/`-separated paths.
fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.starts_with('/') { "/" } else { "." }.to_owned();
    }
    match trimmed.rfind('/') {
        None => ".".to_owned(),
        Some(0) => "/".to_owned(),
        Some(index) => trimmed[..index].trim_end_matches('/').to_owned(),
    }
}

/// Node's `path.join` for relative `/`-separated paths.
fn join(dir: &str, name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in dir.split('/').chain(name.split('/')) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn fd_path_query(query: &str) -> String {
    let normalized = query.replace('\\', "/");
    if !normalized.contains('/') {
        return normalized;
    }
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    const SEPARATOR: &str = "[\\\\/]";
    let escape = |segment: &str| {
        segment
            .chars()
            .map(|c| {
                if ".*+?^${}()|[]\\".contains(c) {
                    format!("\\{c}")
                } else {
                    c.to_string()
                }
            })
            .collect::<String>()
    };
    let mut pattern = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(escape)
        .collect::<Vec<_>>()
        .join(SEPARATOR);
    if normalized.ends_with('/') {
        pattern.push_str(SEPARATOR);
    }
    pattern
}

#[derive(Clone)]
struct FoundPath {
    path: String,
    directory: bool,
}

/// Runs `command` to completion unless `cancelled` is set first, which
/// kills it; its output, or `None`.
fn run_cancellable(
    command: &mut std::process::Command,
    cancelled: &AtomicBool,
) -> Option<std::process::Output> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        match child.try_wait() {
            // `--max-results` keeps the output well within a pipe's buffer.
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
}

fn walk_with_fd(
    fd: &Path,
    base: &Path,
    query: &str,
    max_results: usize,
    max_depth: Option<usize>,
    cancelled: &AtomicBool,
) -> Option<Vec<FoundPath>> {
    let mut command = std::process::Command::new(fd);
    command
        .arg("--base-directory")
        .arg(base)
        .args(["--max-results", &max_results.to_string()])
        .args(["--type", "f", "--type", "d", "--follow", "--hidden"])
        .args([
            "--exclude",
            ".git",
            "--exclude",
            ".git/*",
            "--exclude",
            ".git/**",
        ]);
    if let Some(depth) = max_depth {
        command.args(["--max-depth", &depth.to_string()]);
    }
    if query.replace('\\', "/").contains('/') {
        command.arg("--full-path");
    }
    if !query.is_empty() {
        command.arg(fd_path_query(query));
    }
    let output = run_cancellable(&mut command, cancelled)?;
    if !output.status.success() {
        return Some(Vec::new());
    }
    let found = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let display = line.replace('\\', "/");
            let directory = display.ends_with('/');
            let normalized = display.trim_end_matches('/');
            if normalized == ".git"
                || normalized.starts_with(".git/")
                || normalized.contains("/.git/")
            {
                return None;
            }
            Some(FoundPath {
                path: display,
                directory,
            })
        })
        .collect();
    Some(found)
}

/// pi's two `fd` passes for a fuzzy `@` query: direct children first, then
/// the whole tree; `None` when cancelled.
fn search_with_fd(
    fd: &Path,
    base: &Path,
    query: &str,
    cancelled: &AtomicBool,
) -> Option<Vec<FoundPath>> {
    let mut entries = walk_with_fd(fd, base, query, 100, Some(1), cancelled)?;
    let mut seen: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    for entry in walk_with_fd(fd, base, query, 100, None, cancelled)? {
        if !seen.contains(&entry.path) {
            seen.push(entry.path.clone());
            entries.push(entry);
        }
    }
    Some(entries)
}

fn score_entry(path: &str, query: &str, directory: bool) -> u32 {
    let name = basename(path).to_lowercase();
    let query = query.to_lowercase();
    let mut score = if name == query {
        100
    } else if name.starts_with(&query) {
        80
    } else if name.contains(&query) {
        50
    } else if path.to_lowercase().contains(&query) {
        30
    } else {
        0
    };
    if directory && score > 0 {
        score += 10;
    }
    score
}

impl CombinedProvider {
    /// A provider completing `commands` and paths under `base_path`.
    pub fn new(
        commands: Vec<SlashCommand>,
        base_path: PathBuf,
        fd_path: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> CombinedProvider {
        CombinedProvider {
            commands,
            base_path,
            fd_path,
            home,
            search: Arc::default(),
            notify: None,
            pending: AtomicBool::new(false),
        }
    }

    /// Runs `@` file searches in the background, as pi does, calling
    /// `notify` when one finishes; the host then asks for suggestions again.
    /// Without it they run to completion before returning.
    pub fn notify_with(mut self, notify: Arc<dyn Fn() + Send + Sync>) -> CombinedProvider {
        self.notify = Some(notify);
        self
    }

    /// The entries `fd` finds for `query` under `base`, or `None` while a
    /// background search runs. A search for another query is cancelled.
    fn file_entries(&self, fd: &Path, base: &Path, query: &str) -> Option<Vec<FoundPath>> {
        let Some(notify) = &self.notify else {
            return search_with_fd(fd, base, query, &AtomicBool::new(false));
        };
        let key = (base.to_path_buf(), query.to_owned());
        let mut slot = lock(&self.search);
        if let Some(search) = slot.as_ref().filter(|search| search.key == key) {
            return search.entries.clone();
        }
        if let Some(previous) = slot.take() {
            previous.cancelled.store(true, Ordering::Relaxed);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        *slot = Some(FileSearch {
            key: key.clone(),
            entries: None,
            cancelled: Arc::clone(&cancelled),
        });
        let (fd, search, notify) = (
            fd.to_path_buf(),
            Arc::clone(&self.search),
            Arc::clone(notify),
        );
        std::thread::spawn(move || {
            let Some(entries) = search_with_fd(&fd, &key.0, &key.1, &cancelled) else {
                return;
            };
            if let Some(current) = lock(&search)
                .as_mut()
                .filter(|current| current.key == key && !cancelled.load(Ordering::Relaxed))
            {
                current.entries = Some(entries);
            } else {
                return;
            }
            notify();
        });
        None
    }

    fn expand_home(&self, path: &str) -> String {
        let Some(home) = self.home.as_deref().and_then(Path::to_str) else {
            return path.to_owned();
        };
        if let Some(rest) = path.strip_prefix("~/") {
            let mut expanded = format!("{}/{rest}", home.trim_end_matches('/'));
            if path.ends_with('/') && !expanded.ends_with('/') {
                expanded.push('/');
            }
            expanded
        } else if path == "~" {
            home.to_owned()
        } else {
            path.to_owned()
        }
    }

    fn resolve_dir(&self, raw: &str, expanded: &str) -> PathBuf {
        if raw.starts_with('~') || expanded.starts_with('/') {
            PathBuf::from(expanded)
        } else {
            self.base_path.join(expanded)
        }
    }

    fn command_suggestions(&self, command_text: &str) -> Option<Suggestions> {
        let prefix = &command_text[1..];
        let items: Vec<(usize, String)> = self
            .commands
            .iter()
            .enumerate()
            .map(|(index, command)| (index, command.name.clone()))
            .collect();
        let bare = fuzzy_filter(items.clone(), prefix, |(_, name)| {
            name.strip_prefix("skill:").unwrap_or(name).to_owned()
        });
        let matched: Vec<usize> = bare.iter().map(|(index, _)| *index).collect();
        let full = fuzzy_filter(
            items
                .into_iter()
                .filter(|(index, name)| name.starts_with("skill:") && !matched.contains(index))
                .collect(),
            prefix,
            |(_, name)| name.clone(),
        );
        let list: Vec<SelectItem> = bare
            .into_iter()
            .chain(full)
            .map(|(index, _)| {
                let command = &self.commands[index];
                let description = command.description.clone().unwrap_or_default();
                let description = match &command.argument_hint {
                    Some(hint) if description.is_empty() => hint.clone(),
                    Some(hint) => format!("{hint} — {description}"),
                    None => description,
                };
                SelectItem {
                    value: command.name.clone(),
                    label: command.name.clone(),
                    description: (!description.is_empty()).then_some(description),
                }
            })
            .collect();
        (!list.is_empty()).then(|| Suggestions {
            items: list,
            prefix: command_text.to_owned(),
        })
    }

    fn extract_at_prefix<'a>(&self, text: &'a str) -> Option<&'a str> {
        if let Some(quoted) = extract_quoted_prefix(text)
            && quoted.starts_with("@\"")
        {
            return Some(quoted);
        }
        let token = strip_leading_wrappers(&text[after_last_delimiter(text)..]);
        token.starts_with('@').then_some(token)
    }

    fn extract_path_prefix<'a>(&self, text: &'a str, force: bool) -> Option<&'a str> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            return Some(quoted);
        }
        let prefix = strip_leading_wrappers(&text[after_last_delimiter(text)..]);
        if force || prefix.contains('/') || prefix.starts_with('.') || prefix.starts_with("~/") {
            return Some(prefix);
        }
        (prefix.is_empty() && !text.is_empty() && at_token_start(text)).then_some(prefix)
    }

    fn file_suggestions(&self, prefix: &str) -> Vec<SelectItem> {
        let PathPrefix { raw, at, quoted } = parse_path_prefix(prefix);
        let expanded = if raw.starts_with('~') {
            self.expand_home(raw)
        } else {
            raw.to_owned()
        };
        let root = matches!(raw, "" | "./" | "../" | "~" | "~/" | "/");
        let (search_dir, search_prefix) = if root || raw.ends_with('/') {
            (self.resolve_dir(raw, &expanded), String::new())
        } else {
            (
                self.resolve_dir(raw, &dirname(&expanded)),
                basename(&expanded).to_owned(),
            )
        };
        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };
        let mut suggestions = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name
                .to_lowercase()
                .starts_with(&search_prefix.to_lowercase())
            {
                continue;
            }
            let directory = entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()));
            let relative = if raw.ends_with('/') {
                format!("{raw}{name}")
            } else if raw.contains('/') || raw.contains('\\') {
                if let Some(home_relative) = raw.strip_prefix("~/") {
                    let dir = dirname(home_relative);
                    if dir == "." {
                        format!("~/{name}")
                    } else {
                        format!("~/{}", join(&dir, &name))
                    }
                } else if raw.starts_with('/') {
                    match dirname(raw).as_str() {
                        "/" => format!("/{name}"),
                        dir => format!("{dir}/{name}"),
                    }
                } else {
                    let joined = join(&dirname(raw), &name);
                    if raw.starts_with("./") && !joined.starts_with("./") {
                        format!("./{joined}")
                    } else {
                        joined
                    }
                }
            } else if raw.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };
            let relative = relative.replace('\\', "/");
            let path = if directory {
                format!("{relative}/")
            } else {
                relative
            };
            suggestions.push(SelectItem {
                value: completion_value(&path, at, quoted),
                label: if directory { format!("{name}/") } else { name },
                description: None,
            });
        }
        suggestions.sort_by(|a, b| {
            let (a_dir, b_dir) = (a.label.ends_with('/'), b.label.ends_with('/'));
            b_dir
                .cmp(&a_dir)
                .then_with(|| locale_compare(&a.label, &b.label))
        });
        suggestions
    }

    /// pi's fuzzy `@` suggestions; `None` while their search runs.
    fn fuzzy_file_suggestions(&self, query: &str, quoted: bool) -> Option<Vec<SelectItem>> {
        let Some(fd) = &self.fd_path else {
            return Some(Vec::new());
        };
        let normalized = query.replace('\\', "/");
        let scoped = normalized.rfind('/').and_then(|slash| {
            let display_base = &normalized[..=slash];
            let base = if display_base.starts_with("~/") {
                PathBuf::from(self.expand_home(display_base))
            } else if display_base.starts_with('/') {
                PathBuf::from(display_base)
            } else {
                self.base_path.join(display_base)
            };
            base.is_dir().then(|| {
                (
                    base,
                    normalized[slash + 1..].to_owned(),
                    display_base.to_owned(),
                )
            })
        });
        let (base, fd_query) = match &scoped {
            Some((base, query, _)) => (base.clone(), query.clone()),
            None => (self.base_path.clone(), query.to_owned()),
        };
        let entries = self.file_entries(fd, &base, &fd_query)?;
        let mut scored: Vec<(FoundPath, u32)> = entries
            .into_iter()
            .map(|entry| {
                let score = if fd_query.is_empty() {
                    1
                } else {
                    score_entry(&entry.path, &fd_query, entry.directory)
                };
                (entry, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        let depth = |path: &str| path.split('/').filter(|part| !part.is_empty()).count();
        scored.sort_by(|(a, a_score), (b, b_score)| {
            b_score
                .cmp(a_score)
                .then_with(|| depth(&a.path).cmp(&depth(&b.path)))
                .then_with(|| yapi_types::js::len(&a.path).cmp(&yapi_types::js::len(&b.path)))
                .then_with(|| locale_compare(&a.path, &b.path))
        });
        let items: Vec<SelectItem> = scored
            .into_iter()
            .take(20)
            .map(|(entry, _)| {
                let without_slash = entry.path.strip_suffix('/').unwrap_or(&entry.path);
                let display = match &scoped {
                    Some((_, _, display_base)) if display_base == "/" => {
                        format!("/{without_slash}")
                    }
                    Some((_, _, display_base)) => format!("{display_base}{without_slash}"),
                    None => without_slash.to_owned(),
                };
                let name = basename(without_slash);
                let completion = if entry.directory {
                    format!("{display}/")
                } else {
                    display.clone()
                };
                SelectItem {
                    value: completion_value(&completion, true, quoted),
                    label: if entry.directory {
                        format!("{name}/")
                    } else {
                        name.to_owned()
                    },
                    description: Some(display),
                }
            })
            .collect();
        Some(items)
    }
}

impl AutocompleteProvider for CombinedProvider {
    fn suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<Suggestions> {
        let current = lines.get(line).map_or("", String::as_str);
        let before = &current[..col.min(current.len())];
        self.pending.store(false, Ordering::Relaxed);
        if let Some(prefix) = self.extract_at_prefix(before) {
            let PathPrefix { raw, quoted, .. } = parse_path_prefix(prefix);
            let Some(items) = self.fuzzy_file_suggestions(raw, quoted) else {
                self.pending.store(true, Ordering::Relaxed);
                return None;
            };
            return (!items.is_empty()).then(|| Suggestions {
                items,
                prefix: prefix.to_owned(),
            });
        }
        let command_text = before.trim_start_matches(is_js_whitespace);
        if !force && command_text.starts_with('/') {
            let Some(space) = command_text.find(' ') else {
                return self.command_suggestions(command_text);
            };
            let name = &command_text[1..space];
            let argument = &command_text[space + 1..];
            let complete = self
                .commands
                .iter()
                .find(|command| command.name == name)?
                .complete
                .as_ref()?;
            let items = complete(argument).filter(|items| !items.is_empty())?;
            return Some(Suggestions {
                items,
                prefix: argument.to_owned(),
            });
        }
        let prefix = self.extract_path_prefix(before, force)?;
        let items = self.file_suggestions(prefix);
        (!items.is_empty()).then(|| Suggestions {
            items,
            prefix: prefix.to_owned(),
        })
    }

    fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    fn apply(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> Completion {
        let current = lines.get(line).map_or("", String::as_str);
        let col = col.min(current.len());
        let before_prefix = &current[..col.saturating_sub(prefix.len())];
        let after_cursor = &current[col..];
        let quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let after = if quoted_prefix && item.value.ends_with('"') && after_cursor.starts_with('"') {
            &after_cursor[1..]
        } else {
            after_cursor
        };
        let with_line = |text: String| {
            let mut out = lines.to_vec();
            if line < out.len() {
                out[line] = text;
            } else {
                out.push(text);
            }
            out
        };
        let directory = item.label.ends_with('/');
        let trailing_quote = item.value.ends_with('"');
        let offset = if directory && trailing_quote {
            item.value.len() - 1
        } else {
            item.value.len()
        };
        if prefix.starts_with('/') && before_prefix.trim().is_empty() && !prefix[1..].contains('/')
        {
            return Completion {
                lines: with_line(format!("{before_prefix}/{} {after}", item.value)),
                cursor_line: line,
                cursor_col: before_prefix.len() + item.value.len() + 2,
            };
        }
        if prefix.starts_with('@') {
            let suffix = if directory { "" } else { " " };
            return Completion {
                lines: with_line(format!("{before_prefix}{}{suffix}{after}", item.value)),
                cursor_line: line,
                cursor_col: before_prefix.len() + offset + suffix.len(),
            };
        }
        Completion {
            lines: with_line(format!("{before_prefix}{}{after}", item.value)),
            cursor_line: line,
            cursor_col: before_prefix.len() + offset,
        }
    }

    fn should_trigger_file_completion(&self, lines: &[String], line: usize, col: usize) -> bool {
        let current = lines.get(line).map_or("", String::as_str);
        let before = current[..col.min(current.len())].trim();
        !(before.starts_with('/') && !before.contains(' '))
    }
}
