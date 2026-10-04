//! Built-in slash commands: the autocomplete list and the handlers that print
//! into the transcript.
//!
//! Ports of `core/slash-commands.ts` and the command handlers in
//! `modes/interactive/interactive-mode.ts` in pi `v1.0.0`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use ri_core::agent_session::AgentSession;
use ri_core::session::SessionManager;
use ri_tui::autocomplete::{CombinedProvider, SlashCommand};
use ri_tui::fuzzy::fuzzy_filter;
use ri_tui::keybindings::Keybindings;
use ri_tui::keys::Keys;
use ri_tui::lines::{self, StyledLine, styled};
use ri_tui::markdown::{self, MarkdownOptions, MarkdownTheme};
use ri_tui::select_list::SelectItem;
use ri_tui::theme::Theme;

use super::chat::group_thousands;
use super::footer::format_tokens;
use super::keybindings::keys_display;

/// pi's `BUILTIN_SLASH_COMMANDS`: name, description and argument hint, in
/// autocomplete order.
pub const BUILTIN: &[(&str, &str, Option<&str>)] = &[
    ("settings", "Open settings menu", None),
    (
        "model",
        "Select model (opens selector UI)",
        Some("<provider/model>"),
    ),
    ("tree", "Navigate session tree (switch branches)", None),
    ("thinking", "Set thinking level", Some("<level>")),
    (
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        None,
    ),
    (
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        None,
    ),
    (
        "import",
        "Import and resume a session from a JSONL file",
        None,
    ),
    ("share", "Share session as a secret GitHub gist", None),
    (
        "bug",
        "Report a bug to the Pi developers",
        Some("<description>"),
    ),
    ("copy", "Copy last agent message to clipboard", None),
    ("name", "Set session display name", None),
    ("session", "Show session info and stats", None),
    ("changelog", "Show changelog entries", None),
    ("hotkeys", "Show all keyboard shortcuts", None),
    (
        "fork",
        "Create a new fork from a previous user message",
        None,
    ),
    (
        "clone",
        "Duplicate the current session at the current position",
        None,
    ),
    (
        "trust",
        "Save project trust decision for future sessions",
        None,
    ),
    (
        "login",
        "Configure provider authentication",
        Some("<provider>"),
    ),
    ("logout", "Remove provider authentication", None),
    ("new", "Start a new session", None),
    ("compact", "Manually compact the session context", None),
    ("resume", "Resume a different session", None),
    (
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
    ),
    ("quit", "Quit ri", None),
];

/// pi's `getAutocompleteSourceTag` for an extension, skill or prompt: none
/// for built-ins, the scope (`u`, `p` or `t`) for local files, and the scope
/// with the package for npm and git packages.
fn source_tag(source: &ri_types::rpc::SourceInfo) -> Option<String> {
    if source.source == "builtin" {
        return None;
    }
    let scope = match source.scope.as_str() {
        "user" => "u",
        "project" => "p",
        _ => "t",
    };
    let origin = source.source.trim();
    if matches!(origin, "auto" | "local" | "cli") {
        return Some(scope.to_owned());
    }
    if origin.starts_with("npm:") {
        return Some(format!("{scope}:{origin}"));
    }
    match ri_core::packages::source::parse(origin) {
        ri_core::packages::source::Source::Git {
            host,
            path,
            reference,
            ..
        } => {
            let reference = reference.map(|r| format!("@{r}")).unwrap_or_default();
            Some(format!("{scope}:git:{host}/{path}{reference}"))
        }
        _ => Some(scope.to_owned()),
    }
}

fn tagged(description: &str, tag: &str) -> String {
    if description.is_empty() {
        format!("[{tag}]")
    } else {
        format!("[{tag}] {description}")
    }
}

/// pi's `getModelSearchText`.
fn model_search_text(id: &str, provider: &str, name: &str) -> String {
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(" {name}")
    };
    format!("{id} {provider} {provider}/{id} {provider} {id}{name}")
}

