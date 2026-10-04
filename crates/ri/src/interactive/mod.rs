//! Interactive mode: the full-screen or inline terminal UI.
//!
//! Port of `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! in pi `v1.0.0`, on pi-tui's line model: every component renders styled
//! lines for a width, and a renderer writes the frame.

mod bash_view;
mod chat;
mod clipboard;
mod commands;
mod extension_ui;
mod footer;
mod header;
pub mod keybindings;
mod login;
pub mod picker;
mod selectors;
mod session_selector;
mod themes;
mod tools;
mod tree_selector;
mod word_diff;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui_core::text::{Line, Span};
use ri_core::agent_session::{AgentSession, TreeNavigation, TreeOutcome};
use ri_core::bash_executor::BashResult;
use ri_core::extensions::{Mode, NotifyKind};
use ri_core::session::SessionManager;
use ri_tui::color::ColorMode;
use ri_tui::editor::{Editor, EditorEvent, EditorTheme};
use ri_tui::input::{Input, InputBuffer, escape_timeout};
use ri_tui::keybindings::Keybindings;
use ri_tui::keys::Keys;
use ri_tui::lines::{self, StyledLine};
use ri_tui::markdown::MarkdownTheme;
use ri_tui::screen::{ALT_SCREEN_ENTER, ALT_SCREEN_LEAVE, AltScreen, MainScreen};
use ri_tui::select_list::SelectListTheme;
use ri_tui::terminal::{
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, ColorQuery, Filtered, KeyboardProtocol,
    color_query,
};
use ri_tui::theme::{
    Appearance, SystemThemeInput, Theme, detect_colorfgbg, resolve_theme_setting,
    terminal_appearance,
};
use ri_types::event::{AgentEvent, AssistantMessageEvent, CompactionReason, ToolResult};
use ri_types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingContent, ToolCall,
};
use ri_types::settings::{DoubleEscapeAction, TuiMode};
use serde_json::{Map, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use self::bash_view::BashView;
use self::chat::{Item, RenderContext};
use self::selectors::{Action, ChoiceDialog, Outcome, Selector, TextDialog, Ui};
use self::tools::ToolView;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const DOUBLE_PRESS: Duration = Duration::from_millis(500);
const COLOR_QUERY_TIMEOUT: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STDIN_POLL: Duration = Duration::from_millis(50);

/// Events the loop handles. Session events carry the epoch of the session
/// that sent them, so a replaced session's late events are dropped.
enum Event {
    Input(Vec<u8>),
    InputClosed,
    Agent(u64, Box<AgentEvent>),
    PromptDone(u64, Result<(), String>),
    BashChunk(u64, u64, String),
    BashDone(u64, u64, Box<Result<BashResult, String>>),
    TreeDone(u64, String, Box<Result<TreeOutcome, String>>),
    Fd(Option<PathBuf>),
    Notify(u64, String, NotifyKind),
    /// A request from the sign-in with this id.
    Auth(u64, ri_ai::auth::AuthRequest),
    /// The sign-in with this id finished.
    LoginDone(u64, Result<(), ri_ai::auth::AuthError>),
    LogoutDone(
        Box<login::ProviderOption>,
        Result<(), ri_ai::auth::AuthError>,
    ),
    /// A request from an extension of the session with this epoch.
    Ui(u64, Box<extension_ui::Request>),
    /// Lines of the extension component with this key, rendered at a width.
    Rendered(u64, (u64, u32), usize, Vec<String>),
    /// The first session's extensions started.
    Bound,
    /// A component an extension built for a transcript item, answering the
    /// request with this sequence number.
    Component(
        u64,
        Box<extension_ui::Slot>,
        u64,
        Option<ri_core::extensions::RemoteComponent>,
    ),
}

/// A running status shown in the editor's top border.
#[derive(Clone, Debug)]
enum Indicator {
    Working,
    Retry {
        attempt: u32,
        max: u32,
        until: Instant,
    },
    Task(String),
}

/// What an open dialog's answer is for.
#[derive(Debug)]
enum Dialog {
    /// "Summarize branch?" for navigating to the entry.
    TreeSummary(String),
    /// Custom summary instructions for navigating to the entry.
    TreeInstructions(String),
    /// Resume a session whose working directory is gone.
    ResumeMissingCwd(PathBuf),
    /// Import a session file.
    Import(String),
    /// The `/login` method menu, for all providers or these, offering these kinds.
    LoginMenu(
        Option<Vec<login::ProviderOption>>,
        Vec<ri_ai::registry::LoginKind>,
    ),
    /// The `/login` provider selector, of one kind or all, with its search.
    LoginProviders(Option<ri_ai::registry::LoginKind>, String),
    /// The `/logout` provider selector.
    Logout,
    /// A select prompt of the running sign-in.
    LoginSelect,
    /// An extension's dialog.
    Extension(ExtensionReply),
}

/// Where an extension dialog's answer goes.
#[derive(Debug)]
enum ExtensionReply {
    Select(Vec<String>, tokio::sync::oneshot::Sender<Option<String>>),
    Confirm(tokio::sync::oneshot::Sender<bool>),
    Text(tokio::sync::oneshot::Sender<Option<String>>),
}

impl ExtensionReply {
    fn cancel(self) {
        match self {
            ExtensionReply::Select(_, reply) | ExtensionReply::Text(reply) => {
                let _ = reply.send(None);
            }
            ExtensionReply::Confirm(reply) => {
                let _ = reply.send(false);
            }
        }
    }
}

use crate::runtime::{SessionFactory, user_text};

/// What the run needs from startup.
pub struct Options {
    /// Fullscreen (`--tui-mode`) overrides the setting.
    pub tui_mode: Option<TuiMode>,
    /// Show every startup detail.
    pub verbose: bool,
    /// Messages to send at start, in order.
    pub initial: Vec<String>,
    /// Builds replacement sessions for `/new`, `/resume`, `/fork` and `/clone`.
    pub factory: SessionFactory,
    /// `--use-theme`: the theme for this run instead of the `theme` setting.
    pub use_theme: Option<String>,
}

fn true_color() -> bool {
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    match env("PI_TRUE_COLOR").as_str() {
        "1" => return true,
        "0" => return false,
        _ => {}
    }
    let colorterm = env("COLORTERM").to_lowercase();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return true;
    }
    let term = env("TERM");
    if term.ends_with("-direct") || term == "xterm-kitty" || term == "xterm-ghostty" {
        return true;
    }
    if term.starts_with("screen") || term.starts_with("tmux") || !env("TMUX").is_empty() {
        return false;
    }
    matches!(
        env("TERM_PROGRAM").as_str(),
        "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "WarpTerminal" | "zed" | "Alacritty"
    ) || !env("WT_SESSION").is_empty()
        || !env("KITTY_WINDOW_ID").is_empty()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn tool_result_of(message: &ri_types::message::ToolResultMessage) -> ToolResult {
    ToolResult {
        content: message.content.clone(),
        details: message.details.clone(),
        usage: message.usage.clone(),
        is_error: Some(message.is_error),
        ..ToolResult::default()
    }
}

/// The terminal modes the app owns, paused around external programs.
struct Terminal {
    #[cfg(unix)]
    raw: ri_tui::terminal::RawMode,
    protocol: KeyboardProtocol,
    stdin_paused: Arc<AtomicBool>,
}

/// The interactive application.
struct App {
    session: AgentSession,
    factory: SessionFactory,
    agent_dir: PathBuf,
    epoch: u64,
    theme: Theme,
    markdown: MarkdownTheme,
    keys: Keybindings,
    editor: Editor,
    selector: Option<Selector>,
    dialog: Option<Dialog>,
    chat: Vec<Item>,
    cache: Vec<Option<(usize, u64, Vec<StyledLine>)>>,
    flat: Vec<StyledLine>,
    flat_header: Vec<StyledLine>,
    flat_key: Option<(usize, usize)>,
    flat_offsets: Vec<usize>,
    footer_cache: Option<(usize, Vec<StyledLine>)>,
    generation: u64,
    streaming: Option<usize>,
    tool_items: HashMap<String, usize>,
    pending: (Vec<String>, Vec<String>),
    pending_bash: Vec<BashView>,
    next_bash: u64,
    compaction_queue: Vec<(String, bool)>,
    manual_compaction: bool,
    indicator: Option<(Indicator, Instant)>,
    expanded: bool,
    hide_thinking: bool,
    output_pad: usize,
    show_details: bool,
    fullscreen: bool,
    alt: AltScreen,
    main: MainScreen,
    size: (usize, usize),
    last_clear: Option<Instant>,
    last_escape: Option<Instant>,
    running: bool,
    quit: bool,
    exit_code: u8,
    cwd: PathBuf,
    home: Option<PathBuf>,
    branch: Option<String>,
    expand_key: String,
    cancel_key: String,
    fd: Option<PathBuf>,
    colors: ri_tui::terminal::TerminalColors,
    color_mode: ColorMode,
    kitty: bool,
    tx: UnboundedSender<Event>,
    login: Option<login::LoginRun>,
    next_login: u64,
    anthropic_warning_shown: bool,
    ext: extension_ui::ExtensionState,
    /// Providers with models available, as the footer last counted them.
    provider_count: usize,
    /// The current session's extensions wait to start, after the session
    /// they replace (if any) shuts down.
    binding: Option<Option<AgentSession>>,
    /// Messages to send once extensions have started.
    initial: Vec<String>,
    /// pi's `OverlayOptions` when the open custom component is an overlay.
    overlay: Option<Value>,
    /// The startup header shows (`quietStartup` is not `true`).
    show_header: bool,
    /// The session's registered theme files.
    theme_files: themes::ThemeFiles,
    /// `--use-theme`, which replaces the `theme` setting for this run.
    theme_override: Option<String>,
}

/// Writes to the terminal, ignoring errors from a vanished terminal.
fn emit(data: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(data.as_bytes());
    let _ = out.flush();
}

fn markdown_theme(theme: &Theme) -> MarkdownTheme {
    MarkdownTheme {
        heading: theme.fg("mdHeading"),
        link: theme.fg("mdLink"),
        link_url: theme.fg("mdLinkUrl"),
        code: theme.fg("mdCode"),
        code_block: theme.fg("mdCodeBlock"),
        code_block_border: theme.fg("mdCodeBlockBorder"),
        quote: theme.fg("mdQuote"),
        quote_border: theme.fg("mdQuoteBorder"),
        hr: theme.fg("mdHr"),
        list_bullet: theme.fg("mdListBullet"),
        code_block_indent: "  ".to_owned(),
    }
}

fn editor_theme(theme: &Theme) -> EditorTheme {
    EditorTheme {
        border: theme.fg("borderMuted"),
        select_list: SelectListTheme {
            selected_text: theme.fg("accent"),
            description: theme.fg("muted"),
            scroll_info: theme.fg("muted"),
            no_match: theme.fg("muted"),
        },
    }
}

/// The theme an export outside the terminal UI uses: the `theme` setting
/// when it names a theme, else the system theme, without terminal colors.
pub(crate) fn export_theme(setting: Option<&str>, agent_dir: &Path) -> (Theme, Appearance) {
    let appearance =
        detect_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()).unwrap_or(Appearance::Dark);
    let named = resolve_theme_setting(setting, appearance)
        .filter(|name| name != ri_tui::theme::SYSTEM_THEME_NAME)
        .and_then(|name| {
            let (theme, error) = load_theme(
                Some(&name),
                &themes::ThemeFiles::default(),
                agent_dir,
                &ri_tui::terminal::TerminalColors::default(),
                ColorMode::TrueColor,
            );
            error.is_none().then_some(theme)
        });
    (
        named.unwrap_or_else(|| crate::export_html::system_theme(appearance)),
        appearance,
    )
}

