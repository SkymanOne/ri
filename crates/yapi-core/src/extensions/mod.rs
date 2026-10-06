//! Extensions: code that registers tools and commands with a session and
//! handles its events, as pi's extension runner (`core/extensions` in pi
//! `v1.0.0`) runs them.
//!
//! This is the part of pi's `ExtensionAPI` that the built-in extensions use.
//! The wasm host in `yapi-ext` implements [`Extension`] for loaded packages with
//! the same contract. Each session gets its own extension instances; a
//! replaced session receives `session_shutdown` before its successor starts.

pub mod codemode;
pub mod discovery;
pub mod tool_search;
mod ui;

pub use ui::{
    ComponentHost, CustomOptions, DialogOptions, ExtensionUi, NoUi, NotifyKind, Placement,
    RemoteComponent, Widget, WorkingIndicator,
};

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use yapi_types::autocomplete::ArgumentCompletions;
use yapi_types::rpc::SourceInfo;

use crate::agent_session::WeakSession;
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
        yapi_types::sync::lock(&self.0)
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
    /// The session, for extensions that act on it later.
    pub session: WeakSession,
}

/// A slash command an extension registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// Name without the slash.
    pub name: String,
    /// What it does.
    pub description: String,
}

/// An extension command with the name it is invoked by; pi's
/// `ResolvedCommand`.
#[derive(Clone)]
pub struct ResolvedCommand {
    /// `name`, or `name:N` when several extensions register `name`.
    pub invocation: String,
    /// The command as registered.
    pub command: Command,
    /// The extension that handles it.
    pub extension: Arc<dyn Extension>,
}

/// A key an extension binds; pi's `registerShortcut`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    /// The key id, such as `alt+u`.
    pub key: String,
    /// What it does, for `/hotkeys`.
    pub description: Option<String>,
}

/// An extension shortcut the interactive mode honors.
#[derive(Clone)]
pub struct ShortcutBinding {
    /// The key id, lowercased.
    pub key: String,
    /// The key id as the extension registered it.
    pub registered: String,
    /// What it does.
    pub description: Option<String>,
    /// The extension's path.
    pub path: String,
    /// The extension.
    pub extension: Arc<dyn Extension>,
}

/// Actions whose keys extensions may not take; pi's
/// `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`.
pub const RESERVED_ACTIONS: [&str; 18] = [
    "app.interrupt",
    "app.clear",
    "app.exit",
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
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// pi's `getShortcuts`: the shortcuts of `extensions` in load order, given
/// the keys bound to each built-in action. A key a reserved action uses is
/// refused; another built-in key, or one an earlier extension took, goes to
/// the later extension. Returns the bindings and pi's warnings, each with the
/// path of the extension it concerns.
pub fn resolve_shortcuts(
    extensions: &[Arc<dyn Extension>],
    builtin: &[(String, Vec<String>)],
) -> (Vec<ShortcutBinding>, Vec<(String, String)>) {
    let mut taken: std::collections::HashMap<String, (&str, bool)> =
        std::collections::HashMap::new();
    for (action, keys) in builtin {
        let reserved = RESERVED_ACTIONS.contains(&action.as_str());
        for key in keys {
            let key = key.to_lowercase();
            if taken
                .get(&key)
                .is_some_and(|(_, existing)| *existing && !reserved)
            {
                continue;
            }
            taken.insert(key, (action.as_str(), reserved));
        }
    }
    let mut bindings: Vec<ShortcutBinding> = Vec::new();
    let mut warnings = Vec::new();
    for extension in extensions {
        let path = extension.source().path;
        for shortcut in extension.shortcuts() {
            let key = shortcut.key.to_lowercase();
            match taken.get(&key) {
                Some((_, true)) => {
                    warnings.push((
                        path.clone(),
                        format!(
                            "Extension shortcut '{}' from {path} conflicts with built-in shortcut. Skipping.",
                            shortcut.key
                        ),
                    ));
                    continue;
                }
                Some((action, false)) => warnings.push((
                    path.clone(),
                    format!(
                        "Extension shortcut conflict: '{}' is built-in shortcut for {action} and {path}. Using {path}.",
                        shortcut.key
                    ),
                )),
                None => {}
            }
            let binding = ShortcutBinding {
                key: key.clone(),
                registered: shortcut.key.clone(),
                description: shortcut.description,
                path: path.clone(),
                extension: extension.clone(),
            };
            // Like a `Map`, the later binding keeps the earlier one's place.
            match bindings.iter_mut().find(|existing| existing.key == key) {
                Some(existing) => {
                    warnings.push((
                        path.clone(),
                        format!(
                            "Extension shortcut conflict: '{}' registered by both {} and {path}. Using {path}.",
                            shortcut.key, existing.path
                        ),
                    ));
                    *existing = binding;
                }
                None => bindings.push(binding),
            }
        }
    }
    (bindings, warnings)
}

/// How an extension draws one of its tools; pi's `renderCall`,
/// `renderResult` and `renderShell`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolRenderers {
    /// It draws the call.
    pub call: bool,
    /// It draws the result.
    pub result: bool,
    /// It draws its own frame (`renderShell: "self"`).
    pub own_shell: bool,
}