/// The editor's completion source: built-in commands with model and thinking
/// arguments, prompt templates, skill commands and paths.
pub fn autocomplete(
    session: &AgentSession,
    fd: Option<PathBuf>,
    home: Option<PathBuf>,
) -> CombinedProvider {
    let mut commands: Vec<SlashCommand> = BUILTIN
        .iter()
        .map(|(name, description, hint)| SlashCommand {
            name: (*name).to_owned(),
            description: Some((*description).to_owned()),
            argument_hint: hint.map(str::to_owned),
            complete: None,
        })
        .collect();
    for command in &mut commands {
        match command.name.as_str() {
            "model" => {
                let session = session.clone();
                command.complete = Some(Box::new(move |prefix: &str| {
                    let models = session.models_in_scope();
                    if models.is_empty() {
                        return None;
                    }
                    let filtered = fuzzy_filter(models, prefix, |model| {
                        model_search_text(&model.id, &model.provider, &model.name)
                    });
                    (!filtered.is_empty()).then(|| {
                        filtered
                            .into_iter()
                            .map(|model| SelectItem {
                                value: format!("{}/{}", model.provider, model.id),
                                label: model.id,
                                description: Some(model.provider),
                            })
                            .collect()
                    })
                }));
            }
            "thinking" => {
                let session = session.clone();
                command.complete = Some(Box::new(move |prefix: &str| {
                    let levels =
                        fuzzy_filter(session.available_thinking_levels(), prefix, |level| {
                            level.as_str().to_owned()
                        });
                    (!levels.is_empty()).then(|| {
                        levels
                            .into_iter()
                            .map(|level| SelectItem::new(level.as_str()))
                            .collect()
                    })
                }));
            }
            _ => {}
        }
    }
    let cwd = session.cwd().to_path_buf();
    for template in &session.resources().templates {
        commands.push(SlashCommand {
            name: template.name.clone(),
            description: Some(match source_tag(&template.source) {
                Some(tag) => tagged(&template.description, &tag),
                None => template.description.clone(),
            }),
            argument_hint: template.argument_hint.clone(),
            complete: None,
        });
    }
    // Built-in extension commands are untagged, like built-in commands.
    let builtin: Vec<String> = commands
        .iter()
        .map(|command| command.name.clone())
        .collect();
    for resolved in session.extension_commands() {
        let (extension, command) = (&resolved.extension, resolved.command);
        {
            if builtin.contains(&command.name) {
                continue;
            }
            let owner = Arc::clone(extension);
            let name = command.name.clone();
            let description = match source_tag(&extension.source()) {
                Some(tag) => tagged(&command.description, &tag),
                None => command.description,
            };
            commands.push(SlashCommand {
                name: resolved.invocation,
                description: Some(description),
                argument_hint: None,
                complete: Some(Box::new(move |prefix: &str| {
                    owner.complete(&name, prefix).map(|items| {
                        items
                            .into_iter()
                            .map(|item| SelectItem {
                                value: item.value,
                                label: item.label,
                                description: item.description,
                            })
                            .collect()
                    })
                })),
            });
        }
    }
    if session.settings().enable_skill_commands.unwrap_or(true) {
        for skill in &session.resources().skills {
            commands.push(SlashCommand {
                name: format!("skill:{}", skill.name),
                description: Some(match source_tag(&skill.source) {
                    Some(tag) => tagged(&skill.description, &tag),
                    None => skill.description.clone(),
                }),
                argument_hint: None,
                complete: None,
            });
        }
    }
    CombinedProvider::new(commands, cwd, fd, home)
}

/// pi's `getPathCommandArgument`: the first argument after `/command `,
/// honoring single or double quotes.
pub fn path_argument(text: &str, command: &str) -> Option<String> {
    let rest = text.strip_prefix(command)?.strip_prefix(' ')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    match quote {
        Some(quote) => {
            let inner = &rest[1..];
            inner.find(quote).map(|end| inner[..end].to_owned())
        }
        None => rest.split_whitespace().next().map(str::to_owned),
    }
}

fn dim_label(theme: &Theme, label: &str, value: String) -> StyledLine {
    Line::from(vec![
        Span::styled(label.to_owned(), theme.fg("dim")),
        Span::raw(format!(" {value}")),
    ])
}

fn bold(text: &str) -> StyledLine {
    styled(
        text,
        ratatui_core::style::Style::new().add_modifier(Modifier::BOLD),
    )
}

