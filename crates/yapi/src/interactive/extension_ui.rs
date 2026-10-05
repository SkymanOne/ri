//! Extension UI in the interactive mode: pi's `ExtensionUIContext` as
//! `interactive-mode.ts` provides it. Requests travel to the app as events;
//! what extensions read back at once (the editor text, the theme, footer
//! data) is mirrored in [`Shared`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use yapi_core::extensions::{
    CustomOptions, DialogOptions, ExtensionUi, NotifyKind, Placement, RemoteComponent, Widget,
    WorkingIndicator,
};
use yapi_tui::lines::{self, StyledLine};
use yapi_tui::theme::{Paint, Theme};

use super::{Event, Indicator};

/// A request from an extension.
pub(super) enum Request {
    Select {
        title: String,
        options: Vec<String>,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Option<String>>,
    },
    Confirm {
        title: String,
        message: String,
        timeout: Option<Duration>,
        reply: oneshot::Sender<bool>,
    },
    Input {
        title: String,
        timeout: Option<Duration>,
        reply: oneshot::Sender<Option<String>>,
    },
    Editor {
        title: String,
        prefill: Option<String>,
        reply: oneshot::Sender<Option<String>>,
    },
    Status(String, Option<String>),
    Widget(String, Option<Widget>, Option<Placement>),
    Footer(Option<RemoteComponent>),
    Header(Option<RemoteComponent>),
    Title(String),
    WorkingMessage(Option<String>),
    HiddenThinkingLabel(Option<String>),
    WorkingVisible(bool),
    WorkingIndicator(Option<WorkingIndicator>),
    EditorText(String),
    Paste(String),
    Custom(RemoteComponent, CustomOptions),
    Close(RemoteComponent),
    Render,
    ToolsExpanded(bool),
    Shutdown,
}

/// What extensions read without waiting, kept current by the app.
#[derive(Default)]
pub(super) struct Shared {
    pub editor_text: String,
    pub tools_expanded: bool,
    pub theme: Value,
    pub footer: Value,
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The extension UI of one session.
pub(super) struct InteractiveUi {
    pub tx: UnboundedSender<Event>,
    pub epoch: u64,
    pub shared: Arc<Mutex<Shared>>,
}

impl InteractiveUi {
    fn send(&self, request: Request) {
        let _ = self.tx.send(Event::Ui(self.epoch, Box::new(request)));
    }
}

impl ExtensionUi for InteractiveUi {
    fn has_ui(&self) -> bool {
        true
    }

    fn shutdown(&self) {
        self.send(Request::Shutdown);
    }

    fn notify(&self, message: &str, kind: NotifyKind) {
        let _ = self
            .tx
            .send(Event::Notify(self.epoch, message.to_owned(), kind));
    }

    fn extension_error(&self, path: &str, _event: &str, error: &str, stack: Option<&str>) {
        let message = format!("Extension \"{path}\" error: {error}");
        let _ = self.tx.send(Event::ExtensionError(
            self.epoch,
            message,
            stack.map(str::to_owned),
        ));
    }

    fn select(
        &self,
        title: &str,
        options: Vec<String>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let (reply, answer) = oneshot::channel();
        self.send(Request::Select {
            title: title.to_owned(),
            options,
            timeout: dialog.timeout,
            reply,
        });
        Box::pin(async move { answer.await.ok().flatten() })
    }

    fn confirm(
        &self,
        title: &str,
        message: &str,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, bool> {
        let (reply, answer) = oneshot::channel();
        self.send(Request::Confirm {
            title: title.to_owned(),
            message: message.to_owned(),
            timeout: dialog.timeout,
            reply,
        });
        Box::pin(async move { answer.await.unwrap_or(false) })
    }

    /// pi's interactive input shows no placeholder.
    fn input(
        &self,
        title: &str,
        _placeholder: Option<&str>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let (reply, answer) = oneshot::channel();
        self.send(Request::Input {
            title: title.to_owned(),
            timeout: dialog.timeout,
            reply,
        });
        Box::pin(async move { answer.await.ok().flatten() })
    }

    fn editor(&self, title: &str, prefill: Option<&str>) -> BoxFuture<'static, Option<String>> {
        let (reply, answer) = oneshot::channel();
        self.send(Request::Editor {
            title: title.to_owned(),
            prefill: prefill.map(str::to_owned),
            reply,
        });
        Box::pin(async move { answer.await.ok().flatten() })
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        self.send(Request::Status(key.to_owned(), text.map(str::to_owned)));
    }

