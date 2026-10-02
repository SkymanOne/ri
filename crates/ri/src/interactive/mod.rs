//! Interactive mode: the full-screen or inline terminal UI.
//!
//! Port of `packages/coding-agent/src/modes/interactive/interactive-mode.ts`
//! in pi `v1.0.0`, on pi-tui's line model: every component renders styled
//! lines for a width, and a renderer writes the frame.

mod chat;
mod footer;
mod header;
pub mod keybindings;
mod tools;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui_core::text::{Line, Span};
use ri_core::agent_session::AgentSession;
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

use self::chat::{Item, RenderContext};
use self::tools::ToolView;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const DOUBLE_PRESS: Duration = Duration::from_millis(500);
const COLOR_QUERY_TIMEOUT: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Events the loop handles.
enum Event {
    Input(Vec<u8>),
    InputClosed,
    Agent(Box<AgentEvent>),
    PromptDone(Result<(), String>),
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
    Compaction(String),
}

/// What the run needs from startup.
pub struct Options {
    /// Fullscreen (`--tui-mode`) overrides the setting.
    pub tui_mode: Option<TuiMode>,
    /// Show every startup detail.
    pub verbose: bool,
    /// Messages to send at start, in order.
    pub initial: Vec<String>,
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

/// The interactive application.
struct App {
    session: AgentSession,
    theme: Theme,
    markdown: MarkdownTheme,
    keys: Keybindings,
    editor: Editor,
    chat: Vec<Item>,
    cache: Vec<Option<(usize, u64, Vec<StyledLine>)>>,
    generation: u64,
    streaming: Option<usize>,
    tool_items: HashMap<String, usize>,
    pending: (Vec<String>, Vec<String>),
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
    cwd: PathBuf,
    home: Option<PathBuf>,
    branch: Option<String>,
    expand_key: String,
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
    session: &AgentSession,
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
    let setting = session.settings().theme;
    let Some(name) = resolve_theme_setting(setting.as_deref(), appearance) else {
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

impl App {
    fn ctx(&self) -> RenderContext<'_> {
        RenderContext {
            theme: &self.theme,
            markdown: &self.markdown,
            expanded: self.expanded,
            hide_thinking: self.hide_thinking,
            output_pad: self.output_pad,
            expand_key: &self.expand_key,
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

    fn clear_chat(&mut self) {
        self.chat.clear();
        self.cache.clear();
        self.streaming = None;
        self.tool_items.clear();
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
            Indicator::Compaction(label) => (
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
            let dynamic = matches!(self.chat[index], Item::Tool(_));
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
        let (steering, follow_up) = &self.pending;
        if !steering.is_empty() || !follow_up.is_empty() {
            out.extend(lines::spacer(1));
            let dim = self.theme.fg("dim");
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
        let cursor = self
            .editor
            .cursor_position()
            .map(|(row, col)| (out.len() + row, col));
        out.extend(editor);
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
                self.indicator = Some((Indicator::Compaction(label.to_owned()), Instant::now()));
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
                        self.push(Item::Lines({
                            let mut lines = lines::spacer(1);
                            lines.extend(lines::text(
                                &[lines::styled(message, self.theme.fg("error"))],
                                self.size.0,
                                1,
                                0,
                                None,
                            ));
                            lines
                        }));
                    }
                }
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

    // Input

    fn start_prompt(&mut self, text: String) {
        self.running = true;
        let session = self.session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = session.prompt(&text, Vec::new()).await;
            let _ = tx.send(Event::PromptDone(result));
        });
    }

    fn on_submit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if text == "/quit" {
            self.editor.set_text("");
            self.quit = true;
            return;
        }
        self.editor.add_to_history(&text);
        if self.running {
            self.session.steer(&text, Vec::new());
            return;
        }
        self.start_prompt(text);
    }

    fn restore_queue(&mut self, abort: bool) -> usize {
        let queued = self.session.clear_queues();
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
            } else {
                self.last_escape = Some(Instant::now());
            }
        }
    }