/// The `/session` block, unwrapped.
pub fn session_info(session: &AgentSession, theme: &Theme) -> Vec<StyledLine> {
    let stats = session.session_stats();
    let totals = session.usage_totals();
    let (file, id, name) = session.with_session(|file| {
        (
            file.file().map(|path| path.display().to_string()),
            file.id().to_owned(),
            file.name(),
        )
    });
    let settings = session.settings();
    let dim = theme.fg("dim");
    let mut info: Vec<StyledLine> = vec![bold("Session Info"), Line::default()];
    if let Some(name) = name {
        info.push(dim_label(theme, "Name:", name));
    }
    info.push(dim_label(
        theme,
        "File:",
        file.unwrap_or_else(|| "In-memory".into()),
    ));
    info.push(dim_label(theme, "ID:", id));
    info.push(Line::default());
    info.push(bold("Messages"));
    info.push(dim_label(theme, "Total:", stats.total_messages.to_string()));
    info.push(dim_label(theme, "User:", stats.user_messages.to_string()));
    info.push(dim_label(
        theme,
        "Assistant:",
        stats.assistant_messages.to_string(),
    ));
    info.push(dim_label(
        theme,
        "Tools:",
        format!("{} calls, {} results", stats.tool_calls, stats.tool_results),
    ));
    info.push(Line::default());
    info.push(bold("Tokens"));
    let prompt = totals.input + totals.cache_read + totals.cache_write;
    info.push(dim_label(theme, "Input:", group_thousands(prompt)));
    if prompt > 0 && (totals.cache_read > 0 || totals.cache_write > 0) {
        info.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Cached:", dim),
            Span::raw(format!(" {} ", group_thousands(totals.cache_read))),
            Span::styled(
                format!("({:.1}%)", totals.cache_read as f64 / prompt as f64 * 100.0),
                dim,
            ),
        ]));
        let mut uncached = vec![
            Span::raw("  "),
            Span::styled("Uncached:", dim),
            Span::raw(format!(
                " {}",
                group_thousands(totals.input + totals.cache_write)
            )),
        ];
        if totals.cache_write > 0 {
            uncached.push(Span::raw(" "));
            uncached.push(Span::styled(
                format!("({} written to cache)", group_thousands(totals.cache_write)),
                dim,
            ));
        }
        info.push(Line::from(uncached));
    }
    info.push(dim_label(theme, "Output:", group_thousands(totals.output)));
    info.push(dim_label(
        theme,
        "Total:",
        group_thousands(prompt + totals.output),
    ));
    info.push(Line::default());
    info.push(bold("Cache Warming"));
    let mode = settings
        .cache_warming
        .as_ref()
        .and_then(|mode| serde_json::to_value(mode).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "streaming".into());
    info.push(dim_label(theme, "Mode:", mode));
    info.push(dim_label(
        theme,
        "Status:",
        "Inactive (cache warming unavailable)".into(),
    ));
    if totals.cost > 0.0 {
        info.push(Line::default());
        info.push(bold("Cost"));
        info.push(dim_label(theme, "Total:", format!("${:.3}", totals.cost)));
        let selected = session
            .model()
            .map(|model| format!("{}/{}", model.provider, model.id));
        let single_selected =
            stats.breakdown.len() == 1 && Some(&stats.breakdown[0].0) == selected.as_ref();
        if !single_selected {
            for (key, cost, tokens) in &stats.breakdown {
                info.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{key}:"), dim),
                    Span::raw(format!(" ${cost:.3} ")),
                    Span::styled(format!("({} tokens)", format_tokens(*tokens)), dim),
                ]));
            }
        }
    }
    info
}

