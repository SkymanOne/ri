//! Interactive mode: the full-screen or inline terminal UI.
//!
//! Port of `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! in pi `v1.0.0`, on pi-tui's line model: every component renders styled
//! lines for a width, and a renderer writes the frame.

mod bash_view;
mod chat;
mod clipboard;
mod commands;
mod footer;
mod header;
pub mod keybindings;
pub mod picker;
mod selectors;
mod session_selector;
mod tools;
mod tree_selector;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui_core::text::{Line, Span};
use ri_core::agent_session::{AgentSession, TreeNavigation, TreeOutcome};
use ri_core::bash_executor::BashResult;
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
    AssistantMessage, Content, ContentBlock, Message, StopReason, TextContent, ThinkingContent,
    ToolCall,
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
#[derive(Clone, Debug)]
enum Dialog {
    /// "Summarize branch?" for navigating to the entry.
    TreeSummary(String),
    /// Custom summary instructions for navigating to the entry.
    TreeInstructions(String),
    /// Resume a session whose working directory is gone.
    ResumeMissingCwd(PathBuf),
    /// Import a session file.
    Import(String),
}

/// Builds a session around a session file, as pi's runtime factory does.
pub type SessionFactory = Box<dyn Fn(SessionManager) -> anyhow::Result<AgentSession>>;

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

fn user_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
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

