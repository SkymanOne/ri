//! Interactive mode: the full-screen or inline terminal UI.
//!
//! Port of `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! in pi `v1.0.0`, on pi-tui's line model: every component renders styled
//! lines for a width, and a renderer writes the frame.

mod bash_view;
mod catalogs;
mod chat;
mod clipboard;
mod commands;
mod completions;
pub mod config_selector;
mod extension_ui;
mod footer;
mod header;
pub mod keybindings;
mod login;
pub mod picker;
mod scoped_models;
mod selectors;
mod session_selector;
mod settings_selector;
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
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use yapi_core::agent_session::{AgentSession, Replacement, TreeNavigation, TreeOutcome, UserBash};
use yapi_core::bash_executor::BashResult;
use yapi_core::extensions::BashOperations;
use yapi_core::extensions::{Mode, NotifyKind};
use yapi_core::session::SessionManager;
use yapi_tui::autocomplete::AutocompleteProvider;
use yapi_tui::color::ColorMode;
use yapi_tui::editor::{Editor, EditorEvent, EditorTheme};
use yapi_tui::input::{Input, InputBuffer, escape_timeout};
use yapi_tui::keybindings::Keybindings;
use yapi_tui::keys::Keys;
use yapi_tui::lines::{self, StyledLine};
use yapi_tui::markdown::MarkdownTheme;
use yapi_tui::screen::{ALT_SCREEN_ENTER, ALT_SCREEN_LEAVE, AltScreen, MainScreen};
use yapi_tui::terminal::{
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, ColorQuery, Filtered, KeyboardProtocol,
    color_query,
};
use yapi_tui::theme::{
    Appearance, SystemThemeInput, Theme, detect_colorfgbg, resolve_theme_setting,
    terminal_appearance,
};
use yapi_types::event::{AgentEvent, AssistantMessageEvent, CompactionReason, ToolResult};
use yapi_types::message::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, ToolCall,
};
use yapi_types::rpc::StreamingBehavior;
use yapi_types::settings::{DoubleEscapeAction, TuiMode};

use self::bash_view::BashView;
use self::chat::{Item, RenderContext};
use self::selectors::{Action, ChoiceDialog, Outcome, Selector, TextDialog, Ui};
use self::tools::ToolView;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
/// pi-tui's OSC 9;4 sequences: indeterminate progress, and none.
const PROGRESS_ACTIVE: &str = "\x1b]9;4;3\x07";
const PROGRESS_CLEAR: &str = "\x1b]9;4;0\x07";
/// How often pi-tui repeats the active sequence, which some terminals expire.
const PROGRESS_KEEPALIVE: Duration = Duration::from_secs(1);
const DOUBLE_PRESS: Duration = Duration::from_millis(500);
const COLOR_QUERY_TIMEOUT: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STDIN_POLL: Duration = Duration::from_millis(50);
/// The app's actions that keys trigger from the editor, in the order pi
/// registers their handlers.
const APP_KEYS: &[&str] = &[
    "app.clear",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "app.message.dequeue",
    "app.session.new",
    "app.session.tree",
    "app.session.fork",
    "app.session.resume",
];
/// Fullscreen scrolling keys, in the order pi-tui checks them.
const VIEWPORT_KEYS: &[&str] = &[
    "tui.altScreen.pageUp",
    "tui.altScreen.pageDown",
    "tui.altScreen.halfPageUp",
    "tui.altScreen.halfPageDown",
    "tui.altScreen.lineUp",
    "tui.altScreen.lineDown",
    "tui.altScreen.previousPrompt",
    "tui.altScreen.nextPrompt",
    "tui.altScreen.top",
    "tui.altScreen.bottom",
];

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
    /// An extension error line and the error's stack.
    ExtensionError(u64, String, Option<String>),
    /// Extension completions arrived; the editor asks again.
    RefreshCompletions(u64),
    /// A request from the sign-in with this id.
    Auth(u64, yapi_ai::auth::AuthRequest),
    /// The sign-in with this id finished.
    LoginDone(u64, Result<(), yapi_ai::auth::AuthError>),
    LogoutDone(
        Box<login::ProviderOption>,
        Result<(), yapi_ai::auth::AuthError>,
    ),
    /// A request from an extension of the session with this epoch.
    Ui(u64, Box<extension_ui::Request>),
    /// An app action an extension's editor asks for, by key binding id.
    EditorAction(u64, String),
    /// Keys that passed the extensions' `onTerminalInput` listeners.
    TerminalInput(Vec<String>),
    /// Lines of the extension component with this key, rendered at a width.
    Rendered(u64, (u64, u32), usize, Vec<String>),
    /// The first session's extensions started.
    Bound,
    /// A model catalog refresh finished.
    Catalogs(
        Box<catalogs::Refresh>,
        yapi_core::agent_session::CatalogRefresh,
    ),
    /// Work to finish on the loop for the session with this epoch.
    Then(u64, Box<dyn FnOnce(&mut App) + Send>),
    /// A component an extension built for a transcript item, answering the
    /// request with this sequence number.
    Component(
        u64,
        Box<extension_ui::Slot>,
        u64,
        Option<yapi_core::extensions::RemoteComponent>,
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
        Vec<yapi_ai::registry::LoginKind>,
        Option<login::ProviderOption>,
    ),
    /// The `/login` provider selector, of one kind or all, with its search.
    LoginProviders(Option<yapi_ai::registry::LoginKind>, String),
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

use crate::runtime::SessionFactory;

/// What the run needs from startup.
pub struct Options {
    /// Fullscreen (`--tui-mode`) overrides the setting.
    pub tui_mode: Option<TuiMode>,
    /// Show every startup detail.
    pub verbose: bool,
    /// Messages to send at start, in order.
    pub initial: Vec<String>,
    /// Images attached to the first of `initial`.
    pub initial_images: Vec<ImageContent>,
    /// Builds replacement sessions for `/new`, `/resume`, `/fork` and `/clone`.
    pub factory: SessionFactory,
    /// `--use-theme`: the theme for this run instead of the `theme` setting.
    pub use_theme: Option<String>,
    /// Why the startup model differs from the session's, or that there is none.
    pub model_fallback: Option<String>,
    /// Whether model catalogs may be fetched; pi's `PI_OFFLINE` unset.
    pub model_network: bool,
}

/// The color mode for this terminal: truecolor when it advertises it.
fn color_mode() -> ColorMode {
    if true_color() {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    }
}

/// The terminal's appearance from its reported background, else from
/// `COLORFGBG`, else dark.
fn appearance(colors: &yapi_tui::terminal::TerminalColors) -> Appearance {
    match colors.background {
        Some(background) => terminal_appearance(background, colors.foreground),
        None => {
            detect_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()).unwrap_or(Appearance::Dark)
        }
    }
}

/// The sequences that take the terminal: bracketed paste on and the keyboard
/// protocol and color queries.
fn terminal_enter(protocol: &mut KeyboardProtocol) -> String {
    format!(
        "{BRACKETED_PASTE_ENABLE}{}{}",
        protocol.query(),
        color_query()
    )
}

/// The sequences that hand the terminal back: bracketed paste and the
/// keyboard protocol off, the cursor shown.
fn terminal_leave(protocol: &mut KeyboardProtocol) -> String {
    format!("{BRACKETED_PASTE_DISABLE}{}\x1b[?25h", protocol.disable())
}

/// Decodes terminal input into keys and bracketed pastes, in order, and
/// writes back the keyboard protocol's replies.
fn decode_input(
    buffer: &mut InputBuffer,
    protocol: &mut KeyboardProtocol,
    bytes: &[u8],
) -> Vec<String> {
    let mut write = String::new();
    let mut keys = Vec::new();
    for input in buffer.push(bytes) {
        match input {
            Input::Key(sequence) => {
                if let Filtered::Forward(forward) = protocol.filter(&sequence, &mut write) {
                    keys.extend(forward);
                }
            }
            Input::Paste(text) => keys.push(format!("\x1b[200~{text}\x1b[201~")),
        }
    }
    if !write.is_empty() {
        emit(&write);
    }
    keys
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

fn tool_result_of(message: &yapi_types::message::ToolResultMessage) -> ToolResult {
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
    raw: yapi_tui::terminal::RawMode,
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
    /// When the terminal progress sequence was last written, while active.
    progress: Option<Instant>,
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
    colors: yapi_tui::terminal::TerminalColors,
    color_mode: ColorMode,
    kitty: bool,
    tx: UnboundedSender<Event>,
    login: Option<login::LoginRun>,
    next_login: u64,
    anthropic_warning_shown: bool,
    ext: extension_ui::ExtensionState,
    /// Providers with models available, as the footer last counted them.
    provider_count: usize,
    /// Whether catalog refreshes may fetch: no `--offline` and no `PI_OFFLINE`.
    model_network: bool,
    /// The last selector catalog refresh id.
    next_refresh: u64,
    /// The current session's extensions wait to start, after the session
    /// they replace (if any) shuts down for a reason, with the replacement's
    /// session file.
    binding: Option<Option<(AgentSession, Replacement, Option<String>)>>,
    /// Messages to send once extensions have started.
    initial: Vec<String>,
    /// Images attached to the first of `initial`.
    initial_images: Vec<ImageContent>,
    /// pi's `OverlayOptions` when the open custom component is an overlay.
    overlay: Option<Value>,
    /// Overlays an open overlay covers, bottom first, with their options;
    /// each gets the keys again once those above it close, as in pi-tui.
    overlays_below: Vec<(extension_ui::RemoteView, Value)>,
    /// The startup header shows (`quietStartup` is not `true`).
    show_header: bool,
    /// The session's registered theme files.
    theme_files: themes::ThemeFiles,
    /// `--use-theme`, which replaces the `theme` setting for this run.
    theme_override: Option<String>,
    /// Keys the session's extensions bind.
    shortcuts: Vec<yapi_core::extensions::ShortcutBinding>,
    /// An extension asked to exit once the agent settles.
    shutdown_requested: bool,
    /// Keys waiting for extensions' `onTerminalInput` listeners, and whether
    /// the listeners are running.
    input_queue: Vec<String>,
    listening: bool,
    /// The built-in completion provider, for extensions' requests, and the
    /// request waiting for its answer.
    builtin_completions: Option<yapi_tui::autocomplete::CombinedProvider>,
    waiting_suggestions: Option<(Value, tokio::sync::oneshot::Sender<Value>)>,
    /// pi's `[Extension issues]`: the extension each concerns, and what.
    extension_issues: Vec<(yapi_types::rpc::SourceInfo, String)>,
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
        select_list: selectors::select_list_theme(theme),
    }
}