/// The configured theme as extensions see it outside the terminal UI, where
/// no terminal colors are known.
pub(crate) fn extension_theme(setting: Option<&str>, agent_dir: &Path) -> Value {
    let mode = if true_color() {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    };
    let (theme, _) = load_theme(
        setting,
        &themes::ThemeFiles::default(),
        agent_dir,
        &ri_tui::terminal::TerminalColors::default(),
        mode,
    );
    extension_ui::theme_json(&theme)
}

/// Picks the configured theme for the terminal's colors.
fn load_theme(
    setting: Option<&str>,
    files: &themes::ThemeFiles,
    agent_dir: &Path,
    colors: &ri_tui::terminal::TerminalColors,
    mode: ColorMode,
) -> (Theme, Option<String>) {
    let appearance = match colors.background {
        Some(background) => terminal_appearance(background, colors.foreground),
        None => {
            detect_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()).unwrap_or(Appearance::Dark)
        }
    };
    let system = || {
        Theme::system(
            &SystemThemeInput {
                foreground: colors.foreground,
                background: colors.background,
                palette: colors.palette.clone(),
                saturation: 1.0,
                appearance_hint: Some(appearance),
            },
            mode,
        )
    };
    let Some(name) = resolve_theme_setting(setting, appearance) else {
        return (system(), None);
    };
    if name == ri_tui::theme::SYSTEM_THEME_NAME {
        return (system(), None);
    }
    // pi's order: registered theme files, built-in themes, then the agent
    // directory's `themes`.
    let registered = files.path(&name).map(Path::to_path_buf);
    if registered.is_none()
        && let Some(theme) = Theme::builtin(&name, mode)
    {
        return (theme, None);
    }
    let path = registered.unwrap_or_else(|| agent_dir.join("themes").join(format!("{name}.json")));
    match std::fs::read_to_string(&path) {
        Ok(text) => match Theme::from_json(&path.display().to_string(), &text, mode) {
            Ok(theme) => (theme, None),
            Err(error) => (
                system(),
                Some(format!(
                    "Failed to load theme \"{name}\": {error}\nFell back to the system theme."
                )),
            ),
        },
        Err(_) => (
            system(),
            Some(format!(
                "Failed to load theme \"{name}\": Theme not found: {name}\nFell back to the system theme."
            )),
        ),
    }
}

/// Forwards a session's events to the loop, tagged with `epoch`.
fn subscribe(session: &AgentSession, tx: &UnboundedSender<Event>, epoch: u64) {
    let tx = tx.clone();
    session.subscribe(Box::new(move |event| {
        let _ = tx.send(Event::Agent(epoch, Box::new(event.clone())));
    }));
}