    fn set_widget(&self, key: &str, widget: Option<Widget>, placement: Option<Placement>) {
        self.send(Request::Widget(key.to_owned(), widget, placement));
    }

    fn set_footer(&self, component: Option<RemoteComponent>) {
        self.send(Request::Footer(component));
    }

    fn set_header(&self, component: Option<RemoteComponent>) {
        self.send(Request::Header(component));
    }

    fn set_title(&self, title: &str) {
        self.send(Request::Title(title.to_owned()));
    }

    fn set_working_message(&self, message: Option<&str>) {
        self.send(Request::WorkingMessage(message.map(str::to_owned)));
    }

    fn refresh_completions(&self) {
        let _ = self.tx.send(Event::RefreshCompletions(self.epoch));
    }

    fn set_working_visible(&self, visible: bool) {
        self.send(Request::WorkingVisible(visible));
    }

    fn set_working_indicator(&self, indicator: Option<WorkingIndicator>) {
        self.send(Request::WorkingIndicator(indicator));
    }

    fn set_hidden_thinking_label(&self, label: Option<&str>) {
        self.send(Request::HiddenThinkingLabel(label.map(str::to_owned)));
    }

    fn set_editor_text(&self, text: &str) {
        // Reads that follow see the new text at once.
        lock(&self.shared).editor_text = text.to_owned();
        self.send(Request::EditorText(text.to_owned()));
    }

    fn paste_to_editor(&self, text: &str) {
        self.send(Request::Paste(text.to_owned()));
    }

    fn editor_text(&self) -> String {
        lock(&self.shared).editor_text.clone()
    }

    fn shows_components(&self) -> bool {
        true
    }

    fn custom(&self, component: RemoteComponent, options: CustomOptions) {
        self.send(Request::Custom(component, options));
    }

    fn close(&self, component: RemoteComponent) {
        self.send(Request::Close(component));
    }

    fn request_render(&self) {
        self.send(Request::Render);
    }

    fn tools_expanded(&self) -> bool {
        lock(&self.shared).tools_expanded
    }

    fn set_tools_expanded(&self, expanded: bool) {
        lock(&self.shared).tools_expanded = expanded;
        self.send(Request::ToolsExpanded(expanded));
    }

    fn theme(&self) -> Value {
        lock(&self.shared).theme.clone()
    }

    fn footer_data(&self) -> Value {
        lock(&self.shared).footer.clone()
    }
}

/// `theme` as extensions see it: the escape sequence that starts each token's
/// color.
pub(super) fn theme_json(theme: &Theme) -> Value {
    let mut fg = Map::new();
    let mut bg = Map::new();
    let mut dim = Vec::new();
    let mut tokens: Vec<&str> = theme.tokens().collect();
    tokens.sort_unstable();
    for token in tokens {
        let (foreground, background) = match theme.paint(token) {
            Some(Paint::Color(_)) => (
                yapi_tui::ansi::sgr(ratatui_core::style::Style::new().fg(theme.color(token))),
                yapi_tui::ansi::sgr(ratatui_core::style::Style::new().bg(theme.color(token))),
            ),
            _ => ("\x1b[39m".to_owned(), "\x1b[49m".to_owned()),
        };
        fg.insert(token.to_owned(), Value::String(foreground));
        bg.insert(token.to_owned(), Value::String(background));
        if theme.is_dim(token) {
            dim.push(token.to_owned());
        }
    }
    json!({
        "name": theme.name,
        "mode": theme.mode().as_str(),
        "fg": fg,
        "bg": bg,
        "dim": dim,
    })
}

/// A component that lives in an extension runtime, as the TUI paints it: the
/// rows of its last render. A stale component asks for new rows and paints
/// the old ones meanwhile, so drawing never waits for an extension.
pub(super) struct RemoteView {
    component: RemoteComponent,
    tx: UnboundedSender<Event>,
    epoch: u64,
    state: std::cell::RefCell<ViewState>,
}

#[derive(Default)]
struct ViewState {
    rows: Vec<StyledLine>,
    cursor: Option<(usize, usize)>,
    rendered: Option<usize>,
    requested: Option<usize>,
    dirty: bool,
}

impl RemoteView {
    pub fn new(component: RemoteComponent, tx: UnboundedSender<Event>, epoch: u64) -> RemoteView {
        RemoteView {
            component,
            tx,
            epoch,
            state: std::cell::RefCell::new(ViewState {
                dirty: true,
                ..ViewState::default()
            }),
        }
    }