/// The theme an export outside the terminal UI uses: the `theme` setting
/// when it names a theme, else the system theme, without terminal colors.
pub(crate) fn export_theme(setting: Option<&str>, agent_dir: &Path) -> (Theme, Appearance) {
    let appearance = appearance(&yapi_tui::terminal::TerminalColors::default());
    let named = resolve_theme_setting(setting, appearance)
        .filter(|name| name != yapi_tui::theme::SYSTEM_THEME_NAME)
        .and_then(|name| {
            let (theme, error) = load_theme(
                Some(&name),
                &themes::ThemeFiles::default(),
                agent_dir,
                &yapi_tui::terminal::TerminalColors::default(),
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
    let (theme, _) = load_theme(
        setting,
        &themes::ThemeFiles::default(),
        agent_dir,
        &yapi_tui::terminal::TerminalColors::default(),
        color_mode(),
    );
    extension_ui::theme_json(&theme)
}

/// Picks the configured theme for the terminal's colors.
fn load_theme(
    setting: Option<&str>,
    files: &themes::ThemeFiles,
    agent_dir: &Path,
    colors: &yapi_tui::terminal::TerminalColors,
    mode: ColorMode,
) -> (Theme, Option<String>) {
    let appearance = appearance(colors);
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
    if name == yapi_tui::theme::SYSTEM_THEME_NAME {
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
        // pi names the theme in its errors when it loads the active one.
        Ok(text) => match Theme::from_json(&name, &text, mode) {
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

    /// A line with its own spacer in theme color `style`, as pi's
    /// `ThemedText` notices: re-wrapped and recolored when drawn.
    fn note(&mut self, text: impl Into<String>, style: &str) {
        let (text, style) = (text.into(), style.to_owned());
        self.push(Item::Render(Box::new(move |width, ctx| {
            let mut out = lines::spacer(1);
            out.extend(lines::text_row(
                lines::styled(text.clone(), ctx.theme.fg(&style)),
                width,
                1,
            ));
            out
        })));
    }

    /// pi's `showExtensionError`: the message in the error color without a
    /// spacer, then the stack's frames dim and indented.
    fn extension_error(&mut self, message: String, stack: Option<&str>) {
        let line = lines::styled(message, self.theme.fg("error"));
        self.text_item(vec![line], false, (1, 0));
        // V8 stacks start with the message, which pi skips; QuickJS's start
        // with the first frame. The runtime's own frames are left out.
        let frames: Vec<String> = stack
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("at ") && !line.contains("(eval_script:"))
            .map(|line| format!("  {line}"))
            .collect();
        if !frames.is_empty() {
            let dim = self.theme.fg("dim");
            let frames = frames
                .into_iter()
                .map(|frame| lines::styled(frame, dim))
                .collect();
            self.text_item(frames, false, (1, 0));
        }
    }

    /// Text that wraps at the current width, as pi's `Text` components: a
    /// spacer when `spacer`, then `content` with padding.
    fn text_item(&mut self, content: Vec<StyledLine>, spacer: bool, padding: (usize, usize)) {
        self.push(Item::Render(Box::new(move |width, _| {
            let mut out = if spacer { lines::spacer(1) } else { Vec::new() };
            out.extend(lines::text(&content, width, padding.0, padding.1, None));
            out
        })));
    }

    fn clear_chat(&mut self) {
        self.chat.clear();
        self.cache.clear();
        self.streaming = None;
        self.tool_items.clear();
    }

    /// Removes the item at `index`; the indices of later items shift down.
    fn remove_item(&mut self, index: usize) {
        self.chat.remove(index);
        self.cache.remove(index);
        self.tool_items.retain(|_, item| *item != index);
        let shift = |item: usize| if item > index { item - 1 } else { item };
        for item in self.tool_items.values_mut() {
            *item = shift(*item);
        }
        self.streaming = self.streaming.filter(|item| *item != index).map(shift);
        // The flattened rows still hold the removed item.
        self.flat_key = None;
    }

    fn ui(&self) -> Ui<'_> {
        Ui {
            theme: &self.theme,
            keys: &self.keys,
        }
    }

    fn install_autocomplete(&mut self) {
        self.install_completions();
        self.install_shortcuts();
    }

    /// The editor's completion: the built-in provider, or the one the
    /// extensions' providers compose over it, as pi's
    /// `setupAutocompleteProvider` installs it. The built-in provider also
    /// answers the extensions' requests.
    fn install_completions(&mut self) {
        let (tx, epoch) = (self.tx.clone(), self.epoch);
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(Event::RefreshCompletions(epoch));
        });
        let builtin = || {
            commands::autocomplete(&self.session, self.fd.clone(), self.home.clone())
                .notify_with(notify.clone())
        };
        let provider: Box<dyn AutocompleteProvider> = match &self.ext.completions {
            Some((providers, triggers)) => Box::new(completions::ExtensionCompletions::new(
                providers.clone(),
                triggers.clone(),
                notify.clone(),
            )),
            None => Box::new(builtin()),
        };
        self.builtin_completions = Some(builtin());
        self.editor.set_autocomplete(provider);
        self.answer_suggestions();
    }

    /// Answers an extension's request for the built-in provider's
    /// suggestions, whose `@` search may answer later.
    fn suggest(&mut self, request: Value, reply: tokio::sync::oneshot::Sender<Value>) {
        // As pi aborts a superseded request, only the latest one waits.
        if let Some((_, previous)) = self.waiting_suggestions.replace((request, reply)) {
            let _ = previous.send(Value::Null);
        }
        self.answer_suggestions();
    }

    /// Answers the waiting request once the built-in provider has its answer.
    fn answer_suggestions(&mut self) {
        let (Some(builtin), Some((request, _))) =
            (&self.builtin_completions, &self.waiting_suggestions)
        else {
            return;
        };
        let suggestions = match completions::request(request) {
            Some((at, force)) => {
                let suggestions =
                    builtin.suggestions(&at.lines, at.cursor_line, at.cursor_col, force);
                if builtin.pending() {
                    return;
                }
                suggestions
            }
            None => None,
        };
        if let Some((_, reply)) = self.waiting_suggestions.take() {
            let _ = reply.send(suggestions.map_or(Value::Null, completions::suggestions_json));
        }
    }

    /// The extensions' shortcuts against the current key bindings, and pi's
    /// `[Extension issues]` about them and about commands named like built-in
    /// ones.
    fn install_shortcuts(&mut self) {
        let builtin: Vec<(String, Vec<String>)> = self
            .keys
            .definitions()
            .iter()
            .map(|definition| {
                (
                    definition.id.to_owned(),
                    self.keys.keys(definition.id).to_vec(),
                )
            })
            .collect();
        let (shortcuts, warnings) = self.session.extension_shortcuts(&builtin);
        self.shortcuts = shortcuts;
        self.mirror_keys();
        let extensions = self.session.extensions();
        let source_of = |path: &str| {
            extensions
                .iter()
                .map(|extension| extension.source())
                .find(|source| source.path == path)
                .unwrap_or_else(|| yapi_types::rpc::SourceInfo {
                    path: path.to_owned(),
                    source: "local".into(),
                    scope: "temporary".into(),
                    origin: "top-level".into(),
                    base_dir: None,
                })
        };
        let mut issues = Vec::new();
        for extension in extensions.iter() {
            for command in extension.commands() {
                if commands::BUILTIN
                    .iter()
                    .any(|(name, ..)| *name == command.name)
                {
                    issues.push((
                        extension.source(),
                        format!(
                            "Extension command '/{}' conflicts with built-in interactive command. Skipping in autocomplete.",
                            command.name
                        ),
                    ));
                }
            }
        }
        for (path, message) in warnings {
            issues.push((source_of(&path), message));
        }
        self.extension_issues = issues;
    }

    /// Mirrors the key bindings and shortcuts for extensions; see
    /// [`ExtensionUi::keybindings`](yapi_core::extensions::ExtensionUi::keybindings).
    fn mirror_keys(&self) {
        let bindings: Map<String, Value> = self
            .keys
            .definitions()
            .iter()
            .map(|definition| {
                (
                    definition.id.to_owned(),
                    json!(self.keys.keys(definition.id)),
                )
            })
            .collect();
        let mut shared = yapi_types::sync::lock(&self.ext.shared);
        shared.keybindings =
            json!({"kitty": self.kitty, "bindings": bindings, "actions": APP_KEYS});
        shared.shortcuts = self.shortcuts.clone();
        shared.keys = self.keys.decoder();
    }

    /// Renders the current branch's messages, as when a session opens.
    fn render_history(&mut self) {
        let messages = self.session.messages();
        let mut history: Vec<String> = Vec::new();
        for message in &messages {
            match message {
                Message::User(user) => {
                    let text = user.content.text("");
                    history.push(text.clone());
                    if !text.trim().is_empty() {
                        self.push(chat::user_item(text));
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
            self.add_to_history(&text);
        }
        if !self.session.project_trusted() && yapi_core::trust::requires_trust(&self.cwd) {
            let spacer = !self.chat.is_empty();
            let notice = lines::styled(
                format!(
                    "This project is not trusted. Project {} resources and packages are ignored. Use /trust to save a trust decision, then restart yapi.",
                    yapi_core::config::PROJECT_DIR
                ),
                self.theme.fg("warning"),
            );
            self.text_item(vec![notice], spacer, (1, 0));
        }
        let compactions = self.session.with_session(|session| {
            session
                .entries()
                .filter(|entry| matches!(entry, yapi_types::session::FileEntry::Compaction(_)))
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

    /// pi's `updateEditorBorderColor`: `bashMode` while the text is a `!`
    /// command, else the thinking level.
    fn border_level(&self) -> &'static str {
        if self.editor.text().trim_start().starts_with('!') {
            "bashMode"
        } else {
            self.session.thinking_level().as_str()
        }
    }

    fn border_style(&self) -> ratatui_core::style::Style {
        match self.border_level() {
            "bashMode" => self.theme.fg("bashMode"),
            level => self.theme.thinking_border(level),
        }
    }

    /// The status as pi's status indicator draws it: the frame and message,
    /// and the frame alone. An extension's custom frames are drawn as given.
    /// The working status takes the `border` color in the editor's top
    /// border, and the accent and muted colors outside it.
    fn status_lines(
        &self,
        border: Option<ratatui_core::style::Style>,
    ) -> Option<(StyledLine, StyledLine)> {
        let (indicator, started) = self.indicator.as_ref()?;
        let custom = match indicator {
            Indicator::Working => self.ext.working_indicator.as_ref(),
            _ => None,
        };
        let frames: Vec<&str> = match custom.and_then(|custom| custom.frames.as_ref()) {
            Some(frames) => frames.iter().map(String::as_str).collect(),
            None => SPINNER.to_vec(),
        };
        let interval = custom
            .and_then(|custom| custom.interval_ms)
            .map_or(SPINNER_INTERVAL, Duration::from_millis);
        let frame = match frames.len() {
            0 => "",
            1 => frames[0],
            count => {
                frames
                    [(started.elapsed().as_millis() / interval.as_millis().max(1)) as usize % count]
            }
        };
        let (spinner, text, message) = match indicator {
            Indicator::Working => (
                border.unwrap_or_else(|| self.theme.fg("accent")),
                border.unwrap_or_else(|| self.theme.fg("muted")),
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
        // Text with escape sequences, drawn over `base` as pi's color
        // functions wrap it.
        let styled = |text: &str, base: ratatui_core::style::Style| {
            yapi_tui::ansi::parse_line(&format!("{}{text}", yapi_tui::ansi::sgr(base))).0
        };
        let frame = if custom.is_some() {
            styled(frame, ratatui_core::style::Style::default())
        } else {
            Line::from(Span::styled(frame.to_owned(), spinner))
        };
        let mut status = frame.clone();
        if lines::width(&frame) > 0 {
            status.spans.push(Span::raw(" "));
        }
        let first_line = message.lines().next().unwrap_or_default();
        status.spans.extend(styled(first_line, text).spans);
        Some((status, frame))
    }

    /// Brings the flattened transcript (header, resources and chat) up to date
    /// for `width`, re-rendering only items that changed or animate.
    fn refresh_transcript(&mut self, width: usize) {
        // An extension's header replaces the built-in one, as in pi.
        let mut header = match &self.ext.header {
            Some(view) => {
                let mut lines = lines::spacer(1);
                lines.extend(view.render(width).0);
                lines.extend(lines::spacer(1));
                lines
            }
            None if self.show_header => header::render(
                &self.theme,
                &self.keys,
                self.expanded,
                self.show_details,
                width,
            ),
            None => Vec::new(),
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
                &self.session.resources(),
                &extensions,
                &self.cwd,
                self.home.as_deref(),
                self.expanded,
                width,
            ));
        }
        // Diagnostics show even when the listing is quiet, as in pi, with the
        // sources of the loaded resources their paths belong to.
        let resources = self.session.resources();
        let extension_sources: Vec<_> = self
            .session
            .extensions()
            .iter()
            .map(|extension| extension.source())
            .collect();
        let loaded: Vec<&yapi_types::rpc::SourceInfo> = extension_sources
            .iter()
            .chain(resources.skills.iter().map(|skill| &skill.source))
            .chain(resources.templates.iter().map(|template| &template.source))
            .chain(self.theme_files.sources())
            .collect();
        header.extend(header::conflicts(
            "[Skill conflicts]",
            &self.theme,
            &resources.skill_diagnostics,
            &loaded,
            self.home.as_deref(),
            width,
        ));
        header.extend(header::conflicts(
            "[Prompt conflicts]",
            &self.theme,
            &resources.template_diagnostics,
            &loaded,
            self.home.as_deref(),
            width,
        ));
        header.extend(header::extension_issues(
            &self.theme,
            &self.extension_issues,
            self.home.as_deref(),
            width,
        ));
        header.extend(header::conflicts(
            "[Theme conflicts]",
            &self.theme,
            &self.theme_files.diagnostics,
            &loaded,
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
            if !fresh || self.chat[index].animating() {
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

    /// The dock rows and the cursor within them. In fullscreen mode, pi's
    /// flex layout: each part is at least its minimum height, a dock taller
    /// than the screen leaves the transcript one row, its parts shrink, and
    /// each part keeps its top rows. Regular mode stacks the parts as drawn.
    fn dock(&mut self, width: usize) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let (mut parts, cursor) = self.dock_parts(width);
        let available = if self.fullscreen {
            self.size.1.saturating_sub(1).max(1)
        } else {
            for (_, min) in &mut parts {
                *min = 0;
            }
            usize::MAX
        };
        yapi_tui::screen::fit_stack(parts, cursor, available)
    }

    /// pi's dock stack: pending messages, status, widgets above, the editor
    /// (minimum three rows), widgets below and the footer, each with its
    /// rows and minimum height, and the cursor as part, row and column.
    fn dock_parts(
        &mut self,
        width: usize,
    ) -> (
        Vec<yapi_tui::screen::StackPart>,
        Option<(usize, usize, usize)>,
    ) {
        let mut out = Vec::new();
        for view in &self.pending_bash {
            out.extend(view.render(width, &self.ctx()));
        }
        let dim = self.theme.fg("dim");
        // Messages queued while compacting follow the session's queues.
        let queue = |label: &str, texts: &[String], follow_up: bool| -> Vec<String> {
            let compacting = self
                .compaction_queue
                .iter()
                .filter(|(_, f)| *f == follow_up);
            texts
                .iter()
                .chain(compacting.map(|(text, _)| text))
                .map(|text| format!("{label}: {text}"))
                .collect()
        };
        let queued = [
            queue("Steering", &self.pending.0, false),
            queue("Follow-up", &self.pending.1, true),
        ]
        .concat();
        if !queued.is_empty() {
            out.extend(lines::spacer(1));
            for text in queued {
                out.push(lines::truncated_text(&lines::styled(text, dim), width, 1));
            }
            let key = keybindings::keys_display(&self.keys, "app.message.dequeue");
            out.push(lines::truncated_text(
                &lines::styled(format!("↳ {key} to edit all queued messages"), dim),
                width,
                1,
            ));
        }
        let border = self.border_style();
        // pi's status container shows the working status when the editor
        // does not draw it in its border.
        let mut status = Vec::new();
        if let Some(editor) = &self.ext.editor
            && !editor.embeds_status
            && let Some((line, _)) = self.status_lines(None)
        {
            status.extend(lines::spacer(1));
            status.extend(lines::text_row(line, width, 1));
        }
        let mut parts = vec![(std::mem::take(&mut out), 0), (status, 0)];
        // pi's widget container above the editor: a spacer, then the widgets.
        out.extend(lines::spacer(1));
        for (_, widget) in &mut self.ext.above {
            out.extend(widget.render(width, &self.theme));
        }
        parts.push((std::mem::take(&mut out), 0));
        let cursor;
        // An overlay draws over the screen; the editor stays below it.
        let overlaid = self.overlay.is_some() && matches!(self.selector, Some(Selector::Remote(_)));
        // An extension's editor gets the built-in one's settings and status.
        let custom = self.ext.editor.is_some().then(|| {
            let config = json!({
                "border": self.border_level(),
                "paddingX": self.editor.padding_x(),
                "autocompleteMaxVisible": self.editor.autocomplete_max_visible(),
                "focused": !overlaid,
                "rows": self.size.1,
            });
            (self.status_lines(Some(border)), config)
        });
        if !overlaid && let Some(mut selector) = self.selector.take() {
            let (rows, at) = selector.render(width, &self.ui());
            self.selector = Some(selector);
            cursor = at.map(|(row, col)| (out.len() + row, col));
            out.extend(rows);
        } else if let (Some(editor), Some((embedded, config))) = (self.ext.editor.as_mut(), custom)
        {
            editor.configure(config);
            let (mut rows, at) = editor.view.render(width);
            if editor.embeds_status
                && let Some((status, spinner)) = embedded
                && let Some(top) = status_border(status, spinner, 0, width, border)
                && let Some(first) = rows.first_mut()
            {
                *first = top;
            }
            cursor = at
                .filter(|_| !overlaid)
                .map(|(row, col)| (out.len() + row, col));
            out.extend(rows);
        } else {
            self.editor.border = border;
            self.editor.set_terminal_rows(self.size.1);
            self.editor.focused = !overlaid;
            let mut editor = self.editor.render(width);
            if let Some((status, spinner)) = self.status_lines(Some(border)) {
                let hidden = self.editor.hidden_above();
                if let Some(top) = status_border(status, spinner, hidden, width, border) {
                    editor[0] = top;
                }
            }
            // The focused overlay has the cursor.
            cursor = self
                .editor
                .cursor_position()
                .filter(|_| !overlaid)
                .map(|(row, col)| (out.len() + row, col));
            out.extend(editor);
        }
        parts.push((std::mem::take(&mut out), 3));
        for (_, widget) in &mut self.ext.below {
            out.extend(widget.render(width, &self.theme));
        }
        parts.push((std::mem::take(&mut out), 0));
        if !matches!(&self.footer_cache, Some((cached, _)) if *cached == width) {
            let lines = self.footer(width);
            self.footer_cache = Some((width, lines));
        }
        if let Some((_, footer)) = &self.footer_cache {
            out.extend(footer.iter().cloned());
        }
        parts.push((out, 0));
        (parts, cursor.map(|(row, col)| (3, row, col)))
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
                subscription: model.as_ref().is_some_and(|model| {
                    footer::subscription(&self.session.registry(), &model.provider)
                }),
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
            // A covered overlay comes back once nothing else is open.
            if self.selector.is_none()
                && let Some((view, options)) = self.overlays_below.pop()
            {
                self.selector = Some(Selector::Remote(Box::new(view)));
                self.overlay = Some(options);
            }
        }
        let overlays = self.overlays();
        self.alt.overlays.clone_from(&overlays);
        self.main.overlays = overlays;
        if let Some(selector) = &mut self.selector {
            selector.tick();
        }
        let (width, height) = self.size;
        let (dock, cursor) = self.dock(width);
        // An `always` scrollbar keeps the transcript off the last column.
        let transcript_width = if self.fullscreen {
            self.alt.content_width(width)
        } else {
            width
        };
        self.refresh_transcript(transcript_width);
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

    /// pi's `setWorkingVisible`: hides the working indicator, or shows it
    /// again while the agent runs.
    fn set_working_visible(&mut self, visible: bool) {
        self.ext.working_hidden = !visible;
        if !visible {
            if matches!(self.indicator, Some((Indicator::Working, _))) {
                self.indicator = None;
            }
        } else if self.running && !matches!(self.indicator, Some((Indicator::Working, _))) {
            self.indicator = Some((Indicator::Working, Instant::now()));
        }
    }

    /// How often animations advance: the spinner's interval, or a faster
    /// custom working indicator's.
    fn animation_interval(&self) -> Duration {
        if matches!(&self.selector, Some(Selector::Choice(dialog)) if dialog.shimmering()) {
            return selectors::SHIMMER_FRAME;
        }
        match (&self.indicator, &self.ext.working_indicator) {
            (Some((Indicator::Working, _)), Some(custom)) => custom
                .interval_ms
                .map_or(SPINNER_INTERVAL, Duration::from_millis)
                .min(SPINNER_INTERVAL),
            _ => SPINNER_INTERVAL,
        }
    }

    /// pi's `scrollToPrompt`: shows the nearest message start above or below
    /// the top row. pi marks the first row of user messages and of
    /// assistant messages without tool calls.
    fn scroll_to_prompt(&mut self, forward: bool) {
        let mut anchors = Vec::new();
        for (index, offset) in self.flat_offsets.iter().enumerate() {
            match self.chat.get(index) {
                Some(Item::User(_)) => anchors.push(offset + usize::from(index > 0)),
                Some(Item::Assistant(message))
                    if !message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolCall(_))) =>
                {
                    anchors.push(*offset);
                }
                _ => {}
            }
        }
        let top = self.alt.scroll_position();
        let target = if forward {
            anchors.into_iter().filter(|row| *row > top).min()
        } else {
            anchors.into_iter().filter(|row| *row < top).max()
        };
        if let Some(row) = target {
            self.alt.scroll_to(row);
        }
    }

    /// The fullscreen renderer's theme colors and scrollbar setting.
    fn style_alt_screen(&mut self) {
        self.alt.jump_label_style = self.theme.bg("selectedBg").patch(self.theme.fg("text"));
        self.alt.scrollbar = self
            .session
            .settings()
            .fullscreen_scrollbar
            .unwrap_or_default();
        self.alt.scrollbar_track = self.theme.fg("scrollbarTrack");
        self.alt.scrollbar_thumb = self.theme.fg("scrollbarThumb");
    }

    fn progress_enabled(&self) -> bool {
        self.session
            .settings()
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.show_terminal_progress)
            .unwrap_or(false)
    }

    /// pi's progress updates when `terminal.showTerminalProgress` is on:
    /// pi-tui's `setProgress`, whose active sequence repeats every second.
    fn show_progress(&mut self, active: bool) {
        if !self.progress_enabled() {
            return;
        }
        if active {
            emit(PROGRESS_ACTIVE);
            self.progress.get_or_insert_with(Instant::now);
        } else {
            self.progress = None;
            emit(PROGRESS_CLEAR);
        }
    }

    /// The time until the progress sequence repeats, while it is active.
    fn progress_wait(&self) -> Option<Duration> {
        self.progress
            .map(|last| PROGRESS_KEEPALIVE.saturating_sub(last.elapsed()))
    }

    fn progress_keepalive(&mut self) {
        if self.progress_wait() == Some(Duration::ZERO) {
            emit(PROGRESS_ACTIVE);
            self.progress = Some(Instant::now());
        }
    }

    /// pi-tui's terminal `stop`: a repeating progress sequence is cleared.
    fn stop_progress(&mut self) -> &'static str {
        if self.progress.take().is_some() {
            PROGRESS_CLEAR
        } else {
            ""
        }
    }

    /// Whether something on screen animates.
    fn animating(&self) -> bool {
        (self.fullscreen && self.alt.scrollbar_deadline().is_some())
            || self.indicator.is_some()
            || self.chat.iter().any(Item::animating)
            || self.pending_bash.iter().any(BashView::running)
            || matches!(&self.selector, Some(Selector::Session(selector)) if selector.has_timed_status())
            || self.countdown().is_some()
            || matches!(&self.selector, Some(Selector::Choice(dialog)) if dialog.shimmering())
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
                    content.push(ContentBlock::text(""));
                }
                content[at] = block;
            };
            match event {
                AssistantMessageEvent::TextStart { content_index } => {
                    ensure(content, content_index, ContentBlock::text(""))
                }
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
                AssistantMessageEvent::ThinkingStart { content_index } => {
                    ensure(content, content_index, ContentBlock::thinking("", None))
                }
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
                        ContentBlock::tool_call(id.clone(), tool_name.clone(), Map::new()),
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
        let cwd = self.cwd.clone();
        if let Some(call) = finished_tool
            && let Some(&index) = self.tool_items.get(&call.id)
            && let Some(view) = self.tool_view(index)
        {
            view.args = Value::Object(call.arguments);
            // pi previews a built-in edit once its arguments are complete.
            if view.name == "edit" && view.draw.is_none() {
                view.edit_preview = tools::edit_preview(&view.args, &cwd);
            }
            self.draw_tool(index);
        }
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.running = true,
            // pi replaces any other indicator, such as a retry countdown.
            AgentEvent::TurnStart => {
                self.show_progress(true);
                if self.ext.working_hidden {
                    self.indicator = None;
                } else if !matches!(self.indicator, Some((Indicator::Working, _))) {
                    self.indicator = Some((Indicator::Working, Instant::now()));
                }
            }
            AgentEvent::MessageStart { message } => match message {
                Message::User(user) => {
                    let text = user.content.text("");
                    if !text.trim().is_empty() {
                        self.push(chat::user_item(text));
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
            AgentEvent::AgentSettled if self.shutdown_requested => self.quit = true,
            AgentEvent::AgentEnd { .. } => {
                self.show_progress(false);
                if matches!(self.indicator, Some((Indicator::Working, _))) {
                    self.indicator = None;
                }
                if let Some(index) = self.streaming.take() {
                    self.remove_item(index);
                }
            }
            AgentEvent::QueueUpdate {
                steering,
                follow_up,
            } => self.pending = (steering, follow_up),
            AgentEvent::CompactionStart { reason } => {
                self.show_progress(true);
                self.manual_compaction = reason == CompactionReason::Manual;
                self.indicator = Some((Indicator::Task(compaction_label(reason)), Instant::now()));
            }
            // pi shows a failed summary request's error and a countdown, then
            // the summary's own indicator again when the retry starts.
            AgentEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                self.error(error_message);
                self.indicator = Some((
                    Indicator::Retry {
                        attempt,
                        max: max_attempts,
                        until: Instant::now() + Duration::from_millis(delay_ms),
                    },
                    Instant::now(),
                ));
            }
            AgentEvent::SummarizationRetryAttemptStart { source, reason } => {
                let label = match source {
                    yapi_types::event::SummarySource::BranchSummary => format!(
                        "Summarizing branch... ({} to cancel)",
                        keybindings::keys_text(&self.keys, "app.interrupt")
                    ),
                    yapi_types::event::SummarySource::Compaction => {
                        compaction_label(reason.unwrap_or(CompactionReason::Manual))
                    }
                };
                self.indicator = Some((Indicator::Task(label), Instant::now()));
            }
            AgentEvent::SummarizationRetryFinished => {
                if matches!(self.indicator, Some((Indicator::Retry { .. }, _))) {
                    self.indicator = None;
                }
            }
            AgentEvent::CompactionEnd {
                reason,
                result,
                aborted,
                error_message,
                ..
            } => {
                self.show_progress(false);
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
            self.remove_item(position);
        }
        if let Some(Item::Status(_)) = self.chat.last() {
            self.remove_item(self.chat.len() - 1);
        }
    }

    /// Sends the messages typed during a manual compaction.
    fn flush_compaction_queue(&mut self) {
        let queue = std::mem::take(&mut self.compaction_queue);
        let mut queue = queue.into_iter();
        let Some((first, _)) = queue.next() else {
            return;
        };
        let mut messages = Vec::new();
        if self.running {
            messages.push((first, StreamingBehavior::Steer));
        } else {
            self.start_prompt(first, Vec::new());
        }
        messages.extend(queue.map(|(text, follow_up)| {
            let behavior = if follow_up {
                StreamingBehavior::FollowUp
            } else {
                StreamingBehavior::Steer
            };
            (text, behavior)
        }));
        self.queue_messages(messages);
    }

    /// pi's `steer` and `followUp` for typed messages, in order: extensions'
    /// `input` handlers see each before it is queued.
    fn queue_messages(&self, messages: Vec<(String, StreamingBehavior)>) {
        if messages.is_empty() {
            return;
        }
        let (session, notify) = (self.session.clone(), self.notifier());
        tokio::spawn(async move {
            for (text, behavior) in messages {
                let source = yapi_core::agent_session::InputSource::Interactive;
                if let Err(error) = session
                    .queue_input(&text, Vec::new(), behavior, source)
                    .await
                {
                    notify(error, NotifyKind::Error);
                }
            }
        });
    }

    /// Shows a notice from a task beside the loop, unless the session it
    /// belongs to has been replaced by then.
    fn notifier(&self) -> impl Fn(String, NotifyKind) + Send + 'static {
        let (tx, epoch) = (self.tx.clone(), self.epoch);
        move |message, kind| {
            let _ = tx.send(Event::Notify(epoch, message, kind));
        }
    }

    // Input

    fn start_prompt(&mut self, text: String, images: Vec<ImageContent>) {
        self.running = true;
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let result = session.prompt(&text, images).await;
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
                    self.set_editor_text(&text);
                    return;
                }
                self.add_to_history(&text);
                self.run_bash(command.to_owned(), exclude);
                return;
            }
        }
        if self.session.is_extension_command(&text) {
            // Extension commands run at once, even while a response streams.
            self.add_to_history(&text);
            let (session, notify) = (self.session.clone(), self.notifier());
            tokio::spawn(async move {
                if let Err(error) = session.prompt(&text, Vec::new()).await {
                    notify(error, NotifyKind::Error);
                }
            });
            return;
        }
        if self.manual_compaction {
            self.add_to_history(&text);
            self.compaction_queue.push((text, false));
            self.status("Queued message for after compaction");
            return;
        }
        self.add_to_history(&text);
        if self.running {
            self.queue_messages(vec![(text, StreamingBehavior::Steer)]);
            return;
        }
        for view in std::mem::take(&mut self.pending_bash) {
            self.push(Item::Bash(Box::new(view)));
        }
        self.start_prompt(text, Vec::new());
    }

    /// pi's `!` command: `user_bash` handlers may run it or say how, and
    /// a handler's failure, which is already shown, stops it.
    fn run_bash(&mut self, command: String, exclude: bool) {
        if !self.session.has_handlers("user_bash") {
            self.start_bash(command, exclude, None);
            return;
        }
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let then: Box<dyn FnOnce(&mut App) + Send> = match session
                .user_bash(&command, exclude)
                .await
            {
                Err(_) => return,
                Ok(UserBash::Done(result)) => {
                    Box::new(move |app| app.show_bash_result(&command, exclude, result))
                }
                Ok(UserBash::Operations(operations)) => {
                    Box::new(move |app| app.start_bash(command, exclude, Some(operations)))
                }
                Ok(UserBash::Local) => Box::new(move |app| app.start_bash(command, exclude, None)),
            };
            let _ = tx.send(Event::Then(epoch, then));
        });
    }

    /// Shows a `!` command's view, where it waits for the current response
    /// when one streams.
    fn add_bash_view(&mut self, view: BashView) {
        if self.running {
            self.pending_bash.push(view);
        } else {
            self.push(Item::Bash(Box::new(view)));
        }
    }

    /// Shows and records the result an extension's `user_bash` handler gave.
    fn show_bash_result(&mut self, command: &str, exclude: bool, result: BashResult) {
        self.next_bash += 1;
        let mut view = BashView::new(self.next_bash, command, exclude);
        if !result.output.is_empty() {
            view.append(&result.output);
        }
        self.session.record_bash(command, &result, Some(exclude));
        view.finish(result);
        self.add_bash_view(view);
    }

    fn start_bash(&mut self, command: String, exclude: bool, operations: Option<BashOperations>) {
        self.next_bash += 1;
        let id = self.next_bash;
        self.add_bash_view(BashView::new(id, &command, exclude));
        let session = self.session.clone();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            let chunks = tx.clone();
            let result = session
                .execute_bash(&command, Some(exclude), None, operations, move |chunk| {
                    let _ = chunks.send(Event::BashChunk(epoch, id, chunk.to_owned()));
                })
                .await;
            let _ = tx.send(Event::BashDone(epoch, id, Box::new(result)));
        });
    }

    /// A view for a call to `name`, which knows whether the session has
    /// such a tool.
    fn new_tool_view(&self, name: &str, args: Value) -> ToolView {
        let mut view = ToolView::new(name, args);
        let tools = self.session.tools();
        view.known = tools.all().iter().any(|tool| tool.name == name);
        view.mcp_label = yapi_core::mcp::tools::server_tool_label(tools, name);
        view
    }

    /// The tool view at `index`, marked for re-rendering.
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
            self.set_editor_text(&text);
        }
        if abort {
            self.session.abort();
        }
        queued.len()
    }

    /// Replaces the editor's text, in an extension's editor too.
    fn set_editor_text(&mut self, text: &str) {
        self.editor.set_text(text);
        if let Some(editor) = &self.ext.editor {
            editor.set_text(text);
        }
    }

    /// Adds a prompt to the history of the editor in use.
    fn add_to_history(&mut self, text: &str) {
        match &self.ext.editor {
            Some(editor) => editor.add_to_history(text),
            None => self.editor.add_to_history(text),
        }
    }

    fn on_escape(&mut self) {
        if self.running {
            self.restore_queue(true);
            return;
        }
        // A summary, or the countdown before retrying one, is cancelled.
        if matches!(
            self.indicator,
            Some((Indicator::Task(_) | Indicator::Retry { .. }, _))
        ) {
            self.session.abort();
            return;
        }
        if self.session.is_bash_running() {
            self.session.abort_bash();
            return;
        }
        if self.editor.text().trim_start().starts_with('!') {
            self.set_editor_text("");
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

    /// Keys of one read, as decoded: through the extensions'
    /// `onTerminalInput` listeners first while there are any, as pi-tui's
    /// input listeners see input before everything else. The listeners run
    /// beside the loop; later keys wait behind them, in order.
    fn on_keys(&mut self, keys: Vec<String>, terminal: &mut Terminal) {
        self.input_queue.extend(keys);
        self.pump_input(terminal);
    }

    /// Hands the waiting keys to the listeners, or handles them when no
    /// extension listens.
    fn pump_input(&mut self, terminal: &mut Terminal) {
        if self.listening || self.input_queue.is_empty() {
            return;
        }
        let keys = std::mem::take(&mut self.input_queue);
        let Some(listeners) = self.ext.listeners.clone() else {
            self.handle_keys(keys, terminal);
            return;
        };
        self.listening = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let keys = listeners.terminal_input(keys).await;
            let _ = tx.send(Event::TerminalInput(keys));
        });
    }

    /// Handles the keys of one read, as one editor input batch.
    fn handle_keys(&mut self, keys: Vec<String>, terminal: &mut Terminal) {
        self.editor.begin_input_batch();
        for key in keys {
            // Pastes are never key releases.
            if !yapi_tui::keys::is_key_release(&key) {
                self.handle_key(&key, terminal);
            }
        }
        self.editor.end_input_batch();
        // Listeners of the next keys read the text these left.
        self.ext.mirror_editor_text(self.editor.expanded_text());
    }

    fn handle_key(&mut self, data: &str, terminal: &mut Terminal) {
        if self.dispatch_key(data, terminal) {
            self.footer_cache = None;
            if let Some(view) = &self.ext.footer {
                view.invalidate();
            }
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
        // An extension's editor handles its keys, app keys included.
        if let Some(editor) = &self.ext.editor {
            editor.view.input(data);
            return false;
        }
        // Extension shortcuts come first, as in pi's editor.
        if let Some(binding) =
            extension_ui::shortcut_for(&self.shortcuts, self.keys.decoder(), data).cloned()
        {
            self.run_shortcut(binding);
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
        let history = keys.matches(data, "tui.editor.historyPrevious")
            || keys.matches(data, "tui.editor.historyNext");
        let Some(&action) = APP_KEYS
            .iter()
            .find(|action| !history && keys.matches(data, action))
        else {
            return match self.editor.handle_input(data, &self.keys) {
                EditorEvent::Submit(text) => {
                    self.on_submit(text);
                    true
                }
                EditorEvent::None => false,
            };
        };
        self.run_action(action, terminal);
        true
    }

    /// Runs extension shortcut `binding` beside the loop.
    fn run_shortcut(&self, binding: yapi_core::extensions::ShortcutBinding) {
        let (session, notify) = (self.session.clone(), self.notifier());
        tokio::spawn(async move {
            if let Err(error) = session.run_shortcut(&binding).await {
                notify(
                    format!("Shortcut handler error: {error}"),
                    NotifyKind::Error,
                );
            }
        });
    }

    /// Runs app action `action`, a key binding id, as the built-in editor's
    /// handlers do.
    fn run_action(&mut self, action: &str, terminal: &mut Terminal) {
        match action {
            "app.interrupt" => self.on_escape(),
            "app.exit" => self.quit = true,
            "app.clear" => {
                if self
                    .last_clear
                    .is_some_and(|at| at.elapsed() < DOUBLE_PRESS)
                {
                    self.quit = true;
                } else {
                    self.set_editor_text("");
                    self.last_clear = Some(Instant::now());
                }
            }
            "app.suspend" => self.suspend(terminal),
            "app.thinking.cycle" => match self.session.cycle_thinking_level() {
                Some(level) => self.status(format!("Thinking level: {}", level.as_str())),
                None => self.status("Current model does not support thinking"),
            },
            "app.model.cycleForward" => self.cycle_model(true),
            "app.model.cycleBackward" => self.cycle_model(false),
            "app.model.select" => self.open_model_selector(""),
            "app.tools.expand" => self.toggle_tools(),
            "app.thinking.toggle" => {
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
            }
            "app.editor.external" => self.external_editor(terminal),
            "app.message.copy" => self.copy_last(),
            "app.message.followUp" => {
                let text = self.editor.expanded_text().trim().to_owned();
                if text.is_empty() {
                    return;
                }
                if self.manual_compaction {
                    self.add_to_history(&text);
                    self.set_editor_text("");
                    self.compaction_queue.push((text, true));
                    self.status("Queued message for after compaction");
                } else if self.running {
                    self.add_to_history(&text);
                    self.set_editor_text("");
                    self.queue_messages(vec![(text, StreamingBehavior::FollowUp)]);
                } else {
                    self.set_editor_text("");
                    self.on_submit(text);
                }
            }
            "app.message.dequeue" => {
                let count = self.restore_queue(false);
                if count == 0 {
                    self.status("No queued messages to restore");
                } else {
                    self.status(format!(
                        "Restored {count} queued message{} to editor",
                        if count > 1 { "s" } else { "" }
                    ));
                }
            }
            "app.session.new" => self.new_session(),
            "app.session.tree" => self.open_tree(None),
            "app.session.fork" => self.open_fork(),
            "app.session.resume" => self.open_resume(),
            _ => {}
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
        self.copy_to_clipboard(text, "Copied last agent message to clipboard");
    }

    /// pi's `copyToClipboard` beside the loop, then `done` or the error.
    fn copy_to_clipboard(&self, text: String, done: &'static str) {
        let notify = self.notifier();
        tokio::spawn(async move {
            match clipboard::copy(&text, emit).await {
                Ok(()) => notify(done.to_owned(), NotifyKind::Info),
                Err(error) => notify(error, NotifyKind::Error),
            }
        });
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
            let lines = match self.session.settings().fullscreen_wheel_scroll_lines {
                Some(yapi_types::settings::NumberOr::Number(lines)) => lines.clamp(1, 100) as isize,
                _ => 1,
            };
            match button & !0b11100 {
                64 => self.alt.scroll_by(-lines),
                65 => self.alt.scroll_by(lines),
                _ => {}
            }
            return true;
        }
        let Some(&action) = VIEWPORT_KEYS
            .iter()
            .find(|action| self.keys.matches(data, action))
        else {
            return false;
        };
        match action {
            "tui.altScreen.pageUp" => self.alt.page(-1),
            "tui.altScreen.pageDown" => self.alt.page(1),
            "tui.altScreen.halfPageUp" => self.alt.half_page(-1),
            "tui.altScreen.halfPageDown" => self.alt.half_page(1),
            "tui.altScreen.lineUp" => self.alt.scroll_by(-1),
            "tui.altScreen.lineDown" => self.alt.scroll_by(1),
            "tui.altScreen.previousPrompt" => self.scroll_to_prompt(false),
            "tui.altScreen.nextPrompt" => self.scroll_to_prompt(true),
            "tui.altScreen.top" => self.alt.top(),
            "tui.altScreen.bottom" => self.alt.bottom(),
            _ => {}
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
            Action::Setting { id, value } => self.apply_setting(&id, &value),
            Action::ThemePreview(setting) => self.use_theme(Some(&setting)),
            Action::ScopedModels { enabled, save } => self.scoped_models_changed(enabled, save),
            Action::ModelThinking {
                provider,
                id,
                level,
            } => self.set_model_thinking(&provider, &id, level),
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
                Some(text) => self.copy_to_clipboard(text, "Copied selected message to clipboard"),
            },
            Action::ToggleTools => self.toggle_tools(),
            Action::Provider(option) => self.provider_chosen(*option),
            Action::LoginCancelled => self.login_cancelled(),
            Action::Choice(index) => self.on_choice(index),
            Action::Text(text) => self.on_text(text),
            Action::Trust(option) => {
                let saved =
                    yapi_core::trust::TrustStore::new(&self.agent_dir).set_many(&option.updates);
                match saved {
                    Ok(()) => self.status(format!(
                        "Saved trust decision: {}. Restart yapi for this to take effect.",
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
            Some(Dialog::LoginMenu(options, kinds, radius)) => match kinds.get(index) {
                Some(kind) => self.login_menu_chosen(options, *kind),
                None => {
                    if let Some(radius) = radius {
                        self.start_login(radius, login::Back::Menu(None));
                    }
                }
            },
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
        let scoped = self
            .session
            .scoped_models()
            .into_iter()
            .map(|entry| entry.model)
            .collect();
        let mut selector = selectors::ModelSelector::new(
            self.session.available_models(),
            scoped,
            self.session.model(),
            default,
            search,
        );
        selector.refresh_id = self.next_refresh_id();
        let refresh = catalogs::Refresh::Selector(selector.refresh_id);
        self.selector = Some(Selector::Model(Box::new(selector)));
        self.refresh_catalogs(None, refresh);
    }

    fn open_thinking_selector(&mut self) {
        let theme = selectors::select_list_theme(&self.theme);
        self.selector = Some(Selector::Thinking(Box::new(
            selectors::ThinkingSelector::new(
                self.session.thinking_level(),
                &self.session.available_thinking_levels(),
                Some(
                    self.session
                        .settings()
                        .default_thinking_level
                        .unwrap_or(yapi_core::model_resolver::DEFAULT_THINKING_LEVEL),
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
            .unwrap_or(yapi_types::settings::TreeFilterMode::Default);
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
        let default_dir = yapi_core::config::default_session_dir(&self.agent_dir, &self.cwd);
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
                    self.set_editor_text(&text);
                }
                self.status("Navigated to selected point");
            }
            Err(error) => self.error(error),
        }
    }

    // Sessions

    /// Builds a session around `manager` and shows it in place of the current
    /// one, which it replaces for `reason`.
    fn replace_session(
        &mut self,
        manager: SessionManager,
        reason: Replacement,
    ) -> Result<(), String> {
        let target = crate::runtime::file_of(&manager);
        let previous = self
            .session
            .with_session(|manager| crate::runtime::file_of(manager));
        let session = (self.factory)(manager).map_err(|error| error.to_string())?;
        session.set_start(reason, previous);
        self.session.abort();
        self.session.abort_bash();
        self.epoch += 1;
        let old = std::mem::replace(&mut self.session, session);
        subscribe(&self.session, &self.tx, self.epoch);
        self.reset_extension_ui();
        self.binding = Some(Some((old, reason, target)));
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

    /// Runs `then` unless an extension cancels a session change: at once
    /// when no extension handles `kind`, otherwise once `before`, pi's
    /// cancellable `session_before_*` event, answers that none did.
    pub(super) fn unless_cancelled(
        &mut self,
        kind: &str,
        before: impl std::future::Future<Output = bool> + Send + 'static,
        then: impl FnOnce(&mut App) + Send + 'static,
    ) {
        if !self.session.has_handlers(kind) {
            then(self);
            return;
        }
        let tx = self.tx.clone();
        let epoch = self.epoch;
        tokio::spawn(async move {
            if !before.await {
                let _ = tx.send(Event::Then(epoch, Box::new(then)));
            }
        });
    }

    fn new_session(&mut self) {
        self.indicator = None;
        let session = self.session.clone();
        let before = async move { session.before_switch(Replacement::New, None).await };
        self.unless_cancelled("session_before_switch", before, |app| {
            let result = crate::runtime::new_session(&app.session, None)
                .map_err(|error| error.to_string())
                .and_then(|manager| app.replace_session(manager, Replacement::New));
            match result {
                Ok(()) => {
                    let notice = lines::styled("✓ New session started", app.theme.fg("accent"));
                    app.text_item(vec![notice], true, (1, 1));
                }
                Err(error) => app.fatal("Failed to create session", &error),
            }
        });
    }

    /// pi's `runtimeHost.fork`: `at` keeps the entry (`/clone`), otherwise the
    /// branch ends before the user message (`/fork`).
    fn fork(&mut self, id: &str, at: bool) {
        let session = self.session.clone();
        let entry = id.to_owned();
        let before = async move { session.before_fork(&entry, at).await };
        let id = id.to_owned();
        self.unless_cancelled("session_before_fork", before, move |app| {
            let result = crate::runtime::plan_fork(&app.session, &id, at).and_then(|fork| {
                let manager = fork.build(&app.session)?;
                app.replace_session(manager, Replacement::Fork)?;
                Ok(fork.text)
            });
            match result {
                Ok(text) => {
                    if at {
                        app.set_editor_text("");
                        app.status("Cloned to new session");
                    } else {
                        app.set_editor_text(text.as_deref().unwrap_or_default());
                        app.status("Forked to new session");
                    }
                }
                Err(error) => app.error(error),
            }
        });
    }

    /// pi's `switchSession` to `path`, in `cwd_override` when given.
    fn resume(&mut self, path: &Path, cwd_override: Option<PathBuf>) {
        let session = self.session.clone();
        let target = path.display().to_string();
        let before = async move {
            session
                .before_switch(Replacement::Resume, Some(&target))
                .await
        };
        let path = path.to_path_buf();
        self.unless_cancelled("session_before_switch", before, move |app| {
            app.open_resumed(&path, cwd_override);
        });
    }

    fn open_resumed(&mut self, path: &Path, cwd_override: Option<PathBuf>) {
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
        match self.replace_session(manager, Replacement::Resume) {
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

    /// pi's `switchTuiMode`: moves the UI between the alternate screen and
    /// the main screen, which keeps what it last drew.
    fn switch_tui_mode(&mut self, fullscreen: bool) {
        if fullscreen == self.fullscreen {
            return;
        }
        if fullscreen {
            emit(ALT_SCREEN_ENTER);
            self.alt.invalidate();
        } else {
            emit(ALT_SCREEN_LEAVE);
        }
        self.fullscreen = fullscreen;
        self.invalidate_all();
    }

    /// Gives the terminal to another program and takes it back.
    fn with_terminal_released(&mut self, terminal: &mut Terminal, run: impl FnOnce()) {
        terminal.stdin_paused.store(true, Ordering::SeqCst);
        let mut out = String::from(self.stop_progress());
        if self.fullscreen {
            out.push_str(ALT_SCREEN_LEAVE);
        } else {
            out.push_str(&self.main.stop());
        }
        out.push_str(&terminal_leave(&mut terminal.protocol));
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
        self.size = yapi_tui::terminal::size();
        self.invalidate_all();
    }

    fn suspend(&mut self, terminal: &mut Terminal) {
        if cfg!(windows) {
            self.status("Suspend to background is not supported on Windows");
            return;
        }
        self.with_terminal_released(terminal, || {
            #[cfg(unix)]
            yapi_tui::terminal::suspend();
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
            "yapi-editor-{}",
            yapi_core::time::uuid_v4().split('-').next().unwrap_or("0")
        ));
        let file = dir.join("prompt.md");
        if std::fs::create_dir_all(&dir).is_err() || std::fs::write(&file, &content).is_err() {
            self.error("Failed to open external editor");
            return;
        }
        let mut edited: Option<String> = None;
        self.with_terminal_released(terminal, || {
            emit(&external_editor_notice(&command));
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
            self.set_editor_text(&text);
        }
    }
}

/// pi's notice before the external editor takes the terminal.
fn external_editor_notice(command: &str) -> String {
    format!("Launching external editor: {command}\nyapi will resume when the editor exits.\n")
}

/// pi's `CompactionStatusIndicator` label for `reason`.
fn compaction_label(reason: CompactionReason) -> String {
    match reason {
        CompactionReason::Manual => "Compacting context... (escape to cancel)",
        CompactionReason::Overflow => {
            "Context overflow detected, Auto-compacting... (escape to cancel)"
        }
        CompactionReason::Threshold => "Auto-compacting... (escape to cancel)",
    }
    .to_owned()
}

/// pi's `CustomEditor.renderTopBorder` with a status: the editor's top border
/// with `status` embedded, keeping the `↑ N more` label of `hidden` scrolled
/// lines where it fits, or showing only `spinner` where the status does not
/// fit. `None` keeps the plain border.
fn status_border(
    status: StyledLine,
    spinner: StyledLine,
    hidden: usize,
    width: usize,
    border: ratatui_core::style::Style,
) -> Option<StyledLine> {
    if width == 0 {
        return None;
    }
    // pi wraps the status to the room left and keeps the first line.
    let room = width.saturating_sub(5).max(1);
    let mut status = lines::wrap(&status, room)
        .into_iter()
        .next()
        .unwrap_or_default();
    while let Some(last) = status.spans.last_mut() {
        let kept = last.content.trim_end().len();
        if kept == 0 {
            status.spans.pop();
            continue;
        }
        if kept < last.content.len() {
            last.content = last.content[..kept].to_owned().into();
        }
        break;
    }
    let mut status = lines::truncate(&status, room, "");
    let mut status_width = lines::width(&status);
    if status_width == 0 {
        return None;
    }
    let label = (hidden > 0).then(|| format!(" ↑ {hidden} more "));
    let label_width = label.as_deref().map_or(0, yapi_tui::text::visible_width);
    let overflow_start = (width as i64 - label_width as i64).div_euclid(2);
    let fits = |status_width: usize| {
        label.is_some()
            && label_width + 2 <= width
            && overflow_start - (3 + status_width as i64 + 1) >= 1
    };
    if label.is_some() && !fits(status_width) {
        status = lines::truncate(&spinner, width, "");
        status_width = lines::width(&status);
    }
    let rule = |count: usize| "─".repeat(count);
    let mut row = Vec::new();
    if let Some(label) = label.as_deref().filter(|_| fits(status_width)) {
        let left = overflow_start as usize - (3 + status_width + 1);
        row.push(Span::styled("── ", border));
        row.extend(status.spans);
        row.push(Span::styled(
            format!(
                " {}{label}{}",
                rule(left),
                rule(width - overflow_start as usize - label_width)
            ),
            border,
        ));
    } else if width >= status_width + 5 {
        row.push(Span::styled("── ", border));
        row.extend(status.spans);
        row.push(Span::styled(
            format!(" {}", rule(width - status_width - 4)),
            border,
        ));
    } else {
        let status = lines::truncate(&spinner, width, "");
        let status_width = lines::width(&status);
        let prefix = 3.min(width.saturating_sub(status_width));
        row.push(Span::styled(rule(prefix), border));
        row.extend(status.spans);
        row.push(Span::styled(
            rule(width.saturating_sub(prefix + status_width)),
            border,
        ));
    }
    Some(Line::from(row))
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
        content: vec![ContentBlock::text(text)],
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
            if !yapi_tui::terminal::stdin_ready(STDIN_POLL) {
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
            .unwrap_or_else(|| yapi_core::config::default_session_dir(agent_dir, cwd)),
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
pub async fn run(session: AgentSession, agent_dir: PathBuf, mut options: Options) -> u8 {
    #[cfg(unix)]
    let raw = match yapi_tui::terminal::RawMode::enable() {
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
        let bin_dir = yapi_core::config::bin_dir(&agent_dir);
        tokio::spawn(async move {
            let fd = yapi_core::tools::external::ensure_tool(
                yapi_core::tools::external::ExternalTool::Fd,
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
    emit(&terminal_enter(&mut terminal.protocol));

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
            Ok(Some(Event::Input(bytes))) => early.extend(
                decode_input(&mut buffer, &mut terminal.protocol, &bytes)
                    .into_iter()
                    .filter(|key| !query.consume(key)),
            ),
            Ok(Some(event)) => early_events.push(event),
            Ok(None) | Err(_) => break,
        }
    }

    let model_fallback = options.model_fallback.take();
    let (mut app, theme_error) = App::new(
        session,
        agent_dir.clone(),
        options,
        tx.clone(),
        query.colors(),
        terminal.protocol.kitty,
    );
    let (show_details, fullscreen) = (app.show_details, app.fullscreen);
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
        let dim = yapi_tui::ansi::sgr(app.theme.fg("dim"));
        let hint = if keys.is_empty() {
            String::new()
        } else {
            format!(
                "{} ({keys} to cycle)\x1b[39m{dim}",
                yapi_tui::ansi::sgr(app.theme.fg("muted"))
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
    if let Some(error) = app
        .session
        .with_registry(|registry| registry.error().map(str::to_owned))
    {
        app.error(format!("models.json error: {error}"));
    }
    if let Some(message) = model_fallback {
        app.warning(message);
    }
    for event in early_events {
        app.on_event(event);
    }
    app.handle_keys(early, &mut terminal);
    app.warn_anthropic_subscription(None);
    app.ext.set_tools_expanded(app.expanded);
    app.binding = Some(None);
    app.start_binding();
    app.draw();
    if app.model_network {
        app.refresh_catalogs(None, catalogs::Refresh::Startup);
    }

    #[cfg(unix)]
    let mut resize =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    // SIGTERM, or SIGHUP when the terminal goes away; it ends the loop.
    let terminated = crate::modes::rpc::termination();
    tokio::pin!(terminated);
    let escape_wait = escape_timeout(|name| std::env::var(name).ok());
    let mut last_draw = Instant::now();
    let mut dirty = false;
    while !app.quit {
        let tick = if app.animating() {
            app.animation_interval()
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
        let progress_wait = app.progress_wait();
        let flush_wait = if dirty {
            FRAME_INTERVAL.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    Event::Input(bytes) => {
                        let keys = decode_input(&mut buffer, &mut terminal.protocol, &bytes);
                        app.on_keys(keys, &mut terminal);
                        if app.kitty != terminal.protocol.kitty {
                            app.keys.set_kitty(terminal.protocol.kitty);
                            app.kitty = terminal.protocol.kitty;
                            app.mirror_keys();
                        }
                    }
                    Event::EditorAction(epoch, action) if epoch == app.epoch => {
                        app.run_action(&action, &mut terminal);
                    }
                    Event::TerminalInput(keys) => {
                        app.listening = false;
                        app.handle_keys(keys, &mut terminal);
                        app.pump_input(&mut terminal);
                    }
                    event => app.on_event(event),
                }
                dirty = true;
            }
            _ = tokio::time::sleep(input_wait) => {
                let mut keys: Vec<String> = buffer
                    .flush()
                    .into_iter()
                    .filter_map(|input| match input {
                        Input::Key(key) => Some(key),
                        _ => None,
                    })
                    .collect();
                keys.extend(terminal.protocol.flush());
                app.on_keys(keys, &mut terminal);
                dirty = true;
            }
            _ = tokio::time::sleep(tick) => dirty = true,
            _ = tokio::time::sleep(progress_wait.unwrap_or_default()), if progress_wait.is_some() => {
                app.progress_keepalive();
            }
            _ = resized => {
                app.size = yapi_tui::terminal::size();
                app.invalidate_all();
                dirty = true;
            }
            _ = &mut terminated => app.quit = true,
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
    yapi_core::tools::bash::kill_tracked_children();
    app.indicator = None;
    app.selector = None;
    // pi's `stop` clears progress when the setting is on; pi-tui's clears a
    // repeating sequence the setting no longer covers.
    let mut out = String::new();
    if app.progress_enabled() {
        app.progress = None;
        out.push_str(PROGRESS_CLEAR);
    } else {
        out.push_str(app.stop_progress());
    }
    let transcript = app.session.settings().fullscreen_exit_output
        != Some(yapi_types::settings::FullscreenExitOutput::ResumeHint);
    if app.fullscreen && !transcript {
        out.push_str(ALT_SCREEN_LEAVE);
    } else if app.fullscreen {
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
    out.push_str(&terminal_leave(&mut terminal.protocol));
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
        let default_dir = yapi_core::config::default_session_dir(&agent_dir, &app.cwd);
        let command = match file.parent() {
            Some(dir) if dir != default_dir => {
                format!(
                    "yapi --session-dir {} --session {id}",
                    quote(&dir.display().to_string())
                )
            }
            _ => format!("yapi --session {id}"),
        };
        // chalk's dim, not the theme's.
        emit(&format!(
            "\x1b[2mTo resume this session:\x1b[22m {command}\n"
        ));
    }
    app.exit_code
}

impl App {
    /// The app for `session` before anything is drawn, with the error from
    /// loading the configured theme.
    fn new(
        session: AgentSession,
        agent_dir: PathBuf,
        options: Options,
        tx: UnboundedSender<Event>,
        colors: yapi_tui::terminal::TerminalColors,
        kitty: bool,
    ) -> (App, Option<String>) {
        let mode = color_mode();
        let theme_files = themes::ThemeFiles::load(&session.resources().themes);
        let theme_override = options.use_theme.clone();
        let (theme, theme_error) = load_theme(
            theme_override
                .as_deref()
                .or(session.settings().theme.as_deref()),
            &theme_files,
            &agent_dir,
            &colors,
            mode,
        );
        let mut keys = keybindings::load(&agent_dir, Keys::detect(kitty));
        keys.set_kitty(kitty);
        let settings = session.settings();
        let fullscreen = options.tui_mode.or(settings.tui_mode) != Some(TuiMode::Regular);
        let quiet = settings.quiet_startup.as_ref();
        let show_details = options.verbose
            || !matches!(
                quiet,
                Some(yapi_types::settings::BoolOr::Bool(true))
                    | Some(yapi_types::settings::BoolOr::Other(_))
            );
        // `quietStartup: true` hides the header too; `"header"` only the details.
        let show_header =
            options.verbose || !matches!(quiet, Some(yapi_types::settings::BoolOr::Bool(true)));
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
            progress: None,
            indicator: None,
            expanded: options.verbose,
            hide_thinking: settings.hide_thinking_block.unwrap_or(false),
            output_pad: usize::from(settings.output_pad.unwrap_or(1).min(1)),
            show_details,
            fullscreen,
            alt: AltScreen::new(),
            main: MainScreen::new(),
            size: yapi_tui::terminal::size(),
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
            colors,
            color_mode: mode,
            kitty,
            tx,
            session,
            factory: options.factory,
            agent_dir,
            epoch: 0,
            login: None,
            next_login: 0,
            anthropic_warning_shown: false,
            ext: extension_ui::ExtensionState::default(),
            initial: options.initial,
            initial_images: options.initial_images,
            provider_count: 0,
            model_network: options.model_network,
            next_refresh: 0,
            binding: None,
            overlay: None,
            overlays_below: Vec::new(),
            show_header,
            theme_files,
            theme_override,
            shortcuts: Vec::new(),
            shutdown_requested: false,
            input_queue: Vec::new(),
            listening: false,
            builtin_completions: None,
            waiting_suggestions: None,
            extension_issues: Vec::new(),
        };
        app.style_alt_screen();
        app.alt.bottom_key = keybindings::keys_display(&app.keys, "tui.altScreen.bottom");
        app.main.show_hardware_cursor = settings.show_hardware_cursor.unwrap_or(false);
        app.alt.show_hardware_cursor = app.main.show_hardware_cursor;
        app.main.clear_on_shrink = settings
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.clear_on_shrink)
            .unwrap_or(false);
        (app, theme_error)
    }

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
            if let Some((old, reason, target)) = old {
                old.shutdown_for(reason, target).await;
            }
            session.bind_extensions(Arc::new(ui), Mode::Tui).await;
            let _ = tx.send(Event::Bound);
        });
    }

    /// Handles everything but input.
    fn on_event(&mut self, event: Event) {
        self.footer_cache = None;
        // pi draws an extension's footer every frame; its stats change with
        // the agent's events.
        if matches!(event, Event::Agent(..))
            && let Some(view) = &self.ext.footer
        {
            view.invalidate();
        }
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
            Event::Catalogs(purpose, result) => self.on_catalogs(*purpose, result),
            Event::LogoutDone(option, result) => self.on_logout_done(&option, result),
            Event::Notify(epoch, message, kind) if epoch == self.epoch => match kind {
                NotifyKind::Error => self.error(message),
                NotifyKind::Warning => self.warning(message),
                NotifyKind::Info => self.status(message),
            },
            Event::RefreshCompletions(epoch) if epoch == self.epoch => {
                self.editor.refresh_autocomplete();
                self.answer_suggestions();
            }
            Event::ExtensionError(epoch, message, stack) if epoch == self.epoch => {
                self.extension_error(message, stack.as_deref());
            }
            Event::Ui(epoch, request) if epoch == self.epoch => self.on_ui_request(*request),
            Event::Bound => {
                // Extensions may have added skills, prompt templates and themes.
                self.theme_files = themes::ThemeFiles::load(&self.session.resources().themes);
                self.install_autocomplete();
                // Items shown before the extensions started get their components.
                self.redraw_transcript();
                let mut initial = std::mem::take(&mut self.initial).into_iter();
                if let Some(first) = initial.next() {
                    let images = std::mem::take(&mut self.initial_images);
                    self.start_prompt(first, images);
                    self.queue_messages(
                        initial
                            .map(|message| (message, StreamingBehavior::FollowUp))
                            .collect(),
                    );
                }
            }
            Event::Rendered(epoch, key, width, lines) if epoch == self.epoch => {
                self.on_rendered(key, width, &lines);
            }
            Event::Component(epoch, slot, sequence, component) if epoch == self.epoch => {
                self.on_component(*slot, sequence, component);
            }
            Event::Then(epoch, then) if epoch == self.epoch => then(self),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app around an in-memory session without a model, as `run` builds
    /// it, and the receiver of its events.
    pub(super) fn app() -> (App, UnboundedReceiver<Event>) {
        let cwd = Path::new("/work");
        let session = AgentSession::new(yapi_core::agent_session::SessionConfig {
            cwd: cwd.to_path_buf(),
            agent_dir: PathBuf::from("/agent"),
            settings: yapi_core::settings::SettingsManager::in_memory(),
            registry: yapi_ai::registry::ModelRegistry::builtin(),
            apis: yapi_ai::api::Apis::default(),
            session: SessionManager::in_memory(cwd),
            model: None,
            thinking_level: yapi_types::message::ThinkingLevel::Off,
            tools: Vec::new(),
            extensions: Vec::new(),
            include_extension_tools: false,
            allowed_tools: None,
            excluded_tools: Vec::new(),
            resources: yapi_core::agent_session::Resources::default(),
            docs: yapi_core::docs::Locations::default(),
        });
        let options = Options {
            tui_mode: None,
            verbose: false,
            initial: Vec::new(),
            initial_images: Vec::new(),
            factory: Box::new(|_| anyhow::bail!("no sessions in tests")),
            use_theme: None,
            model_fallback: None,
            model_network: false,
        };
        let (tx, rx) = unbounded_channel();
        let colors = yapi_tui::terminal::TerminalColors::default();
        let (app, _) = App::new(session, PathBuf::from("/agent"), options, tx, colors, false);
        (app, rx)
    }

    #[tokio::test]
    async fn an_unfinished_message_leaves_later_items_in_place() {
        let (mut app, _events) = app();
        let streaming = app.push(Item::Status("streaming".into()));
        app.streaming = Some(streaming);
        let tool = app.push(Item::Tool(Box::new(ToolView::new("read", Value::Null))));
        app.tool_items.insert("call".into(), tool);
        app.refresh_transcript(40);
        app.on_agent_event(AgentEvent::AgentEnd {
            messages: Vec::new(),
            will_retry: false,
        });
        app.error("after");
        assert!(matches!(app.chat[app.tool_items["call"]], Item::Tool(_)));
        let rows = app.transcript(40);
        app.flat_key = None;
        assert_eq!(rows, app.transcript(40));
    }

    fn text(line: &StyledLine) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn border(status: &str, hidden: usize, width: usize) -> String {
        let style = ratatui_core::style::Style::default();
        let status = yapi_tui::ansi::parse_line(status).0;
        let spinner = Line::from("●");
        text(&status_border(status, spinner, hidden, width, style).expect("a status"))
    }

    #[test]
    fn external_editor_notice_names_yapi() {
        assert_eq!(
            external_editor_notice("vim"),
            "Launching external editor: vim\nyapi will resume when the editor exits.\n"
        );
    }

    #[test]
    fn status_border_matches_pi_layouts() {
        assert_eq!(border("● Working", 0, 20), "── ● Working ───────");
        // ANSI in the status takes no columns.
        assert_eq!(
            border("● \x1b[38;2;1;2;3mWorking\x1b[39m  ", 0, 20),
            "── ● Working ───────"
        );
        // The scroll label sits centered when the status leaves room.
        assert_eq!(
            border("● Working", 3, 40),
            "── ● Working ── ↑ 3 more ───────────────"
        );
        // Otherwise only the spinner keeps the label company.
        assert_eq!(
            border("● Working hard", 3, 26),
            "── ● ─── ↑ 3 more ────────"
        );
        // The status wraps to the room left, keeping its first line.
        assert_eq!(border("● Working", 0, 8), "── ● ───");
        assert_eq!(border("● Working", 0, 4), "───●");
    }
}