impl App {
    fn ctx(&self) -> RenderContext<'_> {
        RenderContext {
            theme: &self.theme,
            markdown: &self.markdown,
            expanded: self.expanded,
            hide_thinking: self.hide_thinking,
            output_pad: self.output_pad,
            expand_key: &self.expand_key,
            cancel_key: &self.cancel_key,
            home: self.home.as_deref().and_then(Path::to_str),
            thinking_label: self.ext.thinking_label.as_deref().unwrap_or("Thinking..."),
        }
    }

    fn push(&mut self, item: Item) -> usize {
        self.chat.push(item);
        self.cache.push(None);
        self.chat.len() - 1
    }

    fn touch(&mut self, index: usize) {
        if let Some(slot) = self.cache.get_mut(index) {
            *slot = None;
        }
    }

    fn invalidate_all(&mut self) {
        self.generation += 1;
    }

    fn status(&mut self, text: impl Into<String>) {
        let text = text.into();
        if let Some(Item::Status(previous)) = self.chat.last_mut() {
            *previous = text;
            let index = self.chat.len() - 1;
            self.touch(index);
            return;
        }
        self.push(Item::Status(text));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.push(Item::Error(text.into()));
    }

    fn warning(&mut self, text: impl Into<String>) {
        self.push(Item::Warning(text.into()));
    }

    /// A dim line with its own spacer, as pi's `ThemedText` notices.
    fn note(&mut self, text: impl Into<String>, style: &str) {
        let mut out = lines::spacer(1);
        out.extend(lines::text(
            &[lines::styled(text.into(), self.theme.fg(style))],
            self.size.0,
            1,
            0,
            None,
        ));
        self.push(Item::Lines(out));
    }

    fn clear_chat(&mut self) {
        self.chat.clear();
        self.cache.clear();
        self.streaming = None;
        self.tool_items.clear();
    }

    fn ui(&self) -> Ui<'_> {
        Ui {
            theme: &self.theme,
            keys: &self.keys,
        }
    }

    fn install_autocomplete(&mut self) {
        let provider = commands::autocomplete(
            &self.session,
            &self.agent_dir,
            self.fd.clone(),
            self.home.clone(),
        );
        self.editor.set_autocomplete(Box::new(provider));
    }

    /// Renders the current branch's messages, as when a session opens.
    fn render_history(&mut self) {
        let messages = self.session.messages();
        let mut history: Vec<String> = Vec::new();
        for message in &messages {
            match message {
                Message::User(user) => {
                    let text = user_text(&user.content);
                    history.push(text.clone());
                    if !text.trim().is_empty() {
                        self.push(Item::User(text));
                    }
                }
                Message::Assistant(assistant) => {
                    self.push(Item::Assistant(assistant.clone()));
                    for block in &assistant.content {
                        if let ContentBlock::ToolCall(call) = block {
                            let mut view = self
                                .new_tool_view(&call.name, Value::Object(call.arguments.clone()));
                            if matches!(
                                assistant.stop_reason,
                                StopReason::Aborted | StopReason::Error
                            ) {
                                view.is_error = true;
                                view.result = Some(error_result(&assistant_error(assistant)));
                            }
                            let index = self.push(Item::Tool(Box::new(view)));
                            self.tool_items.insert(call.id.clone(), index);
                            self.draw_tool(index);
                        }
                    }
                }
                Message::ToolResult(result) => {
                    if let Some(&index) = self.tool_items.get(&result.tool_call_id)
                        && let Some(view) = self.tool_view(index)
                    {
                        view.result = Some(tool_result_of(result));
                        view.is_error = result.is_error;
                        self.draw_tool(index);
                    }
                }
                Message::BashExecution(bash) => {
                    self.push(Item::Bash(Box::new(BashView::from_message(bash))));
                }
                Message::Custom(custom) => self.push_custom(custom.clone()),
                Message::CompactionSummary(summary) => {
                    self.push(Item::Compaction {
                        tokens_before: summary.tokens_before,
                        summary: summary.summary.clone(),
                    });
                }
                Message::BranchSummary(summary) => {
                    self.push(Item::BranchSummary(summary.summary.clone()));
                }
                _ => {}
            }
        }
        for text in history {
            self.editor.add_to_history(&text);
        }
        if !self.session.project_trusted() && ri_core::trust::requires_trust(&self.cwd) {
            let mut out = if self.chat.is_empty() {
                Vec::new()
            } else {
                lines::spacer(1)
            };
            out.extend(lines::text(
                &[lines::styled(
                    format!(
                        "This project is not trusted. Project {} resources and packages are ignored. Use /trust to save a trust decision, then restart ri.",
                        ri_core::config::PROJECT_DIR
                    ),
                    self.theme.fg("warning"),
                )],
                self.size.0,
                1,
                0,
                None,
            ));
            self.push(Item::Lines(out));
        }
        let compactions = self.session.with_session(|session| {
            session
                .entries()
                .filter(|entry| matches!(entry, ri_types::session::FileEntry::Compaction(_)))
                .count()
        });
        if compactions > 0 {
            self.status(format!(
                "Session compacted {compactions} time{}",
                if compactions == 1 { "" } else { "s" }
            ));
        }
    }

    // Rendering

    fn border_style(&self) -> ratatui_core::style::Style {
        if self.editor.text().trim_start().starts_with('!') {
            self.theme.fg("bashMode")
        } else {
            self.theme
                .thinking_border(self.session.thinking_level().as_str())
        }
    }

    fn indicator_spans(&self, border: ratatui_core::style::Style) -> Option<Vec<Span<'static>>> {
        let (indicator, started) = self.indicator.as_ref()?;
        let frame = SPINNER[(started.elapsed().as_millis() / SPINNER_INTERVAL.as_millis())
            as usize
            % SPINNER.len()];
        let (spinner, text, message) = match indicator {
            Indicator::Working => (
                border,
                border,
                self.ext
                    .working_message
                    .clone()
                    .unwrap_or_else(|| "Working".to_owned()),
            ),
            Indicator::Retry {
                attempt,
                max,
                until,
            } => {
                let seconds = until
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    .ceil() as u64;
                (
                    self.theme.fg("warning"),
                    self.theme.fg("muted"),
                    format!("Retrying ({attempt}/{max}) in {seconds}s... (escape to cancel)"),
                )
            }
            Indicator::Task(label) => (
                self.theme.fg("accent"),
                self.theme.fg("muted"),
                label.clone(),
            ),
        };
        Some(vec![
            Span::styled(frame.to_owned(), spinner),
            Span::raw(" "),
            Span::styled(message, text),
        ])
    }

    /// Brings the flattened transcript (header, resources and chat) up to date
    /// for `width`, re-rendering only items that changed or animate.
    fn refresh_transcript(&mut self, width: usize) {
        let mut header = if self.show_header {
            header::render(
                &self.theme,
                &self.keys,
                self.expanded,
                self.show_details,
                width,
            )
        } else {
            Vec::new()
        };
        if self.show_details {
            let extensions: Vec<_> = self
                .session
                .extensions()
                .iter()
                .map(|extension| extension.source())
                .filter(|source| source.source != "builtin")
                .collect();
            header.extend(header::listing(
                &self.theme,
                self.session.resources(),
                &extensions,
                &self.cwd,
                self.home.as_deref(),
                self.expanded,
                width,
            ));
        }
        // Diagnostics show even when the listing is quiet, as in pi.
        header.extend(header::theme_conflicts(
            &self.theme,
            &self.theme_files.diagnostics,
            self.home.as_deref(),
            width,
        ));
        let generation = self.generation;
        let reusable = self
            .flat_key
            .is_some_and(|(cached, items)| cached == width && items <= self.chat.len())
            && self.flat_header == header;
        // Rows before the first changed item are kept.
        let mut first_changed = match self.flat_key {
            Some((_, items)) if reusable => items,
            _ => 0,
        };
        for index in 0..self.chat.len() {
            let fresh =
                matches!(&self.cache[index], Some((w, g, _)) if *w == width && *g == generation);
            let animating = match &self.chat[index] {
                Item::Tool(view) => view.result.is_none() && view.started.is_some(),
                Item::Bash(view) => view.running(),
                _ => false,
            };
            if !fresh || animating {
                let lines = self.chat[index].render(width, index == 0, &self.ctx());
                self.cache[index] = Some((width, generation, lines));
                first_changed = first_changed.min(index);
            }
        }
        if reusable && first_changed == self.chat.len() && self.flat_offsets.len() == first_changed
        {
            return;
        }
        if reusable && first_changed > 0 && first_changed <= self.flat_offsets.len() {
            let keep = self
                .flat_offsets
                .get(first_changed)
                .copied()
                .unwrap_or(self.flat.len());
            self.flat.truncate(keep);
            self.flat_offsets.truncate(first_changed);
        } else {
            self.flat = header.clone();
            self.flat_offsets.clear();
        }
        for index in self.flat_offsets.len()..self.chat.len() {
            self.flat_offsets.push(self.flat.len());
            if let Some((_, _, lines)) = &self.cache[index] {
                self.flat.extend(lines.iter().cloned());
            }
        }
        self.flat_header = header;
        self.flat_key = Some((width, self.chat.len()));
    }

    /// The transcript rows at `width`.
    fn transcript(&mut self, width: usize) -> Vec<StyledLine> {
        self.refresh_transcript(width);
        self.flat.clone()
    }

    /// The dock rows and the cursor within them.
    fn dock(&mut self, width: usize) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let mut out = Vec::new();
        for view in &self.pending_bash {
            out.extend(view.render(width, &self.ctx()));
        }
        let dim = self.theme.fg("dim");
        let steering: Vec<String> = self
            .pending
            .0
            .iter()
            .cloned()
            .chain(
                self.compaction_queue
                    .iter()
                    .filter(|(_, follow_up)| !follow_up)
                    .map(|(text, _)| text.clone()),
            )
            .collect();
        let follow_up: Vec<String> = self
            .pending
            .1
            .iter()
            .cloned()
            .chain(
                self.compaction_queue
                    .iter()
                    .filter(|(_, follow_up)| *follow_up)
                    .map(|(text, _)| text.clone()),
            )
            .collect();
        if !steering.is_empty() || !follow_up.is_empty() {
            out.extend(lines::spacer(1));
            for text in steering {
                out.push(lines::truncated_text(
                    &lines::styled(format!("Steering: {text}"), dim),
                    width,
                    1,
                ));
            }
            for text in follow_up {
                out.push(lines::truncated_text(
                    &lines::styled(format!("Follow-up: {text}"), dim),
                    width,
                    1,
                ));
            }
            let key = keybindings::keys_display(&self.keys, "app.message.dequeue");
            out.push(lines::truncated_text(
                &lines::styled(format!("↳ {key} to edit all queued messages"), dim),
                width,
                1,
            ));
        }
        // pi's widget container above the editor: a spacer, then the widgets.
        out.extend(lines::spacer(1));
        for (_, widget) in &mut self.ext.above {
            out.extend(widget.render(width, &self.theme));
        }
        let cursor;
        // An overlay draws over the screen; the editor stays below it.
        let overlaid = self.overlay.is_some() && matches!(self.selector, Some(Selector::Remote(_)));
        if !overlaid && let Some(mut selector) = self.selector.take() {
            let (rows, at) = selector.render(width, &self.ui());
            self.selector = Some(selector);
            cursor = at.map(|(row, col)| (out.len() + row, col));
            out.extend(rows);
        } else {
            let border = self.border_style();
            self.editor.border = border;
            self.editor.set_terminal_rows(self.size.1);
            self.editor.focused = !overlaid;
            let mut editor = self.editor.render(width);
            if let Some(spans) = self.indicator_spans(border) {
                let status = Line::from(spans);
                let status = lines::truncate(&status, width.saturating_sub(5), "");
                let status_width = lines::width(&status);
                let mut row = vec![Span::styled("── ", border)];
                row.extend(status.spans);
                row.push(Span::styled(
                    format!(" {}", "─".repeat(width.saturating_sub(status_width + 4))),
                    border,
                ));
                editor[0] = Line::from(row);
            }
            // The focused overlay has the cursor.
            cursor = self
                .editor
                .cursor_position()
                .filter(|_| !overlaid)
                .map(|(row, col)| (out.len() + row, col));
            out.extend(editor);
        }
        for (_, widget) in &mut self.ext.below {
            out.extend(widget.render(width, &self.theme));
        }
        if !matches!(&self.footer_cache, Some((cached, _)) if *cached == width) {
            let lines = self.footer(width);
            self.footer_cache = Some((width, lines));
        }
        if let Some((_, footer)) = &self.footer_cache {
            out.extend(footer.iter().cloned());
        }
        (out, cursor)
    }

    /// The footer: an extension's, or pi's with extension statuses below.
    fn footer(&mut self, width: usize) -> Vec<StyledLine> {
        self.ext.mirror(
            self.editor.expanded_text(),
            self.branch.as_deref(),
            self.provider_count,
        );
        if let Some(view) = &mut self.ext.footer {
            return view.render(width).0;
        }
        let (mut lines, providers) = self.builtin_footer(width);
        self.provider_count = providers;
        lines.extend(self.ext.status_line(width, &self.theme));
        lines
    }

    /// pi's footer, and how many providers have models available.
    fn builtin_footer(&self, width: usize) -> (Vec<StyledLine>, usize) {
        let model = self.session.model();
        let cwd = footer::format_cwd(&self.cwd, self.home.as_deref());
        let name = self.session.with_session(|session| session.name());
        let providers: std::collections::HashSet<String> = self
            .session
            .models_in_scope()
            .into_iter()
            .map(|model| model.provider)
            .collect();
        let settings = self.session.settings();
        let auto_compact = settings
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.enabled)
            .unwrap_or(true);
        let lines = footer::render(
            &footer::FooterData {
                cwd: &cwd,
                branch: self.branch.as_deref(),
                session_name: name.as_deref(),
                totals: self.session.usage_totals(),
                context: self.session.context_usage(),
                context_window: model.as_ref().map_or(0, |model| model.context_window),
                auto_compact,
                model: model.as_ref().map(|model| model.id.as_str()),
                provider: model.as_ref().map(|model| model.provider.as_str()),
                reasoning: model.as_ref().is_some_and(|model| model.reasoning),
                thinking: self.session.thinking_level().as_str(),
                several_providers: providers.len() > 1,
                subscription: false,
            },
            &self.theme,
            width,
        );
        (lines, providers.len())
    }

    fn draw(&mut self) {
        self.expire_dialog();
        if !matches!(self.selector, Some(Selector::Remote(_))) {
            self.overlay = None;
        }
        let overlays = self.overlays();
        self.alt.overlays.clone_from(&overlays);
        self.main.overlays = overlays;
        if let Some(selector) = &mut self.selector {
            selector.tick();
        }
        let (width, height) = self.size;
        let (dock, cursor) = self.dock(width);
        self.refresh_transcript(width);
        let frame = if self.fullscreen {
            self.alt.frame(&self.flat, &dock, cursor, width, height)
        } else {
            let offset = self.flat.len();
            let mut document = self.flat.clone();
            document.extend(dock);
            self.main.frame(
                &document,
                cursor.map(|(row, col)| (offset + row, col)),
                width,
                height,
            )
        };
        emit(&frame);
    }

    /// Whether something on screen animates.
    fn animating(&self) -> bool {
        self.indicator.is_some()
            || self.chat.iter().any(|item| match item {
                Item::Tool(view) => view.result.is_none() && view.started.is_some(),
                Item::Bash(view) => view.running(),
                _ => false,
            })
            || self.pending_bash.iter().any(BashView::running)
            || matches!(&self.selector, Some(Selector::Session(selector)) if selector.has_timed_status())
            || self.countdown().is_some()
    }

    // Agent events

    fn streaming_message(&mut self) -> Option<&mut AssistantMessage> {
        let index = self.streaming?;
        match self.chat.get_mut(index) {
            Some(Item::Assistant(message)) => Some(message),
            _ => None,
        }
    }

    fn apply_delta(&mut self, event: AssistantMessageEvent) {
        let index = self.streaming;
        let mut new_tool: Option<(String, String)> = None;
        let mut finished_tool: Option<ToolCall> = None;
        if let Some(message) = self.streaming_message() {
            let content = &mut message.content;
            let ensure = |content: &mut Vec<ContentBlock>, at: usize, block: ContentBlock| {
                while content.len() <= at {
                    content.push(ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                }
                content[at] = block;
            };
            match event {
                AssistantMessageEvent::TextStart { content_index } => ensure(
                    content,
                    content_index,
                    ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                ),
                AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                } => {
                    if let Some(ContentBlock::Text(text)) = content.get_mut(content_index) {
                        text.text.push_str(&delta);
                    }
                }
                AssistantMessageEvent::TextEnd {
                    content_index,
                    content: text,
                } => {
                    if let Some(ContentBlock::Text(block)) = content.get_mut(content_index) {
                        block.text = text;
                    }
                }
                AssistantMessageEvent::ThinkingStart { content_index } => ensure(
                    content,
                    content_index,
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                ),
                AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                } => {
                    if let Some(ContentBlock::Thinking(thinking)) = content.get_mut(content_index) {
                        thinking.thinking.push_str(&delta);
                    }
                }
                AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: text,
                } => {
                    if let Some(ContentBlock::Thinking(block)) = content.get_mut(content_index) {
                        block.thinking = text;
                    }
                }
                AssistantMessageEvent::ToolcallStart {
                    content_index,
                    id,
                    tool_name,
                } => {
                    ensure(
                        content,
                        content_index,
                        ContentBlock::ToolCall(ToolCall {
                            id: id.clone(),
                            name: tool_name.clone(),
                            arguments: Map::new(),
                            thought_signature: None,
                            namespace: None,
                        }),
                    );
                    new_tool = Some((id, tool_name));
                }
                AssistantMessageEvent::ToolcallDelta { .. } => {}
                AssistantMessageEvent::ToolcallEnd {
                    content_index,
                    tool_call,
                } => {
                    ensure(
                        content,
                        content_index,
                        ContentBlock::ToolCall(tool_call.clone()),
                    );
                    finished_tool = Some(tool_call);
                }
            }
        }
        if let Some(index) = index {
            self.touch(index);
        }
        if let Some((id, name)) = new_tool
            && !self.tool_items.contains_key(&id)
        {
            let index = self.push(Item::Tool(Box::new(
                self.new_tool_view(&name, Value::Object(Map::new())),
            )));
            self.tool_items.insert(id, index);
            self.draw_tool(index);
        }
        if let Some(call) = finished_tool
            && let Some(&index) = self.tool_items.get(&call.id)
            && let Some(view) = self.tool_view(index)
        {
            view.args = Value::Object(call.arguments);
            self.draw_tool(index);
        }
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.running = true,
            // pi replaces any other indicator, such as a retry countdown.
            AgentEvent::TurnStart => {
                if !matches!(self.indicator, Some((Indicator::Working, _))) {
                    self.indicator = Some((Indicator::Working, Instant::now()));
                }
            }
            AgentEvent::MessageStart { message } => match message {
                Message::User(user) => {
                    let text = user_text(&user.content);
                    if !text.trim().is_empty() {
                        self.push(Item::User(text));
                    }
                }
                Message::Assistant(assistant) => {
                    let index = self.push(Item::Assistant(assistant));
                    self.streaming = Some(index);
                }
                Message::Custom(custom) => self.push_custom(custom),
                _ => {}
            },
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => self.apply_delta(assistant_message_event),
            AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } => {
                let failed = matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                );
                let error = assistant_error(&assistant);
                let ids: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    })
                    .collect();
                match self.streaming.take() {
                    Some(index) => {
                        self.chat[index] = Item::Assistant(assistant);
                        self.touch(index);
                    }
                    None => {
                        self.push(Item::Assistant(assistant));
                    }
                }
                if failed {
                    for id in ids {
                        if let Some(&index) = self.tool_items.get(&id)
                            && let Some(view) = self.tool_view(index)
                            && view.result.is_none()
                        {
                            view.result = Some(error_result(&error));
                            view.is_error = true;
                        }
                    }
                }
            }
            // Calls a tool made, such as codemode's nested calls, get no row.
            AgentEvent::ToolExecutionStart {
                parent_tool_call_id: Some(_),
                ..
            }
            | AgentEvent::ToolExecutionUpdate {
                parent_tool_call_id: Some(_),
                ..
            }
            | AgentEvent::ToolExecutionEnd {
                parent_tool_call_id: Some(_),
                ..
            } => {}
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
                ..
            } => {
                let index = match self.tool_items.get(&tool_call_id) {
                    Some(&index) => index,
                    None => {
                        let index = self.push(Item::Tool(Box::new(
                            self.new_tool_view(&tool_name, args.clone()),
                        )));
                        self.tool_items.insert(tool_call_id, index);
                        index
                    }
                };
                if let Some(view) = self.tool_view(index) {
                    view.args = args;
                    view.started = Some(Instant::now());
                }
                self.draw_tool(index);
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(&index) = self.tool_items.get(&tool_call_id)
                    && let Some(view) = self.tool_view(index)
                {
                    view.partial = Some(partial_result);
                    self.draw_tool(index);
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                if let Some(&index) = self.tool_items.get(&tool_call_id)
                    && let Some(view) = self.tool_view(index)
                {
                    view.result = Some(result);
                    view.is_error = is_error;
                    view.finished = Some(Instant::now());
                    self.draw_tool(index);
                }
            }
            AgentEvent::AgentEnd { .. } => {
                if matches!(self.indicator, Some((Indicator::Working, _))) {
                    self.indicator = None;
                }
                if let Some(index) = self.streaming.take() {
                    self.chat.remove(index);
                    self.cache.remove(index);
                    self.tool_items.retain(|_, item| *item != index);
                }
            }
            AgentEvent::QueueUpdate {
                steering,
                follow_up,
            } => self.pending = (steering, follow_up),
            AgentEvent::CompactionStart { reason } => {
                let label = match reason {
                    CompactionReason::Manual => "Compacting context... (escape to cancel)",
                    CompactionReason::Overflow => {
                        "Context overflow detected, Auto-compacting... (escape to cancel)"
                    }
                    _ => "Auto-compacting... (escape to cancel)",
                };
                self.manual_compaction = reason == CompactionReason::Manual;
                self.indicator = Some((Indicator::Task(label.to_owned()), Instant::now()));
            }
            AgentEvent::CompactionEnd {
                reason,
                result,
                aborted,
                error_message,
                ..
            } => {
                self.indicator = None;
                let manual = reason == CompactionReason::Manual;
                self.manual_compaction = false;
                if aborted {
                    if manual {
                        self.error("Compaction cancelled");
                    } else {
                        self.status("Auto-compaction cancelled");
                    }
                } else if let Some(result) = result {
                    self.clear_chat();
                    self.render_history_without_summary();
                    self.push(Item::Compaction {
                        tokens_before: result.tokens_before,
                        summary: result.summary,
                    });
                } else if let Some(message) = error_message {
                    if manual {
                        self.error(message);
                    } else {
                        self.note(message, "error");
                    }
                }
                self.flush_compaction_queue();
            }
            AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => {
                self.indicator = Some((
                    Indicator::Retry {
                        attempt,
                        max: max_attempts,
                        until: Instant::now() + Duration::from_millis(delay_ms),
                    },
                    Instant::now(),
                ));
            }
            AgentEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => {
                self.indicator = None;
                if !success {
                    self.error(format!(
                        "Retry failed after {attempt} attempts: {}",
                        final_error.as_deref().unwrap_or("Unknown error")
                    ));
                }
            }
            _ => {}
        }
    }

    fn render_history_without_summary(&mut self) {
        self.render_history();
        if let Some(position) = self
            .chat
            .iter()
            .position(|item| matches!(item, Item::Compaction { .. }))
        {
            self.chat.remove(position);
            self.cache.remove(position);
            let shift: Vec<(String, usize)> = self
                .tool_items
                .iter()
                .map(|(id, index)| {
                    (
                        id.clone(),
                        if *index > position { index - 1 } else { *index },
                    )
                })
                .collect();
            self.tool_items = shift.into_iter().collect();
        }
        if let Some(Item::Status(_)) = self.chat.last() {
            self.chat.pop();
            self.cache.pop();
        }
    }

    /// Sends the messages typed during a manual compaction.
    fn flush_compaction_queue(&mut self) {
        let queue = std::mem::take(&mut self.compaction_queue);
        let mut queue = queue.into_iter();
        let Some((first, _)) = queue.next() else {
            return;
        };
        if self.running {
            self.session.steer(&first, Vec::new());
        } else {
            self.start_prompt(first);
        }
        for (text, follow_up) in queue {
            if follow_up {
                self.session.follow_up(&text, Vec::new());
            } else {
                self.session.steer(&text, Vec::new());
            }
        }
    }

    // Input

    fn start_prompt(&mut self, text: String) {
        self.running = true;
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let result = session.prompt(&text, Vec::new()).await;
            let _ = tx.send(Event::PromptDone(epoch, result));
        });
    }

    fn on_submit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.run_builtin(&text) {
            return;
        }
        if let Some(command) = text.strip_prefix('!') {
            let (exclude, command) = match command.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, command.trim()),
            };
            if !command.is_empty() {
                if self.session.is_bash_running() {
                    self.warning(
                        "A bash command is already running. Press Esc to cancel it first.",
                    );
                    self.editor.set_text(&text);
                    return;
                }
                self.editor.add_to_history(&text);
                self.run_bash(command.to_owned(), exclude);
                return;
            }
        }
        if self.session.is_extension_command(&text) {
            // Extension commands run at once, even while a response streams.
            self.editor.add_to_history(&text);
            let session = self.session.clone();
            let tx = self.tx.clone();
            let epoch = self.epoch;
            tokio::spawn(async move {
                if let Err(error) = session.prompt(&text, Vec::new()).await {
                    let _ = tx.send(Event::Notify(epoch, error, NotifyKind::Error));
                }
            });
            return;
        }
        if self.manual_compaction {
            self.editor.add_to_history(&text);
            self.compaction_queue.push((text, false));
            self.status("Queued message for after compaction");
            return;
        }
        self.editor.add_to_history(&text);
        if self.running {
            self.session.steer(&text, Vec::new());
            return;
        }
        for view in std::mem::take(&mut self.pending_bash) {
            self.push(Item::Bash(Box::new(view)));
        }
        self.start_prompt(text);
    }

    fn run_bash(&mut self, command: String, exclude: bool) {
        self.next_bash += 1;
        let id = self.next_bash;
        let view = BashView::new(id, &command, exclude);
        if self.running {
            self.pending_bash.push(view);
        } else {
            self.push(Item::Bash(Box::new(view)));
        }
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let chunks = tx.clone();
            let result = session
                .execute_bash(&command, Some(exclude), None, move |chunk| {
                    let _ = chunks.send(Event::BashChunk(epoch, id, chunk.to_owned()));
                })
                .await;
            let _ = tx.send(Event::BashDone(epoch, id, Box::new(result)));
        });
    }

    /// The tool view at `index`, marked for re-rendering.
    /// A view for a call to `name`, which knows whether the session has
    /// such a tool.
    fn new_tool_view(&self, name: &str, args: Value) -> ToolView {
        let mut view = ToolView::new(name, args);
        view.known = self
            .session
            .tools()
            .all()
            .iter()
            .any(|tool| tool.name == name);
        view
    }

    fn tool_view(&mut self, index: usize) -> Option<&mut ToolView> {
        self.touch(index);
        match self.chat.get_mut(index) {
            Some(Item::Tool(view)) => Some(view),
            _ => None,
        }
    }

    /// The `!` command view with `id`, marked for re-rendering.
    fn bash_view(&mut self, id: u64) -> Option<&mut BashView> {
        if let Some(position) = self.pending_bash.iter().position(|view| view.id == id) {
            return self.pending_bash.get_mut(position);
        }
        let index = self
            .chat
            .iter()
            .position(|item| matches!(item, Item::Bash(view) if view.id == id))?;
        self.touch(index);
        match self.chat.get_mut(index) {
            Some(Item::Bash(view)) => Some(view.as_mut()),
            _ => None,
        }
    }

    fn restore_queue(&mut self, abort: bool) -> usize {
        let (mut queued, follow_up) = self.session.clear_queues();
        queued.extend(follow_up);
        queued.extend(
            std::mem::take(&mut self.compaction_queue)
                .into_iter()
                .map(|(text, _)| text),
        );
        if !queued.is_empty() {
            let current = self.editor.text();
            let mut text = queued.join("\n\n");
            if !current.trim().is_empty() {
                text = format!("{text}\n\n{current}");
            }
            self.editor.set_text(&text);
        }
        if abort {
            self.session.abort();
        }
        queued.len()
    }

    fn on_escape(&mut self) {
        if self.running {
            self.restore_queue(true);
            return;
        }
        if matches!(self.indicator, Some((Indicator::Task(_), _))) {
            self.session.abort();
            return;
        }
        if self.session.is_bash_running() {
            self.session.abort_bash();
            return;
        }
        if self.editor.text().trim_start().starts_with('!') {
            self.editor.set_text("");
            return;
        }
        let action = self
            .session
            .settings()
            .double_escape_action
            .unwrap_or(DoubleEscapeAction::Tree);
        if self.editor.text().trim().is_empty() && action != DoubleEscapeAction::None {
            if self
                .last_escape
                .is_some_and(|at| at.elapsed() < DOUBLE_PRESS)
            {
                self.last_escape = None;
                if action == DoubleEscapeAction::Tree {
                    self.open_tree(None);
                } else {
                    self.open_fork();
                }
            } else {
                self.last_escape = Some(Instant::now());
            }
        }
    }

    fn handle_key(&mut self, data: &str, terminal: &mut Terminal) {
        if self.dispatch_key(data, terminal) {
            self.footer_cache = None;
        }
    }

    /// Handles a key; whether it may have changed more than the editor.
    fn dispatch_key(&mut self, data: &str, terminal: &mut Terminal) -> bool {
        if self.fullscreen && self.handle_viewport_key(data) {
            return true;
        }
        if self.selector.is_some() {
            self.handle_selector_key(data);
            return true;
        }
        let keys = &self.keys;
        if keys.matches(data, "app.interrupt") && !self.editor.is_showing_autocomplete() {
            self.on_escape();
            return true;
        }
        if keys.matches(data, "app.exit") && self.editor.text().is_empty() {
            self.quit = true;
            return true;
        }
        if !(keys.matches(data, "tui.editor.historyPrevious")
            || keys.matches(data, "tui.editor.historyNext"))
        {
            if keys.matches(data, "app.clear") {
                if self
                    .last_clear
                    .is_some_and(|at| at.elapsed() < DOUBLE_PRESS)
                {
                    self.quit = true;
                } else {
                    self.editor.set_text("");
                    self.last_clear = Some(Instant::now());
                }
                return true;
            }
            if keys.matches(data, "app.suspend") {
                self.suspend(terminal);
                return true;
            }
            if keys.matches(data, "app.thinking.cycle") {
                match self.session.cycle_thinking_level() {
                    Some(level) => self.status(format!("Thinking level: {}", level.as_str())),
                    None => self.status("Current model does not support thinking"),
                }
                return true;
            }
            if keys.matches(data, "app.model.cycleForward")
                || keys.matches(data, "app.model.cycleBackward")
            {
                let forward = keys.matches(data, "app.model.cycleForward");
                self.cycle_model(forward);
                return true;
            }
            if keys.matches(data, "app.model.select") {
                self.open_model_selector("");
                return true;
            }
            if keys.matches(data, "app.tools.expand") {
                self.toggle_tools();
                return true;
            }
            if keys.matches(data, "app.thinking.toggle") {
                self.hide_thinking = !self.hide_thinking;
                let _ = self
                    .session
                    .set_global_setting("hideThinkingBlock", Some(Value::Bool(self.hide_thinking)));
                self.invalidate_all();
                self.status(if self.hide_thinking {
                    "Thinking blocks: hidden"
                } else {
                    "Thinking blocks: visible"
                });
                return true;
            }
            if keys.matches(data, "app.editor.external") {
                self.external_editor(terminal);
                return true;
            }
            if keys.matches(data, "app.message.copy") {
                self.copy_last();
                return true;
            }
            if keys.matches(data, "app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_owned();
                if text.is_empty() {
                    return true;
                }
                if self.manual_compaction {
                    self.editor.add_to_history(&text);
                    self.editor.set_text("");
                    self.compaction_queue.push((text, true));
                    self.status("Queued message for after compaction");
                } else if self.running {
                    self.editor.add_to_history(&text);
                    self.editor.set_text("");
                    self.session.follow_up(&text, Vec::new());
                } else {
                    self.editor.set_text("");
                    self.on_submit(text);
                }
                return true;
            }
            if keys.matches(data, "app.message.dequeue") {
                let count = self.restore_queue(false);
                if count == 0 {
                    self.status("No queued messages to restore");
                } else {
                    self.status(format!(
                        "Restored {count} queued message{} to editor",
                        if count > 1 { "s" } else { "" }
                    ));
                }
                return true;
            }
            if keys.matches(data, "app.session.new") {
                self.new_session();
                return true;
            }
            if keys.matches(data, "app.session.tree") {
                self.open_tree(None);
                return true;
            }
            if keys.matches(data, "app.session.fork") {
                self.open_fork();
                return true;
            }
            if keys.matches(data, "app.session.resume") {
                self.open_resume();
                return true;
            }
        }
        match self.editor.handle_input(data, &self.keys) {
            EditorEvent::Submit(text) => {
                self.on_submit(text);
                true
            }
            EditorEvent::None => false,
        }
    }

    fn toggle_tools(&mut self) {
        self.expanded = !self.expanded;
        self.ext.set_tools_expanded(self.expanded);
        self.redraw_transcript();
        self.invalidate_all();
        self.status(if self.expanded {
            "Tool output: expanded"
        } else {
            "Tool output: collapsed"
        });
    }

    fn copy_last(&mut self) {
        let Some(text) = self.session.last_assistant_text() else {
            self.error("No agent messages to copy yet.");
            return;
        };
        match clipboard::copy(&text, emit) {
            Ok(()) => self.status("Copied last agent message to clipboard"),
            Err(error) => self.error(error),
        }
    }

    fn cycle_model(&mut self, forward: bool) {
        let Some((model, _)) = self.session.cycle_model(forward) else {
            self.status(if self.session.scoped_models().is_empty() {
                "Only one model available"
            } else {
                "Only one model in scope"
            });
            return;
        };
        let name = if model.name.is_empty() {
            model.id.clone()
        } else {
            model.name.clone()
        };
        let level = self.session.thinking_level();
        if model.reasoning && level.as_str() != "off" {
            self.status(format!("Switched to {name} (thinking: {})", level.as_str()));
        } else {
            self.status(format!("Switched to {name}"));
        }
        self.warn_anthropic_subscription(Some(&model));
    }

    /// Fullscreen scrolling; returns whether the key was consumed.
    fn handle_viewport_key(&mut self, data: &str) -> bool {
        if let Some(rest) = data.strip_prefix("\x1b[<") {
            // SGR mouse: wheel up 64, wheel down 65.
            let button: u32 = rest
                .split(';')
                .next()
                .and_then(|b| b.parse().ok())
                .unwrap_or(0);
            match button & !0b11100 {
                64 => self.alt.scroll_by(-1),
                65 => self.alt.scroll_by(1),
                _ => {}
            }
            return true;
        }
        let keys = &self.keys;
        if keys.matches(data, "tui.altScreen.pageUp") {
            self.alt.page(-1);
        } else if keys.matches(data, "tui.altScreen.pageDown") {
            self.alt.page(1);
        } else if keys.matches(data, "tui.altScreen.top") {
            self.alt.top();
        } else if keys.matches(data, "tui.altScreen.bottom") {
            self.alt.bottom();
        } else {
            return false;
        }
        true
    }

    // Selectors

    fn handle_selector_key(&mut self, data: &str) {
        let Some(mut selector) = self.selector.take() else {
            return;
        };
        let outcome = selector.handle_input(data, &self.ui());
        match outcome {
            Outcome::None => self.selector = Some(selector),
            Outcome::Side(action) => {
                self.selector = Some(selector);
                self.act(action);
            }
            Outcome::Cancel => {
                let dialog = self.dialog.take();
                self.on_cancel(dialog);
            }
            Outcome::Done(action) => self.act(action),
        }
    }

    fn on_cancel(&mut self, dialog: Option<Dialog>) {
        match dialog {
            Some(Dialog::TreeSummary(id)) => self.open_tree(Some(id)),
            Some(Dialog::TreeInstructions(id)) => self.ask_tree_summary(id),
            Some(Dialog::ResumeMissingCwd(_)) => self.status("Resume cancelled"),
            Some(Dialog::Import(_)) => self.status("Import cancelled"),
            Some(Dialog::LoginMenu(..) | Dialog::Logout) => {}
            Some(Dialog::LoginProviders(kind, _)) => self.providers_cancelled(kind),
            Some(Dialog::LoginSelect) => self.login_select_done(None),
            Some(Dialog::Extension(reply)) => reply.cancel(),
            None => {}
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Model { model, default } => {
                let id = model.id.clone();
                let provider = model.provider.clone();
                if let Err(error) = self.session.set_model((*model).clone()) {
                    self.error(error);
                    return;
                }
                if default {
                    self.session.save_default_model(&model);
                    self.status(format!("Default model: {provider}/{id}"));
                } else {
                    self.status(format!("Model: {id}"));
                }
                self.warn_anthropic_subscription(None);
            }
            Action::Thinking { level, default } => {
                self.session.set_thinking_level(level);
                if default {
                    let _ = self.session.set_global_setting(
                        "defaultThinkingLevel",
                        Some(Value::String(level.as_str().to_owned())),
                    );
                    self.status(format!("Default thinking level: {}", level.as_str()));
                } else {
                    self.status(format!("Thinking level: {}", level.as_str()));
                }
            }
            Action::Fork(id) => self.fork(&id, false),
            Action::Resume(path) => self.resume(&path, None),
            Action::Tree(id) => self.tree_selected(id),
            Action::Label { id, label } => {
                let _ = self
                    .session
                    .with_session(|session| session.append_label(&id, label));
            }
            Action::Copy(text) => match text {
                None => self.error("Selected entry has no text to copy"),
                Some(text) => match clipboard::copy(&text, emit) {
                    Ok(()) => self.status("Copied selected message to clipboard"),
                    Err(error) => self.error(error),
                },
            },
            Action::ToggleTools => self.toggle_tools(),
            Action::Provider(option) => self.provider_chosen(*option),
            Action::LoginCancelled => self.login_cancelled(),
            Action::Choice(index) => self.on_choice(index),
            Action::Text(text) => self.on_text(text),
            Action::Trust(option) => {
                let saved =
                    ri_core::trust::TrustStore::new(&self.agent_dir).set_many(&option.updates);
                match saved {
                    Ok(()) => self.status(format!(
                        "Saved trust decision: {}. Restart ri for this to take effect.",
                        if option.trusted {
                            "trusted"
                        } else {
                            "untrusted"
                        }
                    )),
                    Err(error) => self.error(format!("Failed to save trust decision: {error}")),
                }
            }
        }
    }

    fn on_choice(&mut self, index: usize) {
        match self.dialog.take() {
            Some(Dialog::TreeSummary(id)) => match index {
                0 => self.navigate(id, false, None),
                1 => self.navigate(id, true, None),
                _ => {
                    self.dialog = Some(Dialog::TreeInstructions(id));
                    self.selector = Some(Selector::Text(Box::new(TextDialog::new(
                        "Custom summarization instructions",
                        editor_theme(&self.theme),
                    ))));
                }
            },
            Some(Dialog::Import(path)) => {
                if index == 0 {
                    self.import(&path);
                } else {
                    self.status("Import cancelled");
                }
            }
            Some(Dialog::LoginMenu(options, kinds)) => {
                if let Some(kind) = kinds.get(index) {
                    self.login_menu_chosen(options, *kind);
                }
            }
            Some(Dialog::LoginSelect) => self.login_select_done(Some(index)),
            Some(Dialog::ResumeMissingCwd(path)) => {
                if index == 0 {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                    self.resume(&path, Some(cwd));
                } else {
                    self.status("Resume cancelled");
                }
            }
            Some(Dialog::Extension(ExtensionReply::Select(options, reply))) => {
                let _ = reply.send(options.get(index).cloned());
            }
            Some(Dialog::Extension(ExtensionReply::Confirm(reply))) => {
                let _ = reply.send(index == 0);
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: String) {
        match self.dialog.take() {
            Some(Dialog::TreeInstructions(id)) => self.navigate(id, true, Some(text)),
            Some(Dialog::Extension(ExtensionReply::Text(reply))) => {
                let _ = reply.send(Some(text));
            }
            _ => {}
        }
    }

    fn open_model_selector(&mut self, search: &str) {
        let settings = self.session.settings();
        let default = settings
            .default_provider
            .clone()
            .zip(settings.default_model.clone());
        self.selector = Some(Selector::Model(Box::new(selectors::ModelSelector::new(
            self.session.available_models(),
            self.session.model(),
            default,
            search,
        ))));
    }

    fn open_thinking_selector(&mut self) {
        let theme = self.ui().select_list_theme();
        self.selector = Some(Selector::Thinking(Box::new(
            selectors::ThinkingSelector::new(
                self.session.thinking_level(),
                &self.session.available_thinking_levels(),
                Some(
                    self.session
                        .settings()
                        .default_thinking_level
                        .unwrap_or(ri_core::model_resolver::DEFAULT_THINKING_LEVEL),
                ),
                theme,
            ),
        )));
    }

    fn open_fork(&mut self) {
        let messages = self.session.user_messages_for_forking();
        if messages.is_empty() {
            self.status("No messages to fork from");
            return;
        }
        self.selector = Some(Selector::Fork(selectors::ForkSelector::new(messages)));
    }

    fn open_tree(&mut self, initial: Option<String>) {
        let (tree, leaf) = self
            .session
            .with_session(|session| (session.tree(), session.leaf_id().map(str::to_owned)));
        if tree.roots.is_empty() {
            self.status("No entries in session");
            return;
        }
        let filter = self
            .session
            .settings()
            .tree_filter_mode
            .unwrap_or(ri_types::settings::TreeFilterMode::Default);
        self.selector = Some(Selector::Tree(Box::new(tree_selector::TreeSelector::new(
            tree,
            leaf,
            self.size.1,
            initial,
            filter,
            self.home
                .as_deref()
                .and_then(Path::to_str)
                .map(str::to_owned),
        ))));
    }

    fn open_resume(&mut self) {
        let (dir, file) = self.session.with_session(|session| {
            (
                session.dir().to_path_buf(),
                session.file().map(Path::to_path_buf),
            )
        });
        let default_dir = ri_core::config::default_session_dir(&self.agent_dir, &self.cwd);
        let custom = (dir != default_dir).then_some(dir);
        let sources = session_sources(&self.agent_dir, &self.cwd, custom);
        self.selector = Some(Selector::Session(Box::new(
            session_selector::SessionSelector::new(
                sources,
                file,
                self.home
                    .as_deref()
                    .and_then(Path::to_str)
                    .map(str::to_owned),
                true,
            ),
        )));
    }

    fn ask_tree_summary(&mut self, id: String) {
        self.dialog = Some(Dialog::TreeSummary(id));
        self.selector = Some(Selector::Choice(ChoiceDialog::new(
            "Summarize branch?",
            &["No summary", "Summarize", "Summarize with custom prompt"],
        )));
    }

    fn tree_selected(&mut self, id: String) {
        let leaf = self
            .session
            .with_session(|session| session.leaf_id().map(str::to_owned));
        if leaf.as_deref() == Some(id.as_str()) {
            self.status("Already at this point");
            return;
        }
        let skip = self
            .session
            .settings()
            .branch_summary
            .as_ref()
            .and_then(|settings| settings.skip_prompt)
            .unwrap_or(false);
        if skip {
            self.navigate(id, false, None);
        } else {
            self.ask_tree_summary(id);
        }
    }

    fn navigate(&mut self, id: String, summarize: bool, instructions: Option<String>) {
        if self.running {
            self.restore_queue(false);
            self.session.abort();
        }
        if matches!(self.indicator, Some((Indicator::Task(_), _))) {
            self.error(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree.",
            );
            return;
        }
        if summarize {
            self.push(Item::Lines(lines::spacer(1)));
            self.indicator = Some((
                Indicator::Task(format!(
                    "Summarizing branch... ({} to cancel)",
                    keybindings::keys_text(&self.keys, "app.interrupt")
                )),
                Instant::now(),
            ));
        }
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            // A run being aborted settles before the tree can move.
            while session.is_streaming() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let result = session
                .navigate_tree(
                    &id,
                    TreeNavigation {
                        summarize,
                        custom_instructions: instructions,
                        ..TreeNavigation::default()
                    },
                )
                .await;
            let _ = tx.send(Event::TreeDone(epoch, id, Box::new(result)));
        });
    }

    fn tree_done(&mut self, id: String, result: Result<TreeOutcome, String>) {
        if matches!(self.indicator, Some((Indicator::Task(_), _))) {
            self.indicator = None;
        }
        match result {
            Ok(outcome) if outcome.aborted => {
                self.status("Branch summarization cancelled");
                self.open_tree(Some(id));
            }
            Ok(outcome) if outcome.cancelled => self.status("Navigation cancelled"),
            Ok(outcome) => {
                self.clear_chat();
                self.render_history();
                if let Some(text) = outcome.editor_text
                    && self.editor.text().trim().is_empty()
                {
                    self.editor.set_text(&text);
                }
                self.status("Navigated to selected point");
            }
            Err(error) => self.error(error),
        }
    }

    // Sessions

    /// Builds a session around `manager` and shows it in place of the current
    /// one.
    fn replace_session(&mut self, manager: SessionManager) -> Result<(), String> {
        let session = (self.factory)(manager).map_err(|error| error.to_string())?;
        self.session.abort();
        self.session.abort_bash();
        self.epoch += 1;
        let old = std::mem::replace(&mut self.session, session);
        subscribe(&self.session, &self.tx, self.epoch);
        self.reset_extension_ui();
        self.binding = Some(Some(old));
        self.cwd = self.session.cwd().to_path_buf();
        self.theme_files = themes::ThemeFiles::load(&self.session.resources().themes);
        self.branch = footer::git_branch(&self.cwd);
        self.running = false;
        self.indicator = None;
        self.pending = (Vec::new(), Vec::new());
        self.pending_bash.clear();
        self.compaction_queue.clear();
        self.manual_compaction = false;
        self.clear_chat();
        self.install_autocomplete();
        self.render_history();
        Ok(())
    }

    fn new_session(&mut self) {
        self.indicator = None;
        let result = crate::runtime::new_session(&self.session, None)
            .map_err(|error| error.to_string())
            .and_then(|manager| self.replace_session(manager));
        match result {
            Ok(()) => {
                let mut out = lines::spacer(1);
                out.extend(lines::text(
                    &[lines::styled(
                        "✓ New session started",
                        self.theme.fg("accent"),
                    )],
                    self.size.0,
                    1,
                    1,
                    None,
                ));
                self.push(Item::Lines(out));
            }
            Err(error) => self.fatal("Failed to create session", &error),
        }
    }

    /// pi's `runtimeHost.fork`: `at` keeps the entry (`/clone`), otherwise the
    /// branch ends before the user message (`/fork`).
    fn fork(&mut self, id: &str, at: bool) {
        let result = crate::runtime::plan_fork(&self.session, id, at).and_then(|fork| {
            let manager = fork.build(&self.session)?;
            self.replace_session(manager)?;
            Ok(fork.text)
        });
        match result {
            Ok(text) => {
                if at {
                    self.editor.set_text("");
                    self.status("Cloned to new session");
                } else {
                    self.editor.set_text(text.as_deref().unwrap_or_default());
                    self.status("Forked to new session");
                }
            }
            Err(error) => self.error(error),
        }
    }

    fn resume(&mut self, path: &Path, cwd_override: Option<PathBuf>) {
        let fallback = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let manager = match crate::runtime::open_session(path, cwd_override.as_deref(), &fallback) {
            Ok(manager) => manager,
            Err(crate::runtime::SwitchError::MissingCwd {
                session_cwd,
                fallback,
                ..
            }) => {
                self.dialog = Some(Dialog::ResumeMissingCwd(path.to_path_buf()));
                self.selector = Some(Selector::Choice(ChoiceDialog::new(
                    &format!(
                        "Session cwd not found\ncwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
                        session_cwd.display(),
                        fallback.display()
                    ),
                    &["Yes", "No"],
                )));
                return;
            }
            Err(error) => {
                self.fatal("Failed to resume session", &error.to_string());
                return;
            }
        };
        match self.replace_session(manager) {
            Ok(()) if cwd_override.is_some() => self.status("Resumed session in current cwd"),
            Ok(()) => self.status("Resumed session"),
            Err(error) => self.fatal("Failed to resume session", &error),
        }
    }

    /// pi's fatal runtime error: shown, then the app exits with status 1.
    fn fatal(&mut self, prefix: &str, error: &str) {
        self.error(format!("{prefix}: {error}"));
        self.quit = true;
        self.exit_code = 1;
    }

    // Terminal handoff

    /// Gives the terminal to another program and takes it back.
    fn with_terminal_released(&mut self, terminal: &mut Terminal, run: impl FnOnce()) {
        terminal.stdin_paused.store(true, Ordering::SeqCst);
        let mut out = String::new();
        if self.fullscreen {
            out.push_str(ALT_SCREEN_LEAVE);
        } else {
            out.push_str(&self.main.stop());
        }
        out.push_str(BRACKETED_PASTE_DISABLE);
        out.push_str(&terminal.protocol.disable());
        out.push_str("\x1b[?25h");
        emit(&out);
        #[cfg(unix)]
        terminal.raw.restore();
        run();
        #[cfg(unix)]
        let _ = terminal.raw.reenable();
        let mut out = String::from(BRACKETED_PASTE_ENABLE);
        out.push_str(terminal.protocol.query());
        if self.fullscreen {
            out.push_str(ALT_SCREEN_ENTER);
            self.alt.invalidate();
        } else {
            self.main.reset();
        }
        emit(&out);
        terminal.stdin_paused.store(false, Ordering::SeqCst);
        self.size = ri_tui::terminal::size();
        self.invalidate_all();
    }

    fn suspend(&mut self, terminal: &mut Terminal) {
        if cfg!(windows) {
            self.status("Suspend to background is not supported on Windows");
            return;
        }
        self.with_terminal_released(terminal, || {
            #[cfg(unix)]
            ri_tui::terminal::suspend();
        });
    }

    fn external_editor(&mut self, terminal: &mut Terminal) {
        let configured = self
            .session
            .settings()
            .external_editor
            .filter(|command| !command.trim().is_empty());
        let command = configured
            .or_else(|| std::env::var("VISUAL").ok().filter(|v| !v.is_empty()))
            .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "nano" }.to_owned());
        let content = self.editor.expanded_text();
        let dir = std::env::temp_dir().join(format!(
            "ri-editor-{}",
            ri_core::time::uuid_v4().split('-').next().unwrap_or("0")
        ));
        let file = dir.join("prompt.md");
        if std::fs::create_dir_all(&dir).is_err() || std::fs::write(&file, &content).is_err() {
            self.error("Failed to open external editor");
            return;
        }
        let mut edited: Option<String> = None;
        self.with_terminal_released(terminal, || {
            emit(&format!(
                "Launching external editor: {command}\nri will resume when the editor exits.\n"
            ));
            let mut parts = command.split(' ').filter(|part| !part.is_empty());
            if let Some(program) = parts.next() {
                let status = std::process::Command::new(program)
                    .args(parts)
                    .arg(&file)
                    .status();
                if status.is_ok_and(|status| status.success())
                    && let Ok(text) = std::fs::read_to_string(&file)
                {
                    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                    edited = Some(text.strip_suffix('\n').unwrap_or(text).to_owned());
                }
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(text) = edited {
            self.editor.set_text(&text);
        }
    }
}

fn assistant_error(message: &AssistantMessage) -> String {
    match message.stop_reason {
        StopReason::Aborted => chat::aborted_message(message),
        _ => message
            .error_message
            .clone()
            .unwrap_or_else(|| "Error".to_owned()),
    }
}

fn error_result(text: &str) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        is_error: Some(true),
        ..ToolResult::default()
    }
}

