//! Extensions: code that registers tools and commands with a session and
//! handles its events, as pi's extension runner (`core/extensions` in pi
//! `v1.0.0`) runs them.
//!
//! This is the part of pi's `ExtensionAPI` that the built-in extensions use.
//! The wasm host in `ri-ext` implements [`Extension`] for loaded packages with
//! the same contract. Each session gets its own extension instances; a
//! replaced session receives `session_shutdown` before its successor starts.

pub mod discovery;
pub mod tool_search;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use ri_types::rpc::SourceInfo;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::tools::RegisteredTool;
use crate::tools::registry::ToolRegistry;

/// The mode a session runs in; pi's `ExtensionMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The interactive terminal UI.
    Tui,
    /// RPC over stdin and stdout.
    Rpc,
    /// Print mode.
    #[default]
    Print,
    /// JSON event mode.
    Json,
}

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

/// Dialogs and notifications for extensions; pi's `ExtensionUIContext`.
/// Methods a mode cannot show resolve as cancelled.
pub trait ExtensionUi: Send + Sync {
    /// Whether a person can answer dialogs.
    fn has_ui(&self) -> bool;

    /// Shows a message.
    fn notify(&self, message: &str, kind: NotifyKind);

    /// Asks to pick one of `options`; `None` when cancelled.
    fn select(&self, _title: &str, _options: Vec<String>) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    /// Asks for a line of text; `None` when cancelled. `cancel` dismisses it.
    fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    /// Asks a yes or no question; `false` when cancelled.
    fn confirm(&self, _title: &str, _message: &str) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
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

/// A tool as extensions see it; pi's `ToolInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Argument schema.
    pub parameters: Value,
    /// How it reaches the model.
    pub exposure: crate::tools::Exposure,
    /// Its group.
    pub namespace: Option<crate::tools::Namespace>,
}

/// The session's tools, shared with its extensions.
#[derive(Clone, Default)]
pub struct Tools(Arc<Mutex<ToolRegistry>>);

impl Tools {
    /// Wraps a registry.
    pub fn new(registry: ToolRegistry) -> Tools {
        Tools(Arc::new(Mutex::new(registry)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ToolRegistry> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `f` with the registry.
    pub fn with<T>(&self, f: impl FnOnce(&mut ToolRegistry) -> T) -> T {
        f(&mut self.lock())
    }

    /// pi's `registerTool`.
    pub fn register(&self, tool: RegisteredTool) {
        self.lock().register(tool);
    }

    /// pi's `getActiveTools`.
    pub fn active(&self) -> Vec<String> {
        self.lock().active()
    }

    /// pi's `setActiveTools`.
    pub fn set_active(&self, names: Vec<String>) {
        self.lock().set_active(names);
    }

    /// pi's `getAllTools`.
    pub fn all(&self) -> Vec<ToolInfo> {
        self.lock()
            .all()
            .into_iter()
            .map(|tool| {
                let declaration = tool.tool.declaration();
                ToolInfo {
                    name: declaration.name.clone(),
                    description: declaration.description.clone(),
                    parameters: declaration.parameters.clone(),
                    exposure: tool.exposure,
                    namespace: tool.namespace,
                }
            })
            .collect()
    }
}

/// What an extension can reach while handling an event or command.
#[derive(Clone)]
pub struct Context {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The agent directory.
    pub agent_dir: PathBuf,
    /// Whether the project's resources are trusted.
    pub project_trusted: bool,
    /// The mode.
    pub mode: Mode,
    /// Dialogs and notifications.
    pub ui: Arc<dyn ExtensionUi>,
    /// The session's tools.
    pub tools: Tools,
    /// Cancelled when the current run is aborted.
    pub cancel: CancellationToken,
}

/// A slash command an extension registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// Name without the slash.
    pub name: String,
    /// What it does.
    pub description: String,
}

/// A completion for a command's argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Text that replaces the argument.
    pub value: String,
    /// Shown in the list.
    pub label: String,
    /// Shown next to the label.
    pub description: Option<String>,
}

/// An extension. Every hook has a no-op default.
pub trait Extension: Send + Sync {
    /// Where the extension comes from, as `get_commands` reports it.
    fn source(&self) -> SourceInfo;

    /// Registers the tools it offers from the start, before the session's
    /// active tools are chosen.
    fn load(&self, _tools: &Tools) {}

    /// The commands it handles.
    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }

    /// Completions for the argument of `command`; `None` offers none.
    fn complete(&self, _command: &str, _prefix: &str) -> Option<Vec<Completion>> {
        None
    }

    /// Runs `command` with its argument text.
    fn run_command<'a>(
        &'a self,
        _command: &'a str,
        _args: &'a str,
        _ctx: &'a Context,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// The session started or was bound to a mode.
    fn session_start<'a>(&'a self, _ctx: &'a Context) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// The session ends or is being replaced.
    fn session_shutdown<'a>(&'a self, _ctx: &'a Context) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// A prompt is about to run; `sections` are extra system prompt sections
    /// by tag name, which the extension may change.
    fn before_agent_start<'a>(
        &'a self,
        _ctx: &'a Context,
        _sections: &'a mut IndexMap<String, String>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// A tool is about to run; `Some(reason)` blocks it.
    fn tool_call<'a>(
        &'a self,
        _ctx: &'a Context,
        _tool: &'a str,
        _input: &'a Value,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

/// The built-in extensions each session starts with.
pub fn builtins() -> Vec<Arc<dyn Extension>> {
    vec![
        Arc::new(tool_search::ToolSearchExtension),
        Arc::new(crate::mcp::extension::McpExtension::new()),
    ]
}

/// pi's source info for a built-in extension named `name`.
pub fn builtin_source(name: &str) -> SourceInfo {
    SourceInfo {
        path: format!("builtin:{name}"),
        source: "builtin".into(),
        scope: "temporary".into(),
        origin: "top-level".into(),
        base_dir: None,
    }
}