/// The tools a loadout hook sees; pi's `ToolLoadout`.
#[derive(Clone, Default)]
pub struct Loadout {
    /// The active tools in activation order, with their own descriptions.
    pub declared: Vec<RegisteredTool>,
    /// The tools other tools may call, in registration order.
    pub callable: Vec<RegisteredTool>,
}

/// What an extension draws in the transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Renderers {
    /// Its tools, by name.
    pub tools: std::collections::HashMap<String, ToolRenderers>,
    /// Custom message types it draws (`registerMessageRenderer`).
    pub messages: Vec<String>,
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

    /// Completions for the argument of `command`. While they are
    /// [`ArgumentCompletions::Pending`], the UI's `refresh_completions`
    /// follows once they are ready.
    fn complete(&self, _command: &str, _prefix: &str) -> ArgumentCompletions {
        ArgumentCompletions::Ready(None)
    }

    /// The keys it binds.
    fn shortcuts(&self) -> Vec<Shortcut> {
        Vec::new()
    }

    /// Runs the handler of shortcut `key`; the error is the handler's.
    fn run_shortcut<'a>(
        &'a self,
        _key: &'a str,
        _ctx: &'a Context,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
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

    /// Resolves once the extension code that is running has finished and
    /// delivered its results. pi runs extension code to completion before it
    /// reads the next input.
    fn settle(&self) -> BoxFuture<'_, ()> {
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

    /// Whether it handles pi events of type `kind` through [`Extension::handle`].
    fn handles(&self, _kind: &str) -> bool {
        false
    }

    /// Runs its handlers for pi event `event`, a JSON object with a `type`,
    /// and returns their combined result as pi's runner combines one
    /// extension's handlers. The session combines results across extensions.
    fn handle<'a>(&'a self, _ctx: &'a Context, _event: &'a Value) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async { None })
    }

    /// Descriptions for declared tools, by name, replacing their own in the
    /// next request; pi's `prepareLoadout` hook of a tool it registered. Runs
    /// before every request.
    fn prepare_loadout(&self, _loadout: &Loadout) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    /// What it draws in the transcript.
    fn renderers(&self) -> Renderers {
        Renderers::default()
    }

    /// Builds the component for a transcript item it draws. `request` is
    /// `{"kind": "toolCall" | "toolResult", "name", "toolCallId", "args",
    /// "result", "options", "context"}` or `{"kind": "message", "key",
    /// "message", "options"}`. `None` keeps the built-in rendering.
    fn component<'a>(&'a self, _request: &'a Value) -> BoxFuture<'a, Option<RemoteComponent>> {
        Box::pin(async { None })
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
