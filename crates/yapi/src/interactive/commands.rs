//! Built-in slash commands: the autocomplete list and the handlers that print
//! into the transcript.
//!
//! Ports of `core/slash-commands.ts` and the command handlers in
//! `modes/interactive/interactive-mode.ts` in pi `v1.0.0`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use yapi_core::agent_session::AgentSession;
use yapi_core::session::SessionManager;
use yapi_tui::autocomplete::{CombinedProvider, SlashCommand};
use yapi_tui::fuzzy::fuzzy_filter;
use yapi_tui::keybindings::Keybindings;
use yapi_tui::keys::Keys;
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::markdown::{self, MarkdownOptions};
use yapi_tui::theme::Theme;
use yapi_types::autocomplete::{ArgumentCompletions, AutocompleteItem};

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
    ("quit", "Quit yapi", None),
];

/// pi's `getAutocompleteSourceTag` for an extension, skill or prompt: none
/// for built-ins, the scope (`u`, `p` or `t`) for local files, and the scope
/// with the package for npm and git packages.
fn source_tag(source: &yapi_types::rpc::SourceInfo) -> Option<String> {
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
    match yapi_core::packages::source::parse(origin) {
        yapi_core::packages::source::Source::Git {
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

/// The editor's completion source: built-in commands with model, thinking
/// and login arguments, prompt templates, skill commands and paths.
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
                        return ArgumentCompletions::Ready(None);
                    }
                    let filtered = fuzzy_filter(models, prefix, super::scoped_models::search_text);
                    (!filtered.is_empty())
                        .then(|| {
                            filtered
                                .into_iter()
                                .map(|model| AutocompleteItem {
                                    value: model.reference(),
                                    label: model.id,
                                    description: Some(model.provider),
                                })
                                .collect()
                        })
                        .into()
                }));
            }
            "thinking" => {
                let session = session.clone();
                command.complete = Some(Box::new(move |prefix: &str| {
                    let levels =
                        fuzzy_filter(session.available_thinking_levels(), prefix, |level| {
                            level.as_str().to_owned()
                        });
                    (!levels.is_empty())
                        .then(|| {
                            levels
                                .into_iter()
                                .map(|level| AutocompleteItem::new(level.as_str()))
                                .collect()
                        })
                        .into()
                }));
            }
            "login" => {
                let session = session.clone();
                command.complete = Some(Box::new(move |prefix: &str| {
                    let options = super::login::login_options(&session.registry(), None);
                    let providers = fuzzy_filter(
                        super::login::completion_options(options),
                        prefix,
                        super::login::CompletionOption::search_text,
                    );
                    (!providers.is_empty())
                        .then(|| {
                            providers
                                .into_iter()
                                .map(|provider| AutocompleteItem {
                                    description: Some(provider.description()),
                                    value: provider.id.clone(),
                                    label: provider.id,
                                })
                                .collect()
                        })
                        .into()
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
                complete: Some(Box::new(move |prefix: &str| owner.complete(&name, prefix))),
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
        let selected = session.model().map(|model| model.reference());
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

/// The `/hotkeys` markdown.
fn hotkeys(keys: &Keybindings, shortcuts: &[yapi_core::extensions::ShortcutBinding]) -> String {
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
    text.trim().to_owned()
}

/// pi's `/hotkeys` and `/changelog` blocks: `title` and the `text` markdown
/// between borders.
fn framed_markdown(
    title: &str,
    text: &str,
    width: usize,
    ctx: &super::chat::RenderContext<'_>,
) -> Vec<StyledLine> {
    let border = ctx.theme.fg("border");
    let mut out = lines::spacer(1);
    out.push(lines::border(width, border));
    out.extend(lines::text_row(
        styled(title, ctx.theme.fg("accent").add_modifier(Modifier::BOLD)),
        width,
        1,
    ));
    out.extend(lines::spacer(1));
    out.extend(markdown::render(
        text,
        width,
        1,
        1,
        ctx.markdown,
        MarkdownOptions::default(),
    ));
    out.push(lines::border(width, border));
    out
}

/// pi's built-in commands in the order `onSubmit` checks them, and whether
/// each takes arguments after a space.
const COMMANDS: &[(&str, bool)] = &[
    ("/settings", false),
    ("/scoped-models", false),
    ("/model", true),
    ("/thinking", true),
    ("/export", true),
    ("/import", true),
    ("/share", false),
    ("/bug", true),
    ("/copy", false),
    ("/name", true),
    ("/session", false),
    ("/changelog", false),
    ("/hotkeys", false),
    ("/fork", false),
    ("/clone", false),
    ("/tree", false),
    ("/trust", false),
    ("/login", true),
    ("/logout", false),
    ("/new", false),
    ("/compact", true),
    ("/reload", false),
    ("/debug", false),
    ("/arminsayshi", false),
    ("/dementedelves", false),
    ("/resume", false),
    ("/quit", false),
];

impl super::App {
    /// Runs `text` when it is a built-in command, as pi's `onSubmit` matches
    /// them: exact names, or the name and a space for commands with arguments.
    pub(super) fn run_builtin(&mut self, text: &str) -> bool {
        let Some(&(command, _)) = COMMANDS.iter().find(|(name, arguments)| {
            text == *name || (*arguments && text.starts_with(&format!("{name} ")))
        }) else {
            return false;
        };
        let argument = text
            .get(command.len()..)
            .unwrap_or_default()
            .trim()
            .to_owned();
        self.set_editor_text("");
        match command {
            // pi built-ins that belong to pi's services and brand.
            "/share" | "/bug" | "/arminsayshi" | "/dementedelves" => {
                self.error(format!("{command} is not available in yapi"));
            }
            "/login" => self.login_command(&argument),
            "/logout" => self.logout_command(),
            "/scoped-models" => self.open_scoped_models(),
            "/settings" => self.open_settings(),
            "/model" if argument.is_empty() => self.open_model_selector(""),
            "/model" => self.select_model(&argument),
            "/thinking" if argument.is_empty() => self.open_thinking_selector(),
            "/thinking" => self.set_thinking(&argument),
            "/export" => self.export(text),
            "/import" => match path_argument(text, "/import") {
                None => self.error("Usage: /import <path.jsonl>"),
                Some(path) => {
                    self.dialog = Some(super::Dialog::Import(path.clone()));
                    self.selector = Some(super::Selector::Choice(super::ChoiceDialog::new(
                        &format!("Import session\nReplace current session with {path}?"),
                        &["Yes", "No"],
                    )));
                }
            },
            "/copy" => self.copy_last(),
            "/name" => self.name(&argument),
            "/session" => {
                let info = session_info(&self.session, &self.theme);
                self.text_item(info, true, (1, 0));
            }
            "/changelog" => {
                self.push(super::Item::Render(Box::new(|width, ctx| {
                    framed_markdown("What's New", "No changelog entries found.", width, ctx)
                })));
            }
            "/hotkeys" => {
                let text = hotkeys(&self.keys, &self.shortcuts);
                self.push(super::Item::Render(Box::new(move |width, ctx| {
                    framed_markdown("Keyboard Shortcuts", &text, width, ctx)
                })));
            }
            "/fork" => self.open_fork(),
            "/trust" => {
                let store = yapi_core::trust::TrustStore::new(&self.agent_dir);
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
                self.indicator = None;
                let session = self.session.clone();
                tokio::spawn(async move {
                    let instructions = (!argument.is_empty()).then_some(argument);
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

    /// `/model <ref>`: pi's `findExactModelMatch`. Without a cached match
    /// and outside a scope, the catalogs are refreshed before searching again.
    fn select_model(&mut self, reference: &str) {
        let models = self.session.models_in_scope();
        let found = yapi_core::model_resolver::exact_match(reference, &models).is_some();
        if found || !self.session.scoped_models().is_empty() {
            return self.select_model_now(reference);
        }
        self.status(super::catalogs::RefreshStatus::RUNNING.to_owned());
        self.refresh_catalogs(
            None,
            super::catalogs::Refresh::ModelSearch(reference.to_owned()),
        );
    }

    /// `/model <ref>` among the models listed now: the exact match, else the
    /// selector searching for it.
    pub(super) fn select_model_now(&mut self, reference: &str) {
        let models = self.session.models_in_scope();
        let found = yapi_core::model_resolver::exact_match(reference, &models);
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
            let theme = self.html_theme();
            match crate::export_html::export_session(&self.session, path.as_deref(), &theme) {
                Ok(target) => self.status(format!("Session exported to: {}", target.display())),
                Err(error) => self.error(format!("Failed to export session: {error}")),
            }
            return;
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let target = yapi_core::tools::path::resolve_to_cwd(&path, &cwd);
        let content = self
            .session
            .with_session(|session| session.serialize_branch());
        let node_error = yapi_core::tools::node_error;
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

    /// The theme and terminal colors an HTML export is drawn with.
    fn html_theme(&self) -> crate::export_html::ExportTheme<'_> {
        let rgb = |rgb: Option<[f64; 3]>| rgb.map(|[r, g, b]| yapi_tui::color::Color::Rgb(r, g, b));
        crate::export_html::ExportTheme {
            theme: &self.theme,
            foreground: rgb(self.colors.foreground),
            background: rgb(self.colors.background),
            appearance: super::appearance(&self.colors),
        }
    }

    /// `/import`, confirmed: copies the file into the session directory unless
    /// it is already there, then resumes it.
    pub(super) fn import(&mut self, path: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let source = yapi_core::tools::path::resolve_to_cwd(path, &cwd);
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
            let quote = |text: &str| yapi_types::json::to_string(&text).unwrap_or_default();
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
        let settings = self.session.settings();
        self.hide_thinking = settings.hide_thinking_block.unwrap_or(false);
        self.output_pad = usize::from(settings.output_pad.unwrap_or(1).min(1));
        self.use_theme(self.theme_override.clone().or(settings.theme).as_deref());
        if let Some(error) = self
            .session
            .with_registry(|registry| registry.error().map(str::to_owned))
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
            format!("Debug output at {}", yapi_core::time::now_iso()),
            format!("Terminal: {width}x{height}"),
            format!("Total lines: {}", rendered.len()),
            String::new(),
            "=== All rendered lines with visible widths ===".to_owned(),
        ];
        for (index, line) in rendered.iter().enumerate() {
            let ansi = yapi_tui::ansi::line_to_ansi(line);
            data.push(format!(
                "[{index}] (w={}) {}",
                lines::width(line),
                yapi_types::json::to_string(&ansi).unwrap_or_default()
            ));
        }
        data.push(String::new());
        data.push("=== Agent messages (JSONL) ===".to_owned());
        for message in self.session.messages() {
            data.push(yapi_types::json::to_string(&message).unwrap_or_default());
        }
        data.push(String::new());
        let path = self.agent_dir.join("yapi-debug.log");
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

    #[tokio::test]
    async fn html_export_reads_colorfgbg_without_a_reported_background() {
        // COLORFGBG comes from the environment, so a child process checks it.
        const CHILD: &str = "YAPI_TEST_HTML_THEME_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "interactive::commands::tests::html_export_reads_colorfgbg_without_a_reported_background",
                ])
                .env(CHILD, "1")
                .env("COLORFGBG", "0;15")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let (app, _events) = super::super::tests::app();
        assert_eq!(
            app.html_theme().appearance,
            yapi_tui::theme::Appearance::Light
        );
    }

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