/// Reads stdin on a thread. While `paused`, input is left for another program.
fn spawn_stdin(tx: UnboundedSender<Event>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 4096];
        loop {
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(STDIN_POLL);
                continue;
            }
            #[cfg(unix)]
            if !ri_tui::terminal::stdin_ready(STDIN_POLL) {
                continue;
            }
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Event::InputClosed);
                    return;
                }
                Ok(count) => {
                    if tx.send(Event::Input(buffer[..count].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// Where `/resume` finds sessions: `custom` when sessions live in a shared
/// directory, else the project's directory under the agent directory.
pub fn session_sources(
    agent_dir: &Path,
    cwd: &Path,
    custom: Option<PathBuf>,
) -> session_selector::Sources {
    session_selector::Sources {
        dir: custom
            .clone()
            .unwrap_or_else(|| ri_core::config::default_session_dir(agent_dir, cwd)),
        cwd_filter: custom.as_ref().map(|_| cwd.to_path_buf()),
        custom,
        root: agent_dir.join("sessions"),
    }
}

/// Shell-quotes a path for the resume hint when needed.
fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./~:@".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Runs interactive mode until the user quits. Returns the exit code.
pub async fn run(session: AgentSession, agent_dir: PathBuf, options: Options) -> u8 {
    #[cfg(unix)]
    let raw = match ri_tui::terminal::RawMode::enable() {
        Ok(raw) => raw,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "Error: cannot use the terminal: {error}");
            return 1;
        }
    };
    let (tx, mut rx): (UnboundedSender<Event>, UnboundedReceiver<Event>) = unbounded_channel();
    let stdin_paused = Arc::new(AtomicBool::new(false));
    spawn_stdin(tx.clone(), stdin_paused.clone());
    subscribe(&session, &tx, 0);
    {
        let tx = tx.clone();
        let bin_dir = ri_core::config::bin_dir(&agent_dir);
        tokio::spawn(async move {
            let fd = ri_core::tools::external::ensure_tool(
                ri_core::tools::external::ExternalTool::Fd,
                &bin_dir,
            )
            .await;
            let _ = tx.send(Event::Fd(fd));
        });
    }

    let mut terminal = Terminal {
        #[cfg(unix)]
        raw,
        protocol: KeyboardProtocol::default(),
        stdin_paused,
    };
    let mut query = ColorQuery::new();
    emit(&format!(
        "{BRACKETED_PASTE_ENABLE}{}{}",
        terminal.protocol.query(),
        color_query()
    ));

    // Wait briefly for the color replies; keys typed meanwhile are kept.
    let mut buffer = InputBuffer::new();
    let mut early: Vec<String> = Vec::new();
    let mut early_events: Vec<Event> = Vec::new();
    let deadline = Instant::now() + COLOR_QUERY_TIMEOUT;
    while !query.is_done() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(Event::Input(bytes))) => {
                let mut write = String::new();
                for input in buffer.push(&bytes) {
                    if let Input::Key(sequence) = input {
                        if let Filtered::Forward(keys) =
                            terminal.protocol.filter(&sequence, &mut write)
                        {
                            for key in keys {
                                if !query.consume(&key) {
                                    early.push(key);
                                }
                            }
                        }
                    } else if let Input::Paste(text) = input {
                        early.push(format!("\x1b[200~{text}\x1b[201~"));
                    }
                }
                if !write.is_empty() {
                    emit(&write);
                }
            }
            Ok(Some(event)) => early_events.push(event),
            Ok(None) | Err(_) => break,
        }
    }

    let mode = if true_color() {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    };
    let theme_files = themes::ThemeFiles::load(&session.resources().themes);
    let theme_override = options.use_theme.clone();
    let (theme, theme_error) = load_theme(
        theme_override
            .as_deref()
            .or(session.settings().theme.as_deref()),
        &theme_files,
        &agent_dir,
        &query.colors(),
        mode,
    );
    let mut keys = keybindings::load(&agent_dir, Keys::detect(terminal.protocol.kitty));
    keys.set_kitty(terminal.protocol.kitty);
    let settings = session.settings();
    let fullscreen = options.tui_mode.or(settings.tui_mode) != Some(TuiMode::Regular);
    let quiet = settings.quiet_startup.as_ref();
    let show_details = options.verbose
        || !matches!(
            quiet,
            Some(ri_types::settings::BoolOr::Bool(true))
                | Some(ri_types::settings::BoolOr::Other(_))
        );
    // `quietStartup: true` hides the header too; `"header"` only the details.
    let show_header =
        options.verbose || !matches!(quiet, Some(ri_types::settings::BoolOr::Bool(true)));
    let mut editor = Editor::new(
        editor_theme(&theme),
        usize::from(settings.editor_padding_x.unwrap_or(0).min(3)),
        usize::from(settings.autocomplete_max_visible.unwrap_or(5)),
    );
    editor.focused = true;
    let cwd = session.cwd().to_path_buf();
    let expand_key = keybindings::keys_text(&keys, "app.tools.expand");
    let cancel_key = keybindings::keys_text(&keys, "tui.select.cancel");
    let mut app = App {
        markdown: markdown_theme(&theme),
        theme,
        keys,
        editor,
        selector: None,
        dialog: None,
        chat: Vec::new(),
        cache: Vec::new(),
        flat: Vec::new(),
        flat_header: Vec::new(),
        flat_key: None,
        flat_offsets: Vec::new(),
        footer_cache: None,
        generation: 0,
        streaming: None,
        tool_items: HashMap::new(),
        pending: (Vec::new(), Vec::new()),
        pending_bash: Vec::new(),
        next_bash: 0,
        compaction_queue: Vec::new(),
        manual_compaction: false,
        indicator: None,
        expanded: options.verbose,
        hide_thinking: settings.hide_thinking_block.unwrap_or(false),
        output_pad: usize::from(settings.output_pad.unwrap_or(1).min(1)),
        show_details,
        fullscreen,
        alt: AltScreen::new(),
        main: MainScreen::new(),
        size: ri_tui::terminal::size(),
        last_clear: None,
        last_escape: None,
        running: false,
        quit: false,
        exit_code: 0,
        branch: footer::git_branch(&cwd),
        cwd,
        home: home_dir(),
        expand_key,
        cancel_key,
        fd: None,
        colors: query.colors(),
        color_mode: mode,
        kitty: terminal.protocol.kitty,
        tx: tx.clone(),
        session: session.clone(),
        factory: options.factory,
        agent_dir: agent_dir.clone(),
        epoch: 0,
        login: None,
        next_login: 0,
        anthropic_warning_shown: false,
        ext: extension_ui::ExtensionState::default(),
        initial: options.initial,
        provider_count: 0,
        binding: None,
        overlay: None,
        show_header,
        theme_files,
        theme_override,
    };
    app.alt.jump_label_style = app.theme.bg("selectedBg").patch(app.theme.fg("text"));
    app.alt.bottom_key = keybindings::keys_display(&app.keys, "tui.altScreen.bottom");
    app.main.show_hardware_cursor = settings.show_hardware_cursor.unwrap_or(false);
    app.alt.show_hardware_cursor = app.main.show_hardware_cursor;
    app.main.clear_on_shrink = settings
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.clear_on_shrink)
        .unwrap_or(false);
    let scoped = app.session.scoped_models();
    if !scoped.is_empty() && show_details {
        let list: Vec<String> = scoped
            .iter()
            .map(|entry| match entry.thinking_level {
                Some(level) => format!("{}:{}", entry.model.id, level.as_str()),
                None => entry.model.id.clone(),
            })
            .collect();
        let keys = keybindings::keys_display(&app.keys, "app.model.cycleForward");
        let dim = ri_tui::ansi::sgr(app.theme.fg("dim"));
        let hint = if keys.is_empty() {
            String::new()
        } else {
            format!(
                "{} ({keys} to cycle)\x1b[39m{dim}",
                ri_tui::ansi::sgr(app.theme.fg("muted"))
            )
        };
        emit(&format!(
            "{dim}Model scope: {}{hint}\x1b[39m\r\n",
            list.join(", ")
        ));
    }
    if fullscreen {
        emit(ALT_SCREEN_ENTER);
    }
    app.install_autocomplete();
    app.render_history();
    for error in app.session.settings_errors() {
        app.warning(error);
    }
    if let Some(error) = theme_error {
        app.error(error);
    }
    for event in early_events {
        app.on_event(event, &mut terminal);
    }
    for key in early {
        app.handle_key(&key, &mut terminal);
    }
    app.warn_anthropic_subscription(None);
    app.ext.set_tools_expanded(app.expanded);
    app.binding = Some(None);
    app.start_binding();
    app.draw();

    #[cfg(unix)]
    let mut resize =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    #[cfg(unix)]
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    let escape_wait = escape_timeout(|name| std::env::var(name).ok());
    let mut last_draw = Instant::now();
    let mut dirty = false;
    while !app.quit {
        let tick = if app.animating() {
            SPINNER_INTERVAL
        } else {
            Duration::from_secs(3600)
        };
        let input_wait = buffer
            .timeout(escape_wait)
            .unwrap_or(Duration::from_secs(3600));
        #[cfg(unix)]
        let resized = async {
            match resize.as_mut() {
                Some(signal) => signal.recv().await,
                None => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let resized = std::future::pending::<Option<()>>();
        // SIGTERM, or SIGHUP when the terminal goes away.
        #[cfg(unix)]
        let terminated = async {
            let term = async {
                match terminate.as_mut() {
                    Some(signal) => signal.recv().await,
                    None => std::future::pending().await,
                }
            };
            let hup = async {
                match hangup.as_mut() {
                    Some(signal) => signal.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                received = term => received,
                received = hup => received,
            }
        };
        #[cfg(not(unix))]
        let terminated = std::future::pending::<Option<()>>();
        let flush_wait = if dirty {
            FRAME_INTERVAL.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                if let Event::Input(bytes) = event {
                    let mut write = String::new();
                    for input in buffer.push(&bytes) {
                        match input {
                            Input::Key(sequence) => {
                                if let Filtered::Forward(keys) = terminal.protocol.filter(&sequence, &mut write) {
                                    for key in keys {
                                        if ri_tui::keys::is_key_release(&key) {
                                            continue;
                                        }
                                        app.handle_key(&key, &mut terminal);
                                    }
                                }
                            }
                            Input::Paste(text) => app.handle_key(&format!("\x1b[200~{text}\x1b[201~"), &mut terminal),
                        }
                    }
                    if !write.is_empty() {
                        emit(&write);
                    }
                    app.keys.set_kitty(terminal.protocol.kitty);
                    app.kitty = terminal.protocol.kitty;
                } else {
                    app.on_event(event, &mut terminal);
                }
                dirty = true;
            }
            _ = tokio::time::sleep(input_wait) => {
                for input in buffer.flush() {
                    if let Input::Key(key) = input {
                        app.handle_key(&key, &mut terminal);
                    }
                }
                if let Some(pending) = terminal.protocol.flush() {
                    app.handle_key(&pending, &mut terminal);
                }
                dirty = true;
            }
            _ = tokio::time::sleep(tick) => dirty = true,
            _ = resized => {
                app.size = ri_tui::terminal::size();
                app.invalidate_all();
                dirty = true;
            }
            _ = terminated => app.quit = true,
            _ = tokio::time::sleep(flush_wait), if dirty => {}
        }
        app.start_binding();
        if dirty && last_draw.elapsed() >= FRAME_INTERVAL {
            app.draw();
            last_draw = Instant::now();
            dirty = false;
        }
    }

    // Teardown: leave the screen as pi does and restore the terminal.
    app.session.abort();
    app.session.abort_bash();
    ri_core::tools::bash::kill_tracked_children();
    app.indicator = None;
    app.selector = None;
    let mut out = String::new();
    if app.fullscreen {
        out.push_str(ALT_SCREEN_LEAVE);
        let (width, height) = app.size;
        let mut document = app.transcript(width);
        let (dock, _) = app.dock(width);
        document.extend(dock);
        let mut main = MainScreen::new();
        out.push_str(&main.frame(&document, None, width, height.max(document.len())));
        out.push_str(&main.stop());
    } else {
        app.draw();
        out.push_str(&app.main.stop());
    }
    out.push_str(BRACKETED_PASTE_DISABLE);
    out.push_str(&terminal.protocol.disable());
    out.push_str("\x1b[?25h");
    emit(&out);
    #[cfg(unix)]
    terminal.raw.restore();
    app.session.shutdown().await;

    let file = app
        .session
        .with_session(|session| session.file().map(Path::to_path_buf));
    let id = app
        .session
        .with_session(|session| session.header().map(|header| header.id.clone()));
    if app.exit_code == 0
        && std::io::stdout().is_terminal()
        && let (Some(file), Some(id)) = (file, id)
        && file.exists()
    {
        let default_dir = ri_core::config::default_session_dir(&agent_dir, &app.cwd);
        let command = match file.parent() {
            Some(dir) if dir != default_dir => {
                format!(
                    "ri --session-dir {} --session {id}",
                    quote(&dir.display().to_string())
                )
            }
            _ => format!("ri --session {id}"),
        };
        // chalk's dim, not the theme's.
        emit(&format!(
            "\x1b[2mTo resume this session:\x1b[22m {command}\n"
        ));
    }
    app.exit_code
}

impl App {
    /// Starts the current session's extensions once the event that replaced
    /// the session is handled, so they see the app's state (the theme) as it
    /// ends. Handlers may wait for dialogs, so they run beside the loop.
    fn start_binding(&mut self) {
        let Some(old) = self.binding.take() else {
            return;
        };
        self.ext.set_theme(&self.theme);
        let ui = extension_ui::InteractiveUi {
            tx: self.tx.clone(),
            epoch: self.epoch,
            shared: self.ext.shared.clone(),
        };
        let session = self.session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(old) = old {
                old.shutdown().await;
            }
            session.bind_extensions(Arc::new(ui), Mode::Tui).await;
            let _ = tx.send(Event::Bound);
        });
    }

    /// Handles everything but input.
    fn on_event(&mut self, event: Event, _terminal: &mut Terminal) {
        self.footer_cache = None;
        match event {
            Event::Input(_) => {}
            Event::InputClosed => self.quit = true,
            Event::Agent(epoch, event) if epoch == self.epoch => self.on_agent_event(*event),
            Event::PromptDone(epoch, result) if epoch == self.epoch => {
                self.running = false;
                if matches!(self.indicator, Some((Indicator::Working, _))) {
                    self.indicator = None;
                }
                if let Err(error) = result {
                    self.error(if error.is_empty() {
                        "Unknown error occurred".to_owned()
                    } else {
                        error
                    });
                }
            }
            Event::BashChunk(epoch, id, chunk) if epoch == self.epoch => {
                if let Some(view) = self.bash_view(id) {
                    view.append(&chunk);
                }
            }
            Event::BashDone(epoch, id, result) if epoch == self.epoch => match *result {
                Ok(result) => {
                    if let Some(view) = self.bash_view(id) {
                        view.finish(result);
                    }
                }
                Err(error) => {
                    if let Some(view) = self.bash_view(id) {
                        view.finish(BashResult {
                            cancelled: true,
                            ..BashResult::default()
                        });
                    }
                    self.error(format!("Bash command failed: {error}"));
                }
            },
            Event::TreeDone(epoch, id, result) if epoch == self.epoch => {
                self.tree_done(id, *result)
            }
            Event::Fd(path) => {
                self.fd = path;
                self.install_autocomplete();
            }
            Event::Auth(id, request) => self.on_auth_request(id, request),
            Event::LoginDone(id, result) => self.on_login_done(id, result),
            Event::LogoutDone(option, result) => self.on_logout_done(&option, result),
            Event::Notify(epoch, message, kind) if epoch == self.epoch => match kind {
                NotifyKind::Error => self.error(message),
                NotifyKind::Warning => self.warning(message),
                NotifyKind::Info => self.status(message),
            },
            Event::Ui(epoch, request) if epoch == self.epoch => self.on_ui_request(*request),
            Event::Bound => {
                // Items shown before the extensions started get their components.
                self.redraw_transcript();
                let mut initial = std::mem::take(&mut self.initial).into_iter();
                if let Some(first) = initial.next() {
                    self.start_prompt(first);
                    for message in initial {
                        self.session.follow_up(&message, Vec::new());
                    }
                }
            }
            Event::Rendered(epoch, key, width, lines) if epoch == self.epoch => {
                self.on_rendered(key, width, &lines);
            }
            Event::Component(epoch, slot, sequence, component) if epoch == self.epoch => {
                self.on_component(*slot, sequence, component);
            }
            _ => {}
        }
    }
}
