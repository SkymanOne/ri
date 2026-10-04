//! What extensions show: dialogs, notifications, footer statuses, widgets,
//! the editor and custom components; pi's `ExtensionUIContext`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// How a notification is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifyKind {
    /// A status message.
    #[default]
    Info,
    /// A warning.
    Warning,
    /// An error.
    Error,
}

impl NotifyKind {
    /// pi's name for the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            NotifyKind::Info => "info",
            NotifyKind::Warning => "warning",
            NotifyKind::Error => "error",
        }
    }
}

/// When a dialog closes on its own; pi's `ExtensionUIDialogOptions`.
#[derive(Clone, Debug, Default)]
pub struct DialogOptions {
    /// Closes the dialog as cancelled after this long. Interactive dialogs
    /// count down in their title.
    pub timeout: Option<Duration>,
    /// Closes the dialog as cancelled once cancelled.
    pub cancel: Option<CancellationToken>,
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
#[derive(Clone, Debug)]
pub enum Widget {
    /// Lines of text with escape sequences.
    Lines(Vec<String>),
    /// A component the extension renders.
    Component(RemoteComponent),
}

/// The extension runtime side of components: renders them and delivers
/// input to them.
pub trait ComponentHost: Send + Sync {
    /// The lines of component `handle` at `width` columns: text with escape
    /// sequences, and pi-tui's cursor marker where a focused component wants
    /// the cursor. Empty once the component is gone.
    fn render(&self, handle: u32, width: u16) -> BoxFuture<'static, Vec<String>>;

    /// Delivers raw terminal input to component `handle`.
    fn input(&self, handle: u32, data: &str);
}

/// A pi-tui component that lives in an extension runtime.
#[derive(Clone)]
pub struct RemoteComponent {
    runtime: u64,
    handle: u32,
    host: Arc<dyn ComponentHost>,
}

impl std::fmt::Debug for RemoteComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteComponent")
            .field("runtime", &self.runtime)
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl RemoteComponent {
    /// Component `handle` of `host`, the runtime numbered `runtime`.
    pub fn new(runtime: u64, handle: u32, host: Arc<dyn ComponentHost>) -> RemoteComponent {
        RemoteComponent {
            runtime,
            handle,
            host,
        }
    }

    /// What identifies it across runtimes: the runtime's number and its
    /// handle there.
    pub fn key(&self) -> (u64, u32) {
        (self.runtime, self.handle)
    }

    /// Its lines at `width` columns; see [`ComponentHost::render`].
    pub fn render(&self, width: u16) -> BoxFuture<'static, Vec<String>> {
        self.host.render(self.handle, width)
    }

    /// Delivers raw terminal input.
    pub fn input(&self, data: &str) {
        self.host.input(self.handle, data);
    }
}

/// How a custom component is shown; the options of pi's `custom`.
#[derive(Clone, Debug, Default)]
pub struct CustomOptions {
    /// Over the screen instead of in the editor's place.
    pub overlay: bool,
    /// pi's `OverlayOptions`: anchor, size and margins.
    pub overlay_options: Value,
}

/// pi's `ExtensionUIContext` as a mode provides it. Dialogs a mode cannot
/// show resolve as cancelled; everything else does nothing by default.
pub trait ExtensionUi: Send + Sync {
    /// Whether a person can answer dialogs.
    fn has_ui(&self) -> bool;

    /// pi's `ctx.shutdown()`: the mode exits once the agent is idle.
    fn shutdown(&self) {}

    /// Shows a message.
    fn notify(&self, message: &str, kind: NotifyKind);

    /// Asks to pick one of `options`; `None` when cancelled.
    fn select(
        &self,
        _title: &str,
        _options: Vec<String>,
        _dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    /// Asks for a line of text; `None` when cancelled.
    fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    /// Asks a yes or no question; `false` when cancelled.
    fn confirm(
        &self,
        _title: &str,
        _message: &str,
        _dialog: DialogOptions,
    ) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }

    /// Asks for multi-line text starting from `prefill`; `None` when
    /// cancelled.
    fn editor(&self, _title: &str, _prefill: Option<&str>) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    /// Sets the footer status `key`, or clears it.
    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    /// Sets widget `key`, or removes it. Without a placement it goes above
    /// the editor.
    fn set_widget(&self, _key: &str, _widget: Option<Widget>, _placement: Option<Placement>) {}

    /// Replaces the footer, or restores the built-in one.
    fn set_footer(&self, _component: Option<RemoteComponent>) {}

    /// Replaces the startup header, or restores the built-in one.
    fn set_header(&self, _component: Option<RemoteComponent>) {}

    /// Sets the terminal title.
    fn set_title(&self, _title: &str) {}

    /// Sets the message shown while the agent works, or restores the default.
    fn set_working_message(&self, _message: Option<&str>) {}

    /// Sets the label of hidden thinking blocks, or restores the default.
    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}

    /// Replaces the editor's text.
    fn set_editor_text(&self, _text: &str) {}

    /// Pastes into the editor as a terminal paste would.
    fn paste_to_editor(&self, text: &str) {
        self.set_editor_text(text);
    }

    /// The editor's text.
    fn editor_text(&self) -> String {
        String::new()
    }

    /// Whether [`ExtensionUi::custom`] and component widgets are shown.
    fn shows_components(&self) -> bool {
        false
    }

    /// Shows `component` with keyboard focus until [`ExtensionUi::close`].
    fn custom(&self, _component: RemoteComponent, _options: CustomOptions) {}

    /// Removes the custom component `component`.
    fn close(&self, _component: RemoteComponent) {}

    /// Components changed and need rendering.
    fn request_render(&self) {}

    /// Whether tool output is expanded.
    fn tools_expanded(&self) -> bool {
        false
    }

    /// Expands or collapses tool output.
    fn set_tools_expanded(&self, _expanded: bool) {}

    /// What custom footers read: `{"gitBranch", "statuses": [[key, text]],
    /// "providers"}`.
    fn footer_data(&self) -> Value {
        Value::Null
    }

    /// The theme as escape sequences: `{"name", "mode", "fg": {token:
    /// sequence}, "bg": {token: sequence}, "dim": [token]}`. `Null` leaves
    /// text unstyled.
    fn theme(&self) -> Value {
        Value::Null
    }

    /// A handler of extension `path` failed on `event`; print mode's report
    /// by default.
    fn extension_error(&self, path: &str, _event: &str, error: &str) {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "Extension error ({path}): {error}");
    }
}

/// The UI of print and JSON modes: nothing is shown.
pub struct NoUi;

impl ExtensionUi for NoUi {
    fn has_ui(&self) -> bool {
        false
    }

    fn notify(&self, _message: &str, _kind: NotifyKind) {}
}
