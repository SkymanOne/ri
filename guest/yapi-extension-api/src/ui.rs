//! Interface components: pi-tui's `Component` model for native extensions.
//!
//! A component renders lines of text with ANSI escape sequences for a width
//! and handles the raw terminal input it gets while it has focus. yapi
//! renders it whenever it may have changed, after input and after
//! [`request_render`], and paints the last lines it rendered meanwhile, so
//! the interface never waits for the extension.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use serde_json::{Value, json};

use crate::{Context, request};

/// pi-tui's `Component`: lines for a width, and the input it handles.
pub trait Component {
    /// The lines at `width` columns: text with ANSI escape sequences, and
    /// pi-tui's cursor marker where the component wants the terminal's
    /// cursor. yapi cuts each line to `width` columns.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Raw terminal input, such as `"\x1b[A"` for Up, while the component
    /// has focus. [`parse_key`] names common keys.
    fn handle_input(&mut self, _data: &str) {}

    /// pi-tui's `TuiMouseEvent` `event`, in fullscreen mode; whether the
    /// component took it.
    fn handle_mouse(&mut self, _event: &Value) -> bool {
        false
    }

    /// The width an overlay [`Context::custom`] shows it at when its
    /// options set none, as pi-tui reads a component's `width`.
    fn width(&self) -> Option<usize> {
        None
    }
}

/// pi's editor component, which [`Context::set_editor_component`] puts in
/// place of the built-in editor. It gets the keys the editor would, and
/// reports its text with [`editor_changed`] and [`editor_submit`].
pub trait EditorComponent: Component {
    /// Replaces the text.
    fn set_text(&mut self, text: &str);

    /// Adds a submitted prompt to the history.
    fn add_to_history(&mut self, _text: &str) {}

    /// Inserts `text` at the cursor. `apart` sets it apart from the words
    /// around the cursor with spaces, as pi does for pasted file paths.
    fn insert_text_at_cursor(&mut self, text: &str, apart: bool);

    /// Takes the built-in editor's settings: `{"border", "paddingX",
    /// "autocompleteMaxVisible", "focused", "rows"}`, where `border` is a
    /// thinking level or `bashMode`.
    fn configure(&mut self, _config: &Value) {}
}

type Mounted = Rc<RefCell<Box<dyn Component>>>;
type MountedEditor = Rc<RefCell<Box<dyn EditorComponent>>>;

#[derive(Default)]
struct Components {
    next: u32,
    mounted: HashMap<u32, Mounted>,
    /// The handle shown by each request kind and key: widgets by their key,
    /// the footer, the header and the editor by an empty one, and the
    /// renders of transcript items by tool call or message.
    shown: HashMap<(&'static str, String), u32>,
    /// The editor component and its handle.
    editor: Option<(u32, MountedEditor)>,
    /// Renderer state by tool call.
    states: HashMap<String, Value>,
}

/// An editor in the registry of components.
struct EditorView(MountedEditor);

impl Component for EditorView {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.0.borrow_mut().render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.0.borrow_mut().handle_input(data);
    }

    fn handle_mouse(&mut self, event: &Value) -> bool {
        self.0.borrow_mut().handle_mouse(event)
    }
}

thread_local! {
    static COMPONENTS: RefCell<Components> = RefCell::default();
    /// The bound session's theme, once a component asked for it.
    static THEME: RefCell<Option<Rc<Theme>>> = const { RefCell::new(None) };
}

fn mount(component: Box<dyn Component>) -> u32 {
    COMPONENTS.with(|components| {
        let mut components = components.borrow_mut();
        components.next += 1;
        let handle = components.next;
        components
            .mounted
            .insert(handle, Rc::new(RefCell::new(component)));
        handle
    })
}

fn unmount(handle: u32) {
    // Dropped outside the borrow, so a component's `Drop` may use the API.
    let component = COMPONENTS.with(|components| components.borrow_mut().mounted.remove(&handle));
    drop(component);
}

/// Component `handle`, taken out of the registry's borrow so it can show or
/// close components while it runs.
fn mounted(handle: u32) -> Option<Mounted> {
    COMPONENTS.with(|components| components.borrow().mounted.get(&handle).cloned())
}

/// Shows `handle` for `kind` and `key` in place of the component shown
/// there before, which is dropped.
fn replace(kind: &'static str, key: &str, handle: Option<u32>) {
    let previous = COMPONENTS.with(|components| {
        let shown = &mut components.borrow_mut().shown;
        match handle {
            Some(handle) => shown.insert((kind, key.to_owned()), handle),
            None => shown.remove(&(kind, key.to_owned())),
        }
    });
    if let Some(previous) = previous {
        unmount(previous);
    }
}