    /// See [`RemoteComponent::key`].
    pub fn key(&self) -> (u64, u32) {
        self.component.key()
    }

    /// Marks the rows stale.
    pub fn invalidate(&self) {
        self.state.borrow_mut().dirty = true;
    }

    /// Delivers raw terminal input; pi-tui renders again after input.
    pub fn input(&self, data: &str) {
        self.component.input(data);
        self.invalidate();
    }

    /// The rows of the last render at `width`, and the cursor among them.
    pub fn render(&self, width: usize) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let mut state = self.state.borrow_mut();
        if (state.dirty || state.rendered != Some(width)) && state.requested.is_none() {
            state.dirty = false;
            state.requested = Some(width);
            let render = self
                .component
                .render(u16::try_from(width).unwrap_or(u16::MAX));
            let (tx, epoch, key) = (self.tx.clone(), self.epoch, self.key());
            tokio::spawn(async move {
                let lines = render.await;
                let _ = tx.send(Event::Rendered(epoch, key, width, lines));
            });
        }
        (state.rows.clone(), state.cursor)
    }

    /// Takes the lines of a render at `width`; `false` when they change
    /// nothing.
    pub fn rendered(&self, width: usize, lines: &[String]) -> bool {
        let mut cursor = None;
        let rows: Vec<StyledLine> = lines
            .iter()
            .enumerate()
            .map(|(row, text)| {
                let (line, column) = yapi_tui::ansi::parse_line(text);
                if let Some(column) = column {
                    cursor = Some((row, column));
                }
                lines::truncate(&line, width, "")
            })
            .collect();
        let mut state = self.state.borrow_mut();
        state.requested = None;
        let changed = rows != state.rows || cursor != state.cursor || state.rendered != Some(width);
        state.rows = rows;
        state.cursor = cursor;
        state.rendered = Some(width);
        changed
    }
}

/// A widget as shown: pi's `Container` of `Text` rows for lines, or a
/// component.
pub(super) enum WidgetView {
    Lines(Vec<String>),
    Remote(RemoteView),
}

/// pi's `MAX_WIDGET_LINES`.
const MAX_WIDGET_LINES: usize = 10;

impl WidgetView {
    pub fn render(&mut self, width: usize, theme: &Theme) -> Vec<StyledLine> {
        match self {
            WidgetView::Lines(text) => {
                let mut out = Vec::new();
                for line in text.iter().take(MAX_WIDGET_LINES) {
                    let (line, _) = yapi_tui::ansi::parse_line(line);
                    out.extend(lines::text(&[line], width, 1, 0, None));
                }
                if text.len() > MAX_WIDGET_LINES {
                    out.extend(lines::text(
                        &[lines::styled("... (widget truncated)", theme.fg("muted"))],
                        width,
                        1,
                        0,
                        None,
                    ));
                }
                out
            }
            WidgetView::Remote(view) => view.render(width).0,
        }
    }
}