/// The `/hotkeys` block.
pub fn hotkeys(
    keys: &Keybindings,
    shortcuts: &[ri_core::extensions::ShortcutBinding],
    theme: &Theme,
    markdown_theme: &MarkdownTheme,
    width: usize,
) -> Vec<StyledLine> {
    let k = |action: &str| keys_display(keys, action);
    let new_line_note = if cfg!(windows) {
        " (Ctrl+Enter on Windows Terminal)"
    } else {
        ""
    };
    let text = format!(
        "**Navigation**
| Key | Action |
|-----|--------|
| `{}` / `{}` / `{}` / `{}` | Move cursor / browse history |
| `{}` / `{}` | Move by word |
| `{}` | Start of line |
| `{}` | End of line |
| `{}` | Jump forward to character |
| `{}` | Jump backward to character |
| `{}` / `{}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{}` | Send message |
| `{}` | New line{new_line_note} |
| `{}` | Delete word backwards |
| `{}` | Delete word forwards |
| `{}` | Delete to start of line |
| `{}` | Delete to end of line |
| `{}` | Paste the most-recently-deleted text |
| `{}` | Cycle through the deleted text after pasting |
| `{}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{}` | Path completion / accept autocomplete |
| `{}` | Cancel autocomplete / abort streaming |
| `{}` | Clear editor (first) / exit (second) |
| `{}` | Exit (when editor is empty) |
| `{}` | Suspend to background |
| `{}` | Cycle thinking level |
| `{}` / `{}` | Cycle models |
| `{}` | Open model selector |
| `{}` | Toggle tool output expansion |
| `{}` | Toggle thinking block visibility |
| `{}` | Edit message in external editor |
| `{}` | Copy selection or last assistant message |
| `{}` | Queue follow-up message |
| `{}` | Restore queued messages |
| `{}` | Paste files on macOS, images, or text from clipboard |
| `/` | Slash commands |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |",
        k("tui.editor.cursorUp"),
        k("tui.editor.cursorDown"),
        k("tui.editor.cursorLeft"),
        k("tui.editor.cursorRight"),
        k("tui.editor.cursorWordLeft"),
        k("tui.editor.cursorWordRight"),
        k("tui.editor.cursorLineStart"),
        k("tui.editor.cursorLineEnd"),
        k("tui.editor.jumpForward"),
        k("tui.editor.jumpBackward"),
        k("tui.editor.pageUp"),
        k("tui.editor.pageDown"),
        k("tui.input.submit"),
        k("tui.input.newLine"),
        k("tui.editor.deleteWordBackward"),
        k("tui.editor.deleteWordForward"),
        k("tui.editor.deleteToLineStart"),
        k("tui.editor.deleteToLineEnd"),
        k("tui.editor.yank"),
        k("tui.editor.yankPop"),
        k("tui.editor.undo"),
        k("tui.input.tab"),
        k("app.interrupt"),
        k("app.clear"),
        k("app.exit"),
        k("app.suspend"),
        k("app.thinking.cycle"),
        k("app.model.cycleForward"),
        k("app.model.cycleBackward"),
        k("app.model.select"),
        k("app.tools.expand"),
        k("app.thinking.toggle"),
        k("app.editor.external"),
        k("app.message.copy"),
        k("app.message.followUp"),
        k("app.message.dequeue"),
        k("app.clipboard.pasteImage"),
    );
    let mut text = text;
    if !shortcuts.is_empty() {
        text.push_str("\n\n**Extensions**\n| Key | Action |\n|-----|--------|\n");
        for shortcut in shortcuts {
            let description = shortcut.description.as_deref().unwrap_or(&shortcut.path);
            text.push_str(&format!(
                "| `{}` | {description} |\n",
                super::keybindings::key_display(&shortcut.key)
            ));
        }
    }
    let text = text.trim().to_owned();
    let mut out = lines::spacer(1);
    out.push(lines::border(width, theme.fg("border")));
    out.extend(lines::text(
        &[styled(
            "Keyboard Shortcuts",
            theme.fg("accent").add_modifier(Modifier::BOLD),
        )],
        width,
        1,
        0,
        None,
    ));
    out.extend(lines::spacer(1));
    out.extend(markdown::render(
        &text,
        width,
        1,
        1,
        markdown_theme,
        MarkdownOptions::default(),
    ));
    out.push(lines::border(width, theme.fg("border")));
    out
}

/// Built-ins pi has that ri does not provide yet.
const UNAVAILABLE: &[&str] = &[
    "/settings",
    "/scoped-models",
    "/share",
    "/bug",
    "/arminsayshi",
    "/dementedelves",
];