pub(crate) fn render(handle: u32, width: u32) -> Vec<String> {
    match mounted(handle) {
        Some(component) => component.borrow_mut().render(width as usize),
        None => Vec::new(),
    }
}

pub(crate) fn input(handle: u32, data: &str) {
    if let Some(component) = mounted(handle) {
        component.borrow_mut().handle_input(data);
    }
}

/// The `mouse` call: `{"handle", "event"}`; whether the component took it.
pub(crate) fn mouse(payload: &Value) -> bool {
    let handle = payload["handle"]
        .as_u64()
        .and_then(|handle| u32::try_from(handle).ok());
    handle
        .and_then(mounted)
        .is_some_and(|component| component.borrow_mut().handle_mouse(&payload["event"]))
}

/// A new session, whose theme [`theme`] asks for.
pub(crate) fn bind() {
    THEME.with(|theme| theme.take());
}

/// The extension is about to load again for another session: every
/// component is dropped.
pub(crate) fn reset() {
    // Handles keep counting, so none names an earlier session's component.
    let dropped = COMPONENTS.with(|components| {
        let mut components = components.borrow_mut();
        components.shown.clear();
        components.states.clear();
        (
            std::mem::take(&mut components.mounted),
            components.editor.take(),
        )
    });
    drop(dropped);
}

/// The `editor` call: operation `{"handle", "op", ...}` for the editor
/// component; see [`EditorComponent`].
pub(crate) fn editor_op(payload: &Value) {
    let editor = COMPONENTS.with(|components| {
        let components = components.borrow();
        let (handle, editor) = components.editor.as_ref()?;
        (payload["handle"] == *handle).then(|| editor.clone())
    });
    let Some(editor) = editor else {
        return;
    };
    let mut editor = editor.borrow_mut();
    let text = payload["text"].as_str().unwrap_or_default();
    match payload["op"].as_str() {
        Some("setText") => editor.set_text(text),
        Some("addToHistory") => editor.add_to_history(text),
        Some("insertTextAtCursor") => editor.insert_text_at_cursor(text, payload["apart"] == true),
        Some("configure") => editor.configure(payload),
        _ => {}
    }
}

/// The `component` call: what a tool's or a message's renderer draws,
/// `{"handle"}`, or `null` for yapi's own rendering.
pub(crate) fn transcript(
    payload: &Value,
    draw: impl FnOnce(&mut Value) -> Option<Box<dyn Component>>,
) -> Value {
    let (kind, key) = match payload["kind"].as_str() {
        Some("message") => ("message", &payload["key"]),
        Some("toolCall") => ("toolCall", &payload["toolCallId"]),
        _ => ("toolResult", &payload["toolCallId"]),
    };
    let key = key.as_str().unwrap_or_default().to_owned();
    let mut state = COMPONENTS.with(|components| {
        let mut components = components.borrow_mut();
        components.states.remove(&key).unwrap_or_else(|| json!({}))
    });
    let component = draw(&mut state);
    COMPONENTS.with(|components| components.borrow_mut().states.insert(key.clone(), state));
    let handle = component.map(mount);
    replace(kind, &key, handle);
    json!(handle.map(|handle| json!({"handle": handle})))
}

/// Sends the text an [`EditorComponent`] now holds, paste markers expanded,
/// so yapi's own editor keeps a copy; pi's `onChange`.
pub fn editor_changed(text: &str) {
    let _ = request("ui.editorChange", &json!({"text": text}));
}

/// Submits the prompt an [`EditorComponent`] holds; pi's `onSubmit`.
pub fn editor_submit(text: &str) {
    let _ = request("ui.editorSubmit", &json!({"text": text}));
}

/// Runs app action `action`, a key binding id such as `app.interrupt` or
/// `app.exit`, as the built-in editor does for its keys.
pub fn editor_action(action: &str) {
    let _ = request("ui.editorAction", &json!({"action": action}));
}

/// Runs the extension shortcut raw input `data` is bound to; whether there
/// was one.
pub fn editor_shortcut(data: &str) -> bool {
    request("ui.editorShortcut", &json!({"data": data})).is_ok_and(|ran| ran == true)
}

/// The terminal's size in columns and rows, as pi-tui's `terminal.columns`
/// and `terminal.rows` give it; `None` without a terminal, as in print mode.
pub fn terminal_size() -> Option<(usize, usize)> {
    let size = request("ui.terminalSize", &json!({})).ok()?;
    let dimension = |key: &str| size[key].as_u64().map(|value| value as usize);
    Some((dimension("columns")?, dimension("rows")?))
}