/// Picks the configured theme for the terminal's colors.
fn load_theme(
    setting: Option<&str>,
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
    if let Some(theme) = Theme::builtin(&name, mode) {
        return (theme, None);
    }
    let path = agent_dir.join("themes").join(format!("{name}.json"));
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
                            let mut view =
                                ToolView::new(&call.name, Value::Object(call.arguments.clone()));
                            if matches!(
                                assistant.stop_reason,
                                StopReason::Aborted | StopReason::Error
                            ) {
                                view.is_error = true;
                                view.result = Some(error_result(&assistant_error(assistant)));
                            }
                            let index = self.push(Item::Tool(Box::new(view)));
                            self.tool_items.insert(call.id.clone(), index);
                        }
                    }
                }
                Message::ToolResult(result) => {
                    if let Some(&index) = self.tool_items.get(&result.tool_call_id)
                        && let Some(Item::Tool(view)) = self.chat.get_mut(index)
                    {
                        view.result = Some(tool_result_of(result));
                        view.is_error = result.is_error;
                    }
                }
                Message::BashExecution(bash) => {
                    self.push(Item::Bash(Box::new(BashView::from_message(bash))));
                }
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
            Indicator::Working => (border, border, "Working".to_owned()),
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

    fn transcript(&mut self, width: usize) -> Vec<StyledLine> {
        let mut out = header::render(
            &self.theme,
            &self.keys,
            self.expanded,
            self.show_details,
            width,
        );
        if self.show_details {
            out.extend(header::listing(
                &self.theme,
                self.session.resources(),
                &self.cwd,
                self.home.as_deref(),
                self.expanded,
                width,
            ));
        }
        let generation = self.generation;
        for index in 0..self.chat.len() {
            let fresh =
                matches!(&self.cache[index], Some((w, g, _)) if *w == width && *g == generation);
            let dynamic = matches!(self.chat[index], Item::Tool(_) | Item::Bash(_));
            if !fresh || dynamic {
                let lines = self.chat[index].render(width, index == 0, &self.ctx());
                self.cache[index] = Some((width, generation, lines));
            }
            if let Some((_, _, lines)) = &self.cache[index] {
                out.extend(lines.iter().cloned());
            }
        }
        out
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
        out.extend(lines::spacer(1));
        let cursor;
        if let Some(mut selector) = self.selector.take() {
            let (rows, at) = selector.render(width, &self.ui());
            self.selector = Some(selector);
            cursor = at.map(|(row, col)| (out.len() + row, col));
            out.extend(rows);
        } else {
            let border = self.border_style();
            self.editor.border = border;
            self.editor.set_terminal_rows(self.size.1);
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
            cursor = self
                .editor
                .cursor_position()
                .map(|(row, col)| (out.len() + row, col));
            out.extend(editor);
        }
        out.extend(self.footer(width));
        (out, cursor)
    }

    fn footer(&self, width: usize) -> Vec<StyledLine> {
        let model = self.session.model();
        let cwd = footer::format_cwd(&self.cwd, self.home.as_deref());
        let name = self.session.with_session(|session| session.name());
        let providers: std::collections::HashSet<String> = self
            .session
            .available_models()
            .into_iter()
            .map(|model| model.provider)
            .collect();
        let settings = self.session.settings();
        let auto_compact = settings
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.enabled)
            .unwrap_or(true);
        footer::render(
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
        )
    }

    fn draw(&mut self) {
        if let Some(selector) = &mut self.selector {
            selector.tick();
        }
        let (width, height) = self.size;
        let transcript = self.transcript(width);
        let (dock, cursor) = self.dock(width);
        let frame = if self.fullscreen {
            self.alt.frame(&transcript, &dock, cursor, width, height)
        } else {
            let offset = transcript.len();
            let mut document = transcript;
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
            let index = self.push(Item::Tool(Box::new(ToolView::new(
                &name,
                Value::Object(Map::new()),
            ))));
            self.tool_items.insert(id, index);
        }
        if let Some(call) = finished_tool
            && let Some(&index) = self.tool_items.get(&call.id)
            && let Some(Item::Tool(view)) = self.chat.get_mut(index)
        {
            view.args = Value::Object(call.arguments);
        }
    }

    fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.running = true,
            AgentEvent::TurnStart => {
                if self.indicator.is_none() {
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
                            && let Some(Item::Tool(view)) = self.chat.get_mut(index)
                            && view.result.is_none()
                        {
                            view.result = Some(error_result(&error));
                            view.is_error = true;
                        }
                    }
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
                ..
            } => {
                let index = match self.tool_items.get(&tool_call_id) {
                    Some(&index) => index,
                    None => {
                        let index = self.push(Item::Tool(Box::new(ToolView::new(
                            &tool_name,
                            args.clone(),
                        ))));
                        self.tool_items.insert(tool_call_id, index);
                        index
                    }
                };
                if let Some(Item::Tool(view)) = self.chat.get_mut(index) {
                    view.args = args;
                    view.started = Some(Instant::now());
                }
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(&index) = self.tool_items.get(&tool_call_id)
                    && let Some(Item::Tool(view)) = self.chat.get_mut(index)
                {
                    view.partial = Some(partial_result);
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                if let Some(&index) = self.tool_items.get(&tool_call_id)
                    && let Some(Item::Tool(view)) = self.chat.get_mut(index)
                {
                    view.result = Some(result);
                    view.is_error = is_error;
                    view.finished = Some(Instant::now());
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
                .execute_bash(&command, exclude, move |chunk| {
                    let _ = chunks.send(Event::BashChunk(epoch, id, chunk.to_owned()));
                })
                .await;
            let _ = tx.send(Event::BashDone(epoch, id, Box::new(result)));
        });
    }

    fn bash_view(&mut self, id: u64) -> Option<&mut BashView> {
        if let Some(view) = self.pending_bash.iter_mut().find(|view| view.id == id) {
            return Some(view);
        }
        self.chat.iter_mut().find_map(|item| match item {
            Item::Bash(view) if view.id == id => Some(view.as_mut()),
            _ => None,
        })
    }

    fn restore_queue(&mut self, abort: bool) -> usize {
        let mut queued = self.session.clear_queues();
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
        if self.fullscreen && self.handle_viewport_key(data) {
            return;
        }
        if self.selector.is_some() {
            self.handle_selector_key(data);
            return;
        }
        let keys = &self.keys;
        if keys.matches(data, "app.interrupt") && !self.editor.is_showing_autocomplete() {
            self.on_escape();
            return;
        }
        if keys.matches(data, "app.exit") && self.editor.text().is_empty() {
            self.quit = true;
            return;
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
                return;
            }
            if keys.matches(data, "app.suspend") {
                self.suspend(terminal);
                return;
            }
            if keys.matches(data, "app.thinking.cycle") {
                match self.session.cycle_thinking_level() {
                    Some(level) => self.status(format!("Thinking level: {}", level.as_str())),
                    None => self.status("Current model does not support thinking"),
                }
                return;
            }
            if keys.matches(data, "app.model.cycleForward")
                || keys.matches(data, "app.model.cycleBackward")
            {
                let forward = keys.matches(data, "app.model.cycleForward");
                self.cycle_model(forward);
                return;
            }
            if keys.matches(data, "app.model.select") {
                self.open_model_selector("");
                return;
            }
            if keys.matches(data, "app.tools.expand") {
                self.toggle_tools();
                return;
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
                return;
            }
            if keys.matches(data, "app.editor.external") {
                self.external_editor(terminal);
                return;
            }
            if keys.matches(data, "app.message.copy") {
                self.copy_last();
                return;
            }
            if keys.matches(data, "app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_owned();
                if text.is_empty() {
                    return;
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
                return;
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
                return;
            }
            if keys.matches(data, "app.session.new") {
                self.new_session();
                return;
            }
            if keys.matches(data, "app.session.tree") {
                self.open_tree(None);
                return;
            }
            if keys.matches(data, "app.session.fork") {
                self.open_fork();
                return;
            }
            if keys.matches(data, "app.session.resume") {
                self.open_resume();
                return;
            }
        }
        if let EditorEvent::Submit(text) = self.editor.handle_input(data, &self.keys) {
            self.on_submit(text);
        }
    }

    fn toggle_tools(&mut self) {
        self.expanded = !self.expanded;
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
        let models = self.session.available_models();
        if models.len() <= 1 {
            self.status("Only one model available");
            return;
        }
        let current = self.session.model();
        let index = current
            .as_ref()
            .and_then(|current| {
                models
                    .iter()
                    .position(|model| model.provider == current.provider && model.id == current.id)
            })
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % models.len()
        } else {
            (index + models.len() - 1) % models.len()
        };
        let model = models[next].clone();
        let name = if model.name.is_empty() {
            model.id.clone()
        } else {
            model.name.clone()
        };
        let reasoning = model.reasoning;
        self.session.switch_model(model);
        let level = self.session.thinking_level();
        if reasoning && level.as_str() != "off" {
            self.status(format!("Switched to {name} (thinking: {})", level.as_str()));
        } else {
            self.status(format!("Switched to {name}"));
        }
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
            None => {}
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Model { model, default } => {
                let id = model.id.clone();
                let provider = model.provider.clone();
                self.session.set_model(*model);
                if default {
                    let _ = self.session.set_global_setting(
                        "defaultProvider",
                        Some(Value::String(provider.clone())),
                    );
                    let _ = self
                        .session
                        .set_global_setting("defaultModel", Some(Value::String(id.clone())));
                    self.status(format!("Default model: {provider}/{id}"));
                } else {
                    self.status(format!("Model: {id}"));
                }
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
            Action::Choice(index) => self.on_choice(index),
            Action::Text(text) => self.on_text(text),
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
            Some(Dialog::ResumeMissingCwd(path)) => {
                if index == 0 {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                    self.resume(&path, Some(cwd));
                } else {
                    self.status("Resume cancelled");
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: String) {
        if let Some(Dialog::TreeInstructions(id)) = self.dialog.take() {
            self.navigate(id, true, Some(text));
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
        self.session = session;
        subscribe(&self.session, &self.tx, self.epoch);
        self.cwd = self.session.cwd().to_path_buf();
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
        let (persisted, cwd, dir) = self.session.with_session(|session| {
            (
                session.is_persisted(),
                session.cwd().to_path_buf(),
                session.dir().to_path_buf(),
            )
        });
        let manager = if persisted {
            match SessionManager::create(&cwd, &dir, None) {
                Ok(manager) => manager,
                Err(error) => {
                    self.fatal("Failed to create session", &error.to_string());
                    return;
                }
            }
        } else {
            SessionManager::in_memory(&cwd)
        };
        match self.replace_session(manager) {
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
        let prepared = self.session.with_session(|session| {
            let entry = session
                .entry(id)
                .cloned()
                .ok_or_else(|| "Invalid entry ID for forking".to_owned())?;
            let (target, text) = if at {
                (Some(id.to_owned()), None)
            } else {
                match &entry {
                    ri_types::session::FileEntry::Message(message) => match &message.message {
                        Message::User(user) => (
                            message.meta.parent_id.clone(),
                            Some(user_text(&user.content)),
                        ),
                        _ => return Err("Invalid entry ID for forking".to_owned()),
                    },
                    _ => return Err("Invalid entry ID for forking".to_owned()),
                }
            };
            Ok((
                target,
                text,
                session.is_persisted(),
                session.file().map(Path::to_path_buf),
                session.cwd().to_path_buf(),
                session.dir().to_path_buf(),
            ))
        });
        let (target, text, persisted, file, cwd, dir) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.error(error);
                return;
            }
        };
        let manager: Result<SessionManager, String> = (|| {
            if !persisted {
                return Err(
                    "This session has not been saved yet. Send a message before cloning or forking it."
                        .to_owned(),
                );
            }
            let file = file.ok_or("Persisted session is missing a session file")?;
            match &target {
                None => {
                    let mut manager =
                        SessionManager::create(&cwd, &dir, None).map_err(|e| e.to_string())?;
                    manager.new_session(None, Some(file.display().to_string()));
                    Ok(manager)
                }
                Some(target) => {
                    if !file.exists() {
                        return Err("This session has not been saved yet. Send a message before cloning or forking it.".to_owned());
                    }
                    let mut manager =
                        SessionManager::open(&file, Some(&dir), None).map_err(|e| e.to_string())?;
                    manager
                        .create_branched_session(target)
                        .map_err(|e| e.to_string())?
                        .ok_or("Failed to create forked session")?;
                    Ok(manager)
                }
            }
        })();
        let result = manager.and_then(|manager| self.replace_session(manager));
        match result {
            Ok(()) => {
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
        let manager = match SessionManager::open(path, None, cwd_override.as_deref()) {
            Ok(manager) => manager,
            Err(error) => {
                self.fatal("Failed to resume session", &error.to_string());
                return;
            }
        };
        if cwd_override.is_none() && !manager.cwd().exists() {
            let fallback = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
            self.dialog = Some(Dialog::ResumeMissingCwd(path.to_path_buf()));
            self.selector = Some(Selector::Choice(ChoiceDialog::new(
                &format!(
                    "Session cwd not found\ncwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
                    manager.cwd().display(),
                    fallback.display()
                ),
                &["Yes", "No"],
            )));
            return;
        }
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
    let (theme, theme_error) = load_theme(
        session.settings().theme.as_deref(),
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
    if fullscreen {
        emit(ALT_SCREEN_ENTER);
    }
    app.install_autocomplete();
    app.render_history();
    if let Some(error) = theme_error {
        app.error(error);
    }
    for event in early_events {
        app.on_event(event, &mut terminal);
    }
    for key in early {
        app.handle_key(&key, &mut terminal);
    }
    let mut initial = options.initial.into_iter();
    if let Some(first) = initial.next() {
        app.start_prompt(first);
        for message in initial {
            app.session.follow_up(&message, Vec::new());
        }
    }
    app.draw();

    #[cfg(unix)]
    let mut resize =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    #[cfg(unix)]
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
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
        #[cfg(unix)]
        let terminated = async {
            match terminate.as_mut() {
                Some(signal) => signal.recv().await,
                None => std::future::pending().await,
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
        if dirty && last_draw.elapsed() >= FRAME_INTERVAL {
            app.draw();
            last_draw = Instant::now();
            dirty = false;
        }
    }

    // Teardown: leave the screen as pi does and restore the terminal.
    app.session.abort();
    app.session.abort_bash();
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
        let dim = ri_tui::ansi::sgr(app.theme.fg("dim"));
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
        emit(&format!("{dim}To resume this session:\x1b[0m {command}\n"));
    }
    app.exit_code
}

impl App {
    /// Handles everything but input.
    fn on_event(&mut self, event: Event, _terminal: &mut Terminal) {
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
            _ => {}
        }
    }
}