    fn handle_key(&mut self, data: &str) {
        if self.fullscreen && self.handle_viewport_key(data) {
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
            if keys.matches(data, "app.tools.expand") {
                self.expanded = !self.expanded;
                self.invalidate_all();
                self.status(if self.expanded {
                    "Tool output: expanded"
                } else {
                    "Tool output: collapsed"
                });
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
            if keys.matches(data, "app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_owned();
                if text.is_empty() {
                    return;
                }
                if self.running {
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
        }
        if let EditorEvent::Submit(text) = self.editor.handle_input(data, &self.keys) {
            self.on_submit(text);
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

fn spawn_stdin(tx: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 4096];
        loop {
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
    spawn_stdin(tx.clone());
    {
        let tx = tx.clone();
        session.subscribe(Box::new(move |event| {
            let _ = tx.send(Event::Agent(Box::new(event.clone())));
        }));
    }

    let mut protocol = KeyboardProtocol::default();
    let mut query = ColorQuery::new();
    emit(&format!(
        "{BRACKETED_PASTE_ENABLE}{}{}",
        protocol.query(),
        color_query()
    ));

    // Wait briefly for the color replies; keys typed meanwhile are kept.
    let mut buffer = InputBuffer::new();
    let mut early: Vec<String> = Vec::new();
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
                        if let Filtered::Forward(keys) = protocol.filter(&sequence, &mut write) {
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
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    let mode = if true_color() {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    };
    let (theme, theme_error) = load_theme(&session, &agent_dir, &query.colors(), mode);
    let mut keys = keybindings::load(&agent_dir, Keys::detect(protocol.kitty));
    keys.set_kitty(protocol.kitty);
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
    let mut app = App {
        markdown: markdown_theme(&theme),
        theme,
        keys,
        editor,
        chat: Vec::new(),
        cache: Vec::new(),
        generation: 0,
        streaming: None,
        tool_items: HashMap::new(),
        pending: (Vec::new(), Vec::new()),
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
        branch: footer::git_branch(&cwd),
        cwd,
        home: home_dir(),
        expand_key,
        tx: tx.clone(),
        session: session.clone(),
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
    app.render_history();
    if let Some(error) = theme_error {
        app.error(error);
    }
    for key in early {
        app.handle_key(&key);
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
        let tick = if app.indicator.is_some() || app.chat.iter().any(|item| matches!(item, Item::Tool(view) if view.result.is_none() && view.started.is_some())) {
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
                match event {
                    Event::Input(bytes) => {
                        let mut write = String::new();
                        for input in buffer.push(&bytes) {
                            match input {
                                Input::Key(sequence) => {
                                    if let Filtered::Forward(keys) = protocol.filter(&sequence, &mut write) {
                                        for key in keys {
                                            if ri_tui::keys::is_key_release(&key) {
                                                continue;
                                            }
                                            app.handle_key(&key);
                                        }
                                    }
                                }
                                Input::Paste(text) => app.handle_key(&format!("\x1b[200~{text}\x1b[201~")),
                            }
                        }
                        if !write.is_empty() {
                            emit(&write);
                        }
                        app.keys.set_kitty(protocol.kitty);
                    }
                    Event::InputClosed => app.quit = true,
                    Event::Agent(event) => app.on_agent_event(*event),
                    Event::PromptDone(result) => {
                        app.running = false;
                        if app.indicator.as_ref().is_some_and(|(indicator, _)| matches!(indicator, Indicator::Working)) {
                            app.indicator = None;
                        }
                        if let Err(error) = result {
                            app.error(if error.is_empty() { "Unknown error occurred".to_owned() } else { error });
                        }
                    }
                }
                dirty = true;
            }
            _ = tokio::time::sleep(input_wait) => {
                for input in buffer.flush() {
                    if let Input::Key(key) = input {
                        app.handle_key(&key);
                    }
                }
                if let Some(pending) = protocol.flush() {
                    app.handle_key(&pending);
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
    app.indicator = None;
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
    out.push_str(&protocol.disable());
    out.push_str("\x1b[?25h");
    emit(&out);
    #[cfg(unix)]
    raw.restore();

    let file = app
        .session
        .with_session(|session| session.file().map(Path::to_path_buf));
    let id = app
        .session
        .with_session(|session| session.header().map(|header| header.id.clone()));
    if std::io::stdout().is_terminal()
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
    0
}