/// Asks yapi to render the components again, as pi-tui's
/// `tui.requestRender()` does. Call it when a component changed other than
/// in response to input, such as from a timer.
pub fn request_render() {
    let _ = request("ui.requestRender", &json!({}));
}

/// Where a widget goes; pi's `WidgetPlacement`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// Above the editor.
    #[default]
    AboveEditor,
    /// Below the editor.
    BelowEditor,
}

/// What a widget shows.
pub enum Widget {
    /// Lines of text with escape sequences.
    Lines(Vec<String>),
    /// A component.
    Component(Box<dyn Component>),
}

/// How [`Context::custom`] shows its component; the options of pi's
/// `ctx.ui.custom`.
#[derive(Clone, Default)]
pub struct CustomOptions {
    /// Over the screen instead of in the editor's place.
    pub overlay: bool,
    /// pi's `OverlayOptions`, such as `{"width": 70, "anchor": "center"}`.
    /// Without them an overlay takes the component's [`Component::width`].
    pub overlay_options: Value,
    /// Receives the component's [`OverlayHandle`] once it is shown; pi's
    /// `onHandle`.
    pub on_handle: Option<Rc<dyn Fn(OverlayHandle)>>,
}

/// pi-tui's `OverlayHandle` for a component [`Context::custom`] shows.
#[derive(Clone, Debug)]
pub struct OverlayHandle {
    handle: u32,
    hidden: Rc<std::cell::Cell<bool>>,
}

impl OverlayHandle {
    /// Closes the component without finishing it. Prefer [`Done::finish`].
    pub fn hide(&self) {
        let _ = request("ui.close", &json!({"handle": self.handle}));
    }

    /// Marks the overlay hidden or shown. yapi keeps showing it either way.
    pub fn set_hidden(&self, hidden: bool) {
        self.hidden.set(hidden);
    }

    /// Whether [`OverlayHandle::set_hidden`] last marked it hidden.
    pub fn is_hidden(&self) -> bool {
        self.hidden.get()
    }

    /// Does nothing: the shown component keeps keyboard focus.
    pub fn focus(&self) {}

    /// Does nothing: the shown component keeps keyboard focus.
    pub fn unfocus(&self) {}

    /// Whether it has keyboard focus: while it is not marked hidden.
    pub fn is_focused(&self) -> bool {
        !self.hidden.get()
    }
}

impl Context {
    /// Whether this mode shows components. The interactive mode does.
    pub(crate) fn shows_components(&self) -> bool {
        self.has_ui() && self.data["components"] == true
    }

    /// pi's `ctx.ui.setWidget`: shows `widget` as widget `key`, replacing
    /// the one shown as `key` before, or removes it. Modes that show no
    /// components leave out component widgets.
    pub fn set_widget(&self, key: &str, widget: Option<Widget>, placement: Placement) {
        let placement = match placement {
            Placement::AboveEditor => "aboveEditor",
            Placement::BelowEditor => "belowEditor",
        };
        let mut payload = json!({"key": key, "options": {"placement": placement}});
        let mut handle = None;
        match widget {
            Some(Widget::Component(component)) if self.shows_components() => {
                handle = Some(mount(component));
                payload["handle"] = json!(handle);
            }
            Some(Widget::Component(_)) => return replace("ui.setWidget", key, None),
            Some(Widget::Lines(lines)) => payload["lines"] = json!(lines),
            None => {}
        }
        let _ = request("ui.setWidget", &payload);
        replace("ui.setWidget", key, handle);
    }

    /// pi's `ctx.ui.setFooter`: shows `component` in place of the footer,
    /// or restores the built-in one.
    pub fn set_footer(&self, component: Option<Box<dyn Component>>) {
        self.set_slot("ui.setFooter", component);
    }

    /// pi's `ctx.ui.setHeader`: shows `component` in place of the startup
    /// header, or restores the built-in one.
    pub fn set_header(&self, component: Option<Box<dyn Component>>) {
        self.set_slot("ui.setHeader", component);
    }