impl super::App {
    /// Runs `text` when it is a built-in command, as pi's `onSubmit` matches
    /// them: exact names, or the name and a space for commands with arguments.
    pub(super) fn run_builtin(&mut self, text: &str) -> bool {
        let with_args = |name: &str| text == name || text.starts_with(&format!("{name} "));
        let argument = |name: &str| text.get(name.len()..).unwrap_or_default().trim().to_owned();
        if let Some(name) = UNAVAILABLE.iter().find(|name| {
            if matches!(**name, "/bug") {
                with_args(name)
            } else {
                text == **name
            }
        }) {
            self.editor.set_text("");
            self.error(format!("{name} is not available in ri yet"));
            return true;
        }
        if with_args("/login") {
            self.editor.set_text("");
            self.login_command(&argument("/login"));
            return true;
        }
        if text == "/logout" {
            self.editor.set_text("");
            self.logout_command();
            return true;
        }
        if with_args("/model") {
            self.editor.set_text("");
            let reference = argument("/model");
            if reference.is_empty() {
                self.open_model_selector("");
            } else {
                self.select_model(&reference);
            }
            return true;
        }
        if with_args("/thinking") {
            self.editor.set_text("");
            let level = argument("/thinking");
            if level.is_empty() {
                self.open_thinking_selector();
            } else {
                self.set_thinking(&level);
            }
            return true;
        }
        if with_args("/export") {
            self.editor.set_text("");
            self.export(text);
            return true;
        }
        if with_args("/import") {
            self.editor.set_text("");
            match path_argument(text, "/import") {
                None => self.error("Usage: /import <path.jsonl>"),
                Some(path) => {
                    self.dialog = Some(super::Dialog::Import(path.clone()));
                    self.selector = Some(super::Selector::Choice(super::ChoiceDialog::new(
                        &format!("Import session\nReplace current session with {path}?"),
                        &["Yes", "No"],
                    )));
                }
            }
            return true;
        }
        let command = match text {
            "/copy" | "/session" | "/changelog" | "/hotkeys" | "/fork" | "/clone" | "/tree"
            | "/new" | "/reload" | "/debug" | "/resume" | "/trust" | "/quit" => text,
            _ if with_args("/name") => "/name",
            _ if with_args("/compact") => "/compact",
            _ => return false,
        };
        self.editor.set_text("");
        match command {
            "/copy" => self.copy_last(),
            "/name" => self.name(&argument("/name")),
            "/session" => {
                let info = session_info(&self.session, &self.theme);
                self.text_item(info, true, (1, 0));
            }
            "/changelog" => {
                self.push(super::Item::Render(Box::new(|width, ctx| {
                    let mut out = lines::spacer(1);
                    out.push(lines::border(width, ctx.theme.fg("border")));
                    out.extend(lines::text(
                        &[styled(
                            "What's New",
                            ctx.theme.fg("accent").add_modifier(Modifier::BOLD),
                        )],
                        width,
                        1,
                        0,
                        None,
                    ));
                    out.extend(lines::spacer(1));
                    out.extend(markdown::render(
                        "No changelog entries found.",
                        width,
                        1,
                        1,
                        ctx.markdown,
                        MarkdownOptions::default(),
                    ));
                    out.push(lines::border(width, ctx.theme.fg("border")));
                    out
                })));
            }
            "/hotkeys" => {
                let (keys, shortcuts) = (self.keys.clone(), self.shortcuts.clone());
                self.push(super::Item::Render(Box::new(move |width, ctx| {
                    hotkeys(&keys, &shortcuts, ctx.theme, ctx.markdown, width)
                })));
            }
            "/fork" => self.open_fork(),
            "/trust" => {
                let store = ri_core::trust::TrustStore::new(&self.agent_dir);
                let selector = super::selectors::TrustSelector::new(
                    &self.cwd,
                    store.entry(&self.cwd),
                    self.session.project_trusted(),
                );
                self.selector = Some(super::Selector::Trust(Box::new(selector)));
            }
            "/clone" => {
                let leaf = self
                    .session
                    .with_session(|session| session.leaf_id().map(str::to_owned));
                match leaf {
                    None => self.status("Nothing to clone yet"),
                    Some(leaf) => self.fork(&leaf, true),
                }
            }
            "/tree" => self.open_tree(None),
            "/new" => self.new_session(),
            "/compact" => {
                let instructions = text.get(9..).unwrap_or_default().trim().to_owned();
                self.indicator = None;
                let session = self.session.clone();
                tokio::spawn(async move {
                    let instructions = (!instructions.is_empty()).then_some(instructions);
                    let _ = session.compact(instructions.as_deref()).await;
                });
            }
            "/reload" => self.reload(),
            "/debug" => self.debug(),
            "/resume" => self.open_resume(),
            "/quit" => self.quit = true,
            _ => {}
        }
        true
    }