/// pi's `sanitizeStatusText`: one line with single spaces.
pub(super) fn sanitize_status(text: &str) -> String {
    text.replace(['\r', '\n', '\t'], " ")
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What extensions placed around the editor; pi's widget containers, footer
/// statuses and replaced footer and header.
#[derive(Default)]
pub(super) struct ExtensionState {
    pub shared: Arc<Mutex<Shared>>,
    pub statuses: Vec<(String, String)>,
    pub above: Vec<(String, WidgetView)>,
    pub below: Vec<(String, WidgetView)>,
    pub footer: Option<RemoteView>,
    pub header: Option<RemoteView>,
    pub working_message: Option<String>,
    pub working_hidden: bool,
    pub working_indicator: Option<WorkingIndicator>,
    pub thinking_label: Option<String>,
}

/// `localeCompare` for status keys: letters without regard to case, then
/// lower case first.
fn locale_order(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

impl ExtensionState {
    /// pi's `resetExtensionUI`, for a new session.
    pub fn reset(&mut self) {
        self.statuses.clear();
        self.above.clear();
        self.below.clear();
        self.footer = None;
        self.header = None;
        self.working_message = None;
        self.working_hidden = false;
        self.working_indicator = None;
        self.thinking_label = None;
    }

    /// Sets status `key`, or clears it.
    pub fn set_status(&mut self, key: String, text: Option<String>) {
        self.statuses.retain(|(existing, _)| *existing != key);
        if let Some(text) = text {
            self.statuses.push((key, text));
            self.statuses.sort_by(|(a, _), (b, _)| locale_order(a, b));
        }
    }

    /// Sets widget `key` at `placement`, or removes it.
    pub fn set_widget(&mut self, key: String, widget: Option<WidgetView>, placement: Placement) {
        self.above.retain(|(existing, _)| *existing != key);
        self.below.retain(|(existing, _)| *existing != key);
        if let Some(widget) = widget {
            match placement {
                Placement::AboveEditor => self.above.push((key, widget)),
                Placement::BelowEditor => self.below.push((key, widget)),
            }
        }
    }

    /// The footer's status line: statuses by key on one line.
    pub fn status_line(&self, width: usize, theme: &Theme) -> Option<StyledLine> {
        if self.statuses.is_empty() {
            return None;
        }
        let text = self
            .statuses
            .iter()
            .map(|(_, text)| sanitize_status(text))
            .collect::<Vec<_>>()
            .join(" ");
        let (line, _) = yapi_tui::ansi::parse_line(&text);
        if lines::width(&line) <= width {
            return Some(line);
        }
        let mut cut = lines::truncate(&line, width.saturating_sub(3), "");
        cut.spans
            .push(ratatui_core::text::Span::styled("...", theme.fg("dim")));
        Some(cut)
    }

    /// The views of components around the editor.
    pub fn views(&mut self) -> impl Iterator<Item = &mut RemoteView> {
        self.above
            .iter_mut()
            .chain(self.below.iter_mut())
            .filter_map(|(_, widget)| match widget {
                WidgetView::Remote(view) => Some(view),
                WidgetView::Lines(_) => None,
            })
            .chain(self.footer.iter_mut())
            .chain(self.header.iter_mut())
    }

    /// Mirrors what extensions read back.
    pub fn mirror(&self, editor_text: String, branch: Option<&str>, providers: usize) {
        let mut shared = lock(&self.shared);
        shared.editor_text = editor_text;
        shared.footer = json!({
            "gitBranch": branch,
            "statuses": self.statuses,
            "providers": providers,
        });
    }

    /// Mirrors the theme.
    pub fn set_theme(&self, theme: &Theme) {
        lock(&self.shared).theme = theme_json(theme);
    }

    /// Mirrors the tool expansion state.
    pub fn set_tools_expanded(&self, expanded: bool) {
        lock(&self.shared).tools_expanded = expanded;
    }
}

impl super::App {
    /// Handles a request from an extension of the current session.
    pub(super) fn on_ui_request(&mut self, request: Request) {
        use super::ExtensionReply;
        use super::selectors::{ChoiceDialog, Countdown, InputDialog, Selector, TextDialog};
        let countdown = |timeout: Option<Duration>| {
            timeout.map(|timeout| Countdown(std::time::Instant::now() + timeout))
        };
        match request {
            Request::Select {
                title,
                options,
                timeout,
                reply,
            } => {
                let labels: Vec<&str> = options.iter().map(String::as_str).collect();
                let mut dialog = ChoiceDialog::new(&title, &labels);
                dialog.countdown = countdown(timeout);
                self.open_extension_dialog(
                    Selector::Choice(dialog),
                    Some(ExtensionReply::Select(options, reply)),
                );
            }
            Request::Confirm {
                title,
                message,
                timeout,
                reply,
            } => {
                let mut dialog = ChoiceDialog::new(&format!("{title}\n{message}"), &["Yes", "No"]);
                dialog.countdown = countdown(timeout);
                self.open_extension_dialog(
                    Selector::Choice(dialog),
                    Some(ExtensionReply::Confirm(reply)),
                );
            }
            Request::Input {
                title,
                timeout,
                reply,
            } => {
                let mut dialog = InputDialog::new(&title);
                dialog.countdown = countdown(timeout);
                self.open_extension_dialog(
                    Selector::Input(Box::new(dialog)),
                    Some(ExtensionReply::Text(reply)),
                );
            }
            Request::Editor {
                title,
                prefill,
                reply,
            } => {
                let mut dialog = TextDialog::new(&title, super::editor_theme(&self.theme));
                if let Some(prefill) = prefill {
                    dialog.prefill(&prefill);
                }
                self.open_extension_dialog(
                    Selector::Text(Box::new(dialog)),
                    Some(ExtensionReply::Text(reply)),
                );
            }
            Request::Status(key, text) => self.ext.set_status(key, text),
            Request::Widget(key, widget, placement) => {
                let view = widget.map(|widget| match widget {
                    Widget::Lines(lines) => WidgetView::Lines(lines),
                    Widget::Component(component) => {
                        WidgetView::Remote(RemoteView::new(component, self.tx.clone(), self.epoch))
                    }
                });
                self.ext
                    .set_widget(key, view, placement.unwrap_or_default());
            }
            Request::Footer(component) => {
                self.ext.footer = component
                    .map(|component| RemoteView::new(component, self.tx.clone(), self.epoch));
            }
            Request::Header(component) => {
                self.ext.header = component
                    .map(|component| RemoteView::new(component, self.tx.clone(), self.epoch));
                self.flat_key = None;
            }
            Request::Title(title) => super::emit(&format!("\x1b]0;{title}\x07")),
            Request::WorkingMessage(message) => self.ext.working_message = message,
            Request::WorkingVisible(visible) => self.set_working_visible(visible),
            Request::WorkingIndicator(indicator) => {
                self.ext.working_indicator = indicator;
                if let Some((Indicator::Working, started)) = &mut self.indicator {
                    *started = Instant::now();
                }
            }
            Request::HiddenThinkingLabel(label) => {
                self.ext.thinking_label = label;
                self.invalidate_all();
            }
            Request::EditorText(text) => self.editor.set_text(&text),
            Request::Paste(text) => {
                self.editor
                    .handle_input(&format!("\x1b[200~{text}\x1b[201~"), &self.keys);
            }
            Request::Custom(component, options) => {
                let view = RemoteView::new(component, self.tx.clone(), self.epoch);
                // An overlay over an open overlay stacks on it.
                if options.overlay
                    && let Some(below) = self.overlay.take()
                    && let Some(Selector::Remote(covered)) = self.selector.take()
                {
                    self.overlays_below.push((*covered, below));
                    self.selector = Some(Selector::Remote(Box::new(view)));
                    self.overlay = Some(options.overlay_options);
                    return;
                }
                self.open_extension_dialog(Selector::Remote(Box::new(view)), None);
                self.overlay = options.overlay.then_some(options.overlay_options);
            }
            Request::Close(component) => {
                if matches!(&self.selector, Some(Selector::Remote(view)) if view.key() == component.key())
                {
                    self.selector = None;
                    self.overlay = None;
                    // The overlay it covered takes the keys again.
                    if let Some((view, options)) = self.overlays_below.pop() {
                        self.selector = Some(Selector::Remote(Box::new(view)));
                        self.overlay = Some(options);
                    }
                } else {
                    self.overlays_below
                        .retain(|(view, _)| view.key() != component.key());
                }
            }
            Request::Render => {
                for view in self.ext.views() {
                    view.invalidate();
                }
                let items: Vec<usize> = self
                    .transcript_views()
                    .into_iter()
                    .map(|(index, view)| {
                        view.invalidate();
                        index
                    })
                    .collect();
                for index in items {
                    self.touch(index);
                }
                if let Some(Selector::Remote(view)) = &mut self.selector {
                    view.invalidate();
                }
                for (view, _) in &mut self.overlays_below {
                    view.invalidate();
                }
            }
            // pi's shutdown handler: exit now when idle, else once the agent
            // settles.
            Request::Shutdown => {
                self.shutdown_requested = true;
                if !self.running {
                    self.quit = true;
                }
            }
            Request::ToolsExpanded(expanded) => {
                if expanded != self.expanded {
                    self.expanded = expanded;
                    self.invalidate_all();
                    self.redraw_transcript();
                }
            }
        }
    }

    /// Shows `selector` in the editor's place for an extension. As in pi, it
    /// replaces whatever selector is open; an extension dialog it replaces is
    /// answered as cancelled.
    fn open_extension_dialog(
        &mut self,
        selector: super::selectors::Selector,
        reply: Option<super::ExtensionReply>,
    ) {
        if let Some(super::Dialog::Extension(previous)) = self.dialog.take() {
            previous.cancel();
        }
        self.selector = Some(selector);
        self.dialog = reply.map(super::Dialog::Extension);
    }

    /// Takes the lines of a component render.
    pub(super) fn on_rendered(&mut self, key: (u64, u32), width: usize, lines: &[String]) {
        use super::selectors::Selector;
        if let Some(Selector::Remote(view)) = &mut self.selector
            && view.key() == key
        {
            view.rendered(width, lines);
        }
        for (view, _) in &mut self.overlays_below {
            if view.key() == key {
                view.rendered(width, lines);
            }
        }
        let touched: Vec<usize> = self
            .transcript_views()
            .into_iter()
            .filter(|(_, view)| view.key() == key && view.rendered(width, lines))
            .map(|(index, _)| index)
            .collect();
        for index in touched {
            self.touch(index);
        }
        let header = self.ext.header.as_ref().map(RemoteView::key);
        let mut header_changed = false;
        for view in self.ext.views() {
            if view.key() == key && view.rendered(width, lines) && Some(key) == header {
                header_changed = true;
            }
        }
        if header_changed {
            self.flat_key = None;
        }
    }

    /// pi's `resetExtensionUI`, when the session is replaced.
    pub(super) fn reset_extension_ui(&mut self) {
        use super::selectors::Selector;
        if let Some(super::Dialog::Extension(reply)) = self.dialog.take() {
            reply.cancel();
            self.selector = None;
        }
        if matches!(&self.selector, Some(Selector::Remote(_))) {
            self.selector = None;
        }
        self.overlays_below.clear();
        self.ext.reset();
        self.invalidate_all();
    }

    /// The countdown of the open extension dialog.
    pub(super) fn countdown(&self) -> Option<super::selectors::Countdown> {
        use super::selectors::Selector;
        match &self.selector {
            Some(Selector::Choice(dialog)) => dialog.countdown,
            Some(Selector::Input(dialog)) => dialog.countdown,
            _ => None,
        }
    }

    /// Cancels the open dialog once its countdown runs out.
    pub(super) fn expire_dialog(&mut self) {
        if self
            .countdown()
            .is_some_and(|countdown| std::time::Instant::now() >= countdown.0)
        {
            self.selector = None;
            let dialog = self.dialog.take();
            self.on_cancel(dialog);
        }
    }
}

/// A transcript item an extension draws.
pub(super) enum Slot {
    ToolCall(String),
    ToolResult(String),
    /// A custom message, by its key.
    Message(String),
}

impl super::App {
    fn request_component(
        &self,
        extension: Arc<dyn yapi_core::extensions::Extension>,
        request: Value,
        slot: Slot,
        sequence: u64,
    ) {
        let (tx, epoch) = (self.tx.clone(), self.epoch);
        tokio::spawn(async move {
            let component = extension.component(&request).await;
            let _ = tx.send(Event::Component(epoch, Box::new(slot), sequence, component));
        });
    }

    /// Asks the extension that draws tool item `index` for its components,
    /// as pi rebuilds them whenever the call changes.
    pub(super) fn draw_tool(&mut self, index: usize) {
        use super::chat::Item;
        let Some(id) = self
            .tool_items
            .iter()
            .find(|(_, item)| **item == index)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let session = self.session.clone();
        let (cwd, expanded) = (self.cwd.clone(), self.expanded);
        let Some(Item::Tool(view)) = self.chat.get_mut(index) else {
            return;
        };
        if view.draw.is_none() {
            let Some((extension, renderers)) = session.tool_renderer(&view.name) else {
                return;
            };
            view.draw = Some(super::tools::ToolDraw {
                extension,
                renderers,
                call: None,
                result: None,
                requests: [0, 0],
            });
        }
        let partial = view.result.is_none();
        let shown = view.result.clone().or_else(|| view.partial.clone());
        let context = json!({
            "cwd": cwd,
            "executionStarted": view.started.is_some(),
            "argsComplete": view.started.is_some() || !partial,
            "isPartial": partial,
            "expanded": expanded,
            "showImages": true,
            "isError": view.is_error,
        });
        let (name, args) = (view.name.clone(), view.args.clone());
        let Some(draw) = view.draw.as_mut() else {
            return;
        };
        let mut requests = Vec::new();
        if draw.renderers.call {
            draw.requests[0] += 1;
            requests.push((
                json!({"kind": "toolCall", "name": name, "toolCallId": id, "args": args, "context": context}),
                Slot::ToolCall(id.clone()),
                draw.requests[0],
            ));
        }
        if draw.renderers.result
            && let Some(result) = shown
        {
            draw.requests[1] += 1;
            requests.push((
                json!({
                    "kind": "toolResult", "name": name, "toolCallId": id, "args": args,
                    "result": {"content": result.content, "details": result.details},
                    "options": {"expanded": expanded, "isPartial": partial},
                    "context": context,
                }),
                Slot::ToolResult(id.clone()),
                draw.requests[1],
            ));
        }
        let extension = draw.extension.clone();
        for (request, slot, sequence) in requests {
            self.request_component(extension.clone(), request, slot, sequence);
        }
    }

    /// Asks the extension that draws custom message item `index` for its
    /// component.
    pub(super) fn draw_custom(&mut self, index: usize) {
        use super::chat::Item;
        let expanded = self.expanded;
        let output_pad = self.output_pad;
        let Some(Item::Custom(custom)) = self.chat.get_mut(index) else {
            return;
        };
        let Some(extension) = custom.renderer.clone() else {
            return;
        };
        custom.requests += 1;
        let request = json!({
            "kind": "message",
            "key": custom.key,
            "message": custom.message,
            "options": {"expanded": expanded, "outputPad": output_pad},
        });
        let (slot, sequence) = (Slot::Message(custom.key.clone()), custom.requests);
        self.request_component(extension, request, slot, sequence);
    }

    /// Adds custom message `message` to the transcript when it is shown.
    pub(super) fn push_custom(&mut self, message: yapi_types::message::CustomMessage) {
        if !message.display {
            return;
        }
        let renderer = self.session.message_renderer(&message.custom_type);
        let key = format!("{}:{}", message.timestamp, self.chat.len());
        let index = self.push(super::chat::Item::Custom(Box::new(
            super::chat::CustomView {
                message,
                key,
                renderer,
                view: None,
                requests: 0,
            },
        )));
        self.draw_custom(index);
    }

    /// Takes a component an extension built for a transcript item.
    pub(super) fn on_component(
        &mut self,
        slot: Slot,
        sequence: u64,
        component: Option<RemoteComponent>,
    ) {
        use super::chat::Item;
        let (tx, epoch) = (self.tx.clone(), self.epoch);
        let place = |current: &mut Option<RemoteView>| match component {
            Some(component) => match current {
                // The renderer returned its last component.
                Some(view) if view.key() == component.key() => view.invalidate(),
                _ => *current = Some(RemoteView::new(component, tx, epoch)),
            },
            None => *current = None,
        };
        let index = match &slot {
            Slot::ToolCall(id) | Slot::ToolResult(id) => self.tool_items.get(id).copied(),
            Slot::Message(key) => self
                .chat
                .iter()
                .position(|item| matches!(item, Item::Custom(custom) if custom.key == *key)),
        };
        let Some(index) = index else {
            return;
        };
        match (&slot, self.chat.get_mut(index)) {
            (Slot::ToolCall(_), Some(Item::Tool(view))) => {
                if let Some(draw) = view.draw.as_mut()
                    && draw.requests[0] == sequence
                {
                    place(&mut draw.call);
                }
            }
            (Slot::ToolResult(_), Some(Item::Tool(view))) => {
                if let Some(draw) = view.draw.as_mut()
                    && draw.requests[1] == sequence
                {
                    place(&mut draw.result);
                }
            }
            (Slot::Message(_), Some(Item::Custom(custom))) if custom.requests == sequence => {
                place(&mut custom.view);
            }
            _ => return,
        }
        self.touch(index);
    }

    /// The components of transcript items, with their item's index.
    pub(super) fn transcript_views(&self) -> Vec<(usize, &RemoteView)> {
        use super::chat::Item;
        let mut out = Vec::new();
        for (index, item) in self.chat.iter().enumerate() {
            match item {
                Item::Tool(view) => {
                    if let Some(draw) = &view.draw {
                        out.extend(
                            draw.call
                                .iter()
                                .chain(draw.result.iter())
                                .map(|view| (index, view)),
                        );
                    }
                }
                Item::Custom(custom) => out.extend(custom.view.iter().map(|view| (index, view))),
                _ => {}
            }
        }
        out
    }

    /// Asks again for every drawn item's components, as pi does when tool
    /// output expands or collapses.
    pub(super) fn redraw_transcript(&mut self) {
        use super::chat::Item;
        for index in 0..self.chat.len() {
            match &self.chat[index] {
                Item::Tool(_) => self.draw_tool(index),
                Item::Custom(custom) if custom.renderer.is_some() => self.draw_custom(index),
                _ => {}
            }
        }
    }
}

/// Where an overlay goes; pi-tui's `resolveOverlayLayout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OverlayLayout {
    pub width: usize,
    pub row: usize,
    pub col: usize,
    pub max_height: Option<usize>,
}

/// pi-tui's `parseSizeValue`: columns or rows, or a percentage of `reference`.
fn size_value(value: &Value, reference: usize) -> Option<i64> {
    if let Some(number) = value.as_f64() {
        return Some(number as i64);
    }
    let percent: f64 = value.as_str()?.strip_suffix('%')?.parse().ok()?;
    Some((reference as f64 * percent / 100.0).floor() as i64)
}

/// The layout of an overlay `height` rows tall with pi's `OverlayOptions`
/// `options` on a `width` by `height` terminal.
pub(super) fn overlay_layout(
    options: &Value,
    overlay_height: usize,
    term_width: usize,
    term_height: usize,
) -> OverlayLayout {
    let (term_width, term_height) = (term_width as i64, term_height as i64);
    let side = |name: &str| match &options["margin"] {
        Value::Number(number) => number.as_i64().unwrap_or(0).max(0),
        margin => margin[name].as_i64().unwrap_or(0).max(0),
    };
    let (top, right, bottom, left) = (side("top"), side("right"), side("bottom"), side("left"));
    let avail_width = (term_width - left - right).max(1);
    let avail_height = (term_height - top - bottom).max(1);
    let mut width =
        size_value(&options["width"], term_width as usize).unwrap_or(80.min(avail_width));
    if let Some(min) = options["minWidth"].as_i64() {
        width = width.max(min);
    }
    let width = width.clamp(1, avail_width);
    let max_height = size_value(&options["maxHeight"], term_height as usize)
        .map(|max| max.clamp(1, avail_height));
    let height = max_height.map_or(overlay_height as i64, |max| {
        (overlay_height as i64).min(max)
    });
    let anchor = options["anchor"].as_str().unwrap_or("center");
    let anchored_row = || match anchor {
        "top-left" | "top-center" | "top-right" => top,
        "bottom-left" | "bottom-center" | "bottom-right" => top + avail_height - height,
        _ => top + (avail_height - height).div_euclid(2),
    };
    let anchored_col = || match anchor {
        "top-left" | "left-center" | "bottom-left" => left,
        "top-right" | "right-center" | "bottom-right" => left + avail_width - width,
        _ => left + (avail_width - width).div_euclid(2),
    };
    let percent = |value: &Value| {
        value
            .as_str()
            .and_then(|text| text.strip_suffix('%'))
            .and_then(|text| text.parse::<f64>().ok())
    };
    let mut row = match &options["row"] {
        Value::Null => anchored_row(),
        value => match (value.as_i64(), percent(value)) {
            (Some(row), _) => row,
            (None, Some(percent)) => {
                top + ((avail_height - height).max(0) as f64 * percent / 100.0).floor() as i64
            }
            _ => anchored_row(),
        },
    };
    let mut col = match &options["col"] {
        Value::Null => anchored_col(),
        value => match (value.as_i64(), percent(value)) {
            (Some(col), _) => col,
            (None, Some(percent)) => {
                left + ((avail_width - width).max(0) as f64 * percent / 100.0).floor() as i64
            }
            _ => anchored_col(),
        },
    };
    row += options["offsetY"].as_i64().unwrap_or(0);
    col += options["offsetX"].as_i64().unwrap_or(0);
    let row = row.min(term_height - bottom - height).max(top);
    let col = col.min(term_width - right - width).max(left);
    OverlayLayout {
        width: width as usize,
        row: row.max(0) as usize,
        col: col.max(0) as usize,
        max_height: max_height.map(|max| max as usize),
    }
}

impl super::App {
    /// The overlay of the open custom component, as pi-tui composites it.
    /// The overlays of open custom components, bottom first, as pi-tui
    /// composites them.
    pub(super) fn overlays(&self) -> Vec<yapi_tui::screen::Overlay> {
        use super::selectors::Selector;
        let (Some(Selector::Remote(top)), Some(top_options)) = (&self.selector, &self.overlay)
        else {
            return Vec::new();
        };
        let (width, height) = self.size;
        self.overlays_below
            .iter()
            .map(|(view, options)| (view, options))
            .chain(std::iter::once((&**top, top_options)))
            .map(|(view, options)| {
                let sized = overlay_layout(options, 0, width, height);
                let (mut lines, _) = view.render(sized.width);
                if let Some(max) = sized.max_height {
                    lines.truncate(max);
                }
                let placed = overlay_layout(options, lines.len(), width, height);
                yapi_tui::screen::Overlay {
                    row: placed.row,
                    col: placed.col,
                    width: placed.width,
                    lines,
                }
            })
            .collect()
    }
}