    /// pi's `ctx.ui.setEditorComponent`: puts `editor` in place of the
    /// built-in editor, with the built-in editor's text, or restores the
    /// built-in one. Modes that show no components keep their editor.
    pub fn set_editor_component(&self, editor: Option<Box<dyn EditorComponent>>) {
        if !self.shows_components() {
            return;
        }
        let text = crate::editor_text();
        let editor = editor.map(|mut editor| {
            editor.set_text(&text);
            Rc::new(RefCell::new(editor))
        });
        let handle = editor
            .clone()
            .map(|editor| mount(Box::new(EditorView(editor))));
        let previous = COMPONENTS.with(|components| {
            let mut components = components.borrow_mut();
            let previous = components.editor.take();
            components.editor = handle.zip(editor);
            previous
        });
        drop(previous);
        let _ = request("ui.setEditor", &json!({"handle": handle}));
        replace("ui.setEditor", "", handle);
    }

    fn set_slot(&self, kind: &'static str, component: Option<Box<dyn Component>>) {
        replace(kind, "", None);
        if !self.shows_components() {
            return;
        }
        let handle = component.map(mount);
        let _ = request(kind, &json!({"handle": handle}));
        replace(kind, "", handle);
    }

    /// pi's `ctx.ui.custom`: shows the component `build` returns with
    /// keyboard focus until it calls [`Done::finish`], and resolves to the
    /// value it finishes with. Resolves to `None` at once in modes that show
    /// no components.
    pub fn custom<T, C>(
        &self,
        build: impl FnOnce(Done<T>) -> C,
        options: CustomOptions,
    ) -> impl Future<Output = Option<T>> + 'static
    where
        T: 'static,
        C: Component + 'static,
    {
        let shown = self.shows_components();
        let state = Rc::new(RefCell::new(Custom {
            handle: None,
            finished: !shown,
            value: None,
            waker: None,
        }));
        if shown {
            let component = build(Done(state.clone()));
            // A component that finished while it was built is not shown.
            if !state.borrow().finished {
                let overlay_options = match (&options.overlay_options, component.width()) {
                    (Value::Null, Some(width)) => json!({"width": width}),
                    (options, _) => options.clone(),
                };
                let handle = mount(Box::new(component));
                state.borrow_mut().handle = Some(handle);
                let _ = request(
                    "ui.custom",
                    &json!({
                        "handle": handle,
                        "overlay": options.overlay,
                        "overlayOptions": overlay_options,
                    }),
                );
                if let Some(on_handle) = &options.on_handle {
                    on_handle(OverlayHandle {
                        handle,
                        hidden: Rc::default(),
                    });
                }
            }
        }
        Shown(state)
    }
}

struct Custom<T> {
    handle: Option<u32>,
    finished: bool,
    value: Option<T>,
    waker: Option<Waker>,
}

/// Ends a [`Context::custom`] interaction; pi's `done` callback.
pub struct Done<T>(Rc<RefCell<Custom<T>>>);

impl<T> Clone for Done<T> {
    fn clone(&self) -> Self {
        Done(self.0.clone())
    }
}

impl<T> Done<T> {
    /// Closes the component and resolves the `custom` future to `value`.
    /// Calls after the first do nothing.
    pub fn finish(&self, value: T) {
        let handle = {
            let mut custom = self.0.borrow_mut();
            if custom.finished {
                return;
            }
            custom.finished = true;
            custom.value = Some(value);
            if let Some(waker) = custom.waker.take() {
                waker.wake();
            }
            custom.handle
        };
        if let Some(handle) = handle {
            let _ = request("ui.close", &json!({"handle": handle}));
            unmount(handle);
        }
    }
}

struct Shown<T>(Rc<RefCell<Custom<T>>>);

impl<T> Future for Shown<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<T>> {
        let mut custom = self.0.borrow_mut();
        if custom.finished {
            return Poll::Ready(custom.value.take());
        }
        custom.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// pi's `Theme`: the escape sequences of the session's theme, by token such
/// as `accent`, `muted` or `success`. Without a theme, as in print mode,
/// and for a token the theme lacks, text stays plain.
#[derive(Debug, Default)]
pub struct Theme {
    fg: HashMap<String, String>,
    bg: HashMap<String, String>,
    dim: HashSet<String>,
    styled: bool,
}

/// The theme of the session the extension is bound to.
pub fn theme() -> Rc<Theme> {
    THEME.with(|theme| {
        theme
            .borrow_mut()
            .get_or_insert_with(|| {
                let spec = request("ui.theme", &json!({})).unwrap_or_default();
                Rc::new(Theme::load(&spec))
            })
            .clone()
    })
}

impl Theme {
    /// yapi's theme description: `{"fg": {token: sequence}, "bg": {token:
    /// sequence}, "dim": [token], ...}`, or `null`.
    fn load(spec: &Value) -> Theme {
        let sequences = |key: &str| {
            spec[key]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(token, sequence)| {
                    Some((token.clone(), sequence.as_str()?.to_owned()))
                })
                .collect()
        };
        Theme {
            fg: sequences("fg"),
            bg: sequences("bg"),
            dim: spec["dim"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|token| Some(token.as_str()?.to_owned()))
                .collect(),
            styled: spec.is_object(),
        }
    }