    /// `/model <ref>`: pi's `findExactModelReferenceMatch`, else the selector
    /// searching for it.
    fn select_model(&mut self, reference: &str) {
        let models = self.session.models_in_scope();
        let found = ri_core::model_resolver::exact_match(reference, &models);
        match found {
            Some(model) => {
                let model = model.clone();
                let id = model.id.clone();
                match self.session.set_model(model) {
                    Ok(()) => {
                        self.status(format!("Model: {id}"));
                        self.warn_anthropic_subscription(None);
                    }
                    Err(error) => self.error(error),
                }
            }
            None => self.open_model_selector(reference),
        }
    }
    fn set_thinking(&mut self, value: &str) {
        let levels = self.session.available_thinking_levels();
        match levels
            .iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(value))
        {
            Some(level) => {
                self.session.set_thinking_level(*level);
                self.status(format!("Thinking level: {}", level.as_str()));
            }
            None => {
                let names: Vec<&str> = levels.iter().map(|level| level.as_str()).collect();
                self.error(format!(
                    "Unknown thinking level \"{value}\". Available levels: {}.",
                    names.join(", ")
                ));
            }
        }
    }

    fn export(&mut self, text: &str) {
        let path = path_argument(text, "/export");
        let Some(path) = path.clone().filter(|path| path.ends_with(".jsonl")) else {
            let rgb =
                |rgb: Option<[f64; 3]>| rgb.map(|[r, g, b]| ri_tui::color::Color::Rgb(r, g, b));
            let appearance = match self.colors.background {
                Some(background) => {
                    ri_tui::theme::terminal_appearance(background, self.colors.foreground)
                }
                None => ri_tui::theme::Appearance::Dark,
            };
            let theme = crate::export_html::ExportTheme {
                theme: &self.theme,
                foreground: rgb(self.colors.foreground),
                background: rgb(self.colors.background),
                appearance,
            };
            match crate::export_html::export_session(&self.session, path.as_deref(), &theme) {
                Ok(target) => self.status(format!("Session exported to: {}", target.display())),
                Err(error) => self.error(format!("Failed to export session: {error}")),
            }
            return;
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let target = ri_core::tools::path::resolve_to_cwd(&path, &cwd);
        let content = self
            .session
            .with_session(|session| session.serialize_branch());
        let node_error = ri_core::tools::node_error;
        let written = match target.parent().filter(|dir| !dir.exists()) {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|error| node_error(&error, "mkdir", dir))
            }
            None => Ok(()),
        }
        .and_then(|()| {
            std::fs::write(&target, content).map_err(|error| node_error(&error, "open", &target))
        });
        match written {
            Ok(()) => self.status(format!("Session exported to: {}", target.display())),
            Err(error) => self.error(format!("Failed to export session: {error}")),
        }
    }

    /// `/import`, confirmed: copies the file into the session directory unless
    /// it is already there, then resumes it.
    pub(super) fn import(&mut self, path: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let source = ri_core::tools::path::resolve_to_cwd(path, &cwd);
        if !source.exists() {
            self.error(format!(
                "Failed to import session: File not found: {}",
                source.display()
            ));
            return;
        }
        let dir = self
            .session
            .with_session(|session| session.dir().to_path_buf());
        let _ = std::fs::create_dir_all(&dir);
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut destination = dir.join(&name);
        let already =
            std::fs::canonicalize(&destination).ok() == std::fs::canonicalize(&source).ok();
        if !already {
            let stem = Path::new(&name)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let extension = Path::new(&name)
                .extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
                .unwrap_or_default();
            let mut suffix = 1;
            while destination.exists() {
                destination = dir.join(format!("{stem}-{suffix}{extension}"));
                suffix += 1;
            }
            if let Err(error) = std::fs::copy(&source, &destination) {
                self.fatal("Failed to import session", &error.to_string());
                return;
            }
        }
        match SessionManager::open(&destination, Some(&dir), None) {
            Ok(manager) => match self.replace_session(manager) {
                Ok(()) => self.status(format!("Session imported from: {path}")),
                Err(error) => self.fatal("Failed to import session", &error),
            },
            Err(error) => self.fatal("Failed to import session", &error.to_string()),
        }
    }

    fn name(&mut self, name: &str) {
        if name.is_empty() {
            match self.session.with_session(|session| session.name()) {
                Some(current) => self.note(format!("Session name: {current}"), "dim"),
                None => self.warning("Usage: /name <name>"),
            }
            return;
        }
        self.session.set_name(name);
        let stored = self.session.with_session(|session| session.name());
        if stored.as_deref() != Some(name) {
            let quote = |text: &str| ri_types::json::to_string(&text).unwrap_or_default();
            self.warning(format!(
                "Session name was normalized from {} to {}",
                quote(name),
                stored
                    .as_deref()
                    .map_or_else(|| "undefined".to_owned(), quote)
            ));
        }
        self.note(
            format!("Session name set: {}", stored.as_deref().unwrap_or(name)),
            "dim",
        );
    }

    fn reload(&mut self) {
        if self.running {
            self.warning("Wait for the current response to finish before reloading.");
            return;
        }
        if self.manual_compaction {
            self.warning("Wait for compaction to finish before reloading.");
            return;
        }
        // pi reloads the session in place, keeping its model and level.
        let previous = self.session.clone();
        let manager = self.session.take_session();
        let replaced = self.replace_session(manager);
        if let Err(error) = replaced {
            self.error(format!("Reload failed: {error}"));
            return;
        }
        self.session.keep_selection(&previous);
        self.keys = super::keybindings::load(&self.agent_dir, Keys::detect(self.kitty));
        self.keys.set_kitty(self.kitty);
        self.expand_key = super::keybindings::keys_text(&self.keys, "app.tools.expand");
        self.cancel_key = super::keybindings::keys_text(&self.keys, "tui.select.cancel");
        self.install_shortcuts();
        let (theme, theme_error) = super::load_theme(
            self.theme_override
                .as_deref()
                .or(self.session.settings().theme.as_deref()),
            &self.theme_files,
            &self.agent_dir,
            &self.colors,
            self.color_mode,
        );
        self.markdown = super::markdown_theme(&theme);
        self.editor.set_theme(super::editor_theme(&theme));
        self.theme = theme;
        self.style_alt_screen();
        let settings = self.session.settings();
        self.hide_thinking = settings.hide_thinking_block.unwrap_or(false);
        self.output_pad = usize::from(settings.output_pad.unwrap_or(1).min(1));
        self.invalidate_all();
        if let Some(error) = theme_error {
            self.error(error);
        }
        if let Some(error) = self
            .session
            .with_registry(|registry| registry.error().map(str::to_owned))
            .flatten()
        {
            self.error(format!("models.json error: {error}"));
        }
        self.status("Reloaded keybindings, extensions, skills, prompts, themes, and context files");
    }

    fn debug(&mut self) {
        let (width, height) = self.size;
        let mut rendered: Vec<StyledLine> = self.transcript(width);
        rendered.extend(self.dock(width).0);
        let mut data = vec![
            format!("Debug output at {}", ri_core::time::now_iso()),
            format!("Terminal: {width}x{height}"),
            format!("Total lines: {}", rendered.len()),
            String::new(),
            "=== All rendered lines with visible widths ===".to_owned(),
        ];
        for (index, line) in rendered.iter().enumerate() {
            let ansi = ri_tui::ansi::line_to_ansi(line);
            data.push(format!(
                "[{index}] (w={}) {}",
                lines::width(line),
                ri_types::json::to_string(&ansi).unwrap_or_default()
            ));
        }
        data.push(String::new());
        data.push("=== Agent messages (JSONL) ===".to_owned());
        for message in self.session.messages() {
            data.push(ri_types::json::to_string(&message).unwrap_or_default());
        }
        data.push(String::new());
        let path = self.agent_dir.join("ri-debug.log");
        let _ = std::fs::create_dir_all(&self.agent_dir);
        let _ = std::fs::write(&path, data.join("\n"));
        let notice = vec![
            styled("✓ Debug log written", self.theme.fg("accent")),
            styled(path.display().to_string(), self.theme.fg("muted")),
        ];
        self.text_item(notice, true, (1, 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_path_arguments() {
        assert_eq!(
            path_argument("/export out.jsonl extra", "/export").as_deref(),
            Some("out.jsonl")
        );
        assert_eq!(
            path_argument("/export  \"my file.html\"", "/export").as_deref(),
            Some("my file.html")
        );
        assert_eq!(path_argument("/export \"open", "/export"), None);
        assert_eq!(path_argument("/export", "/export"), None);
    }
}