    /// `text` in token `token`'s foreground color.
    pub fn fg(&self, token: &str, text: &str) -> String {
        match self.fg.get(token) {
            Some(sequence) if self.dim.contains(token) => {
                format!("{sequence}\x1b[2m{text}\x1b[22;39m")
            }
            Some(sequence) => format!("{sequence}{text}\x1b[39m"),
            None => text.to_owned(),
        }
    }

    /// `text` on token `token`'s background color.
    pub fn bg(&self, token: &str, text: &str) -> String {
        match self.bg.get(token) {
            Some(sequence) => format!("{sequence}{text}\x1b[49m"),
            None => text.to_owned(),
        }
    }

    fn wrap(&self, open: u8, close: u8, text: &str) -> String {
        if self.styled {
            format!("\x1b[{open}m{text}\x1b[{close}m")
        } else {
            text.to_owned()
        }
    }

    /// `text` in bold.
    pub fn bold(&self, text: &str) -> String {
        self.wrap(1, 22, text)
    }

    /// `text` in italics.
    pub fn italic(&self, text: &str) -> String {
        self.wrap(3, 23, text)
    }

    /// `text` underlined.
    pub fn underline(&self, text: &str) -> String {
        self.wrap(4, 24, text)
    }

    /// `text` struck through.
    pub fn strikethrough(&self, text: &str) -> String {
        self.wrap(9, 29, text)
    }

    /// `text` with its colors swapped.
    pub fn inverse(&self, text: &str) -> String {
        self.wrap(7, 27, text)
    }
}

/// The key raw terminal input `data` is, as pi-tui's `parseKey` names it,
/// for the keys components most often handle: `up`, `down`, `left`,
/// `right`, `enter`, `escape`, `tab`, `backspace`, and those keys and
/// letters with Ctrl, such as `ctrl+c`. Reads both the legacy encoding and
/// the Kitty keyboard protocol's, which yapi turns on where the terminal
/// supports it. `None` for anything else, such as typed text.
pub fn parse_key(data: &str) -> Option<String> {
    let name = match data {
        "\x1b" => "escape",
        "\r" => "enter",
        "\t" => "tab",
        "\x7f" => "backspace",
        "\x1b[A" | "\x1bOA" => "up",
        "\x1b[B" | "\x1bOB" => "down",
        "\x1b[C" | "\x1bOC" => "right",
        "\x1b[D" | "\x1bOD" => "left",
        _ => "",
    };
    if !name.is_empty() {
        return Some(name.to_owned());
    }
    if let [byte @ 1..=26] = data.as_bytes() {
        return Some(format!("ctrl+{}", char::from(b'a' + byte - 1)));
    }
    // Kitty: `ESC [ code [: alternates] [; modifiers [: event]] u`, or
    // `ESC [ 1 ; modifiers [: event] A` for the arrows.
    let body = data.strip_prefix("\x1b[")?;
    let split = body.len().checked_sub(1)?;
    let (body, last) = (body.get(..split)?, body.get(split..)?);
    let mut fields = body.split(';');
    let code: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let (modifiers, event) = match fields.next() {
        Some(field) => {
            let mut parts = field.split(':');
            let modifiers: u32 = parts.next()?.parse().ok()?;
            (modifiers, parts.next().unwrap_or("1"))
        }
        None => (1, "1"),
    };
    // A key release, or more than pi-tui's fields.
    if event == "3" || fields.next().is_some() {
        return None;
    }
    let key = match (last, code) {
        ("u", 27) => "escape".to_owned(),
        ("u", 13) => "enter".to_owned(),
        ("u", 9) => "tab".to_owned(),
        ("u", 127) => "backspace".to_owned(),
        ("u", 97..=122) => char::from_u32(code)?.to_string(),
        ("A", 1) => "up".to_owned(),
        ("B", 1) => "down".to_owned(),
        ("C", 1) => "right".to_owned(),
        ("D", 1) => "left".to_owned(),
        _ => return None,
    };
    match modifiers {
        1 => Some(key),
        5 => Some(format!("ctrl+{key}")),
        _ => None,
    }
}
