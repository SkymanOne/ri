//! pi extensions loaded into one `ri-js` instance, as ri-core extensions.
//!
//! The instance loads every extension once. Each session gets one
//! [`Extension`] per loaded file; when a later session starts, the guest runs
//! the extensions' factories again, as pi does for every session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use futures_util::future::BoxFuture;
use ri_agent::tool::{ExecutionMode, Tool, UpdateSink};
use ri_core::agent_session::{AgentSession, WeakSession};
use ri_core::extensions::{
    Command, Completion, ComponentHost, Context, CustomOptions, DialogOptions, Extension, Mode,
    NotifyKind, Placement, RemoteComponent, Renderers, ToolRenderers, Tools, Widget,
    WorkingIndicator,
};
use ri_core::tools::{Exposure, Namespace, RegisteredTool};
use ri_types::event::ToolResult;
use ri_types::message::{
    Content, ContentBlock, CustomMessage, ImageContent, ThinkingLevel, ToolDeclaration,
};
use ri_types::rpc::{SourceInfo, StreamingBehavior};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Bridge, Engine, Error, Instance, Options};

/// An extension file that failed to load, with pi's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    /// The file.
    pub path: PathBuf,
    /// Why.
    pub error: String,
}

/// A command-line flag an extension registered.
#[derive(Clone, Debug, PartialEq)]
pub struct Flag {
    /// Name without dashes.
    pub name: String,
    /// Whether it takes a value.
    pub takes_value: bool,
    /// What it does.
    pub description: Option<String>,
    /// The extension that registered it.
    pub extension_path: String,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Extensions loaded into one runtime instance, shared by the sessions of a
/// process: pi extensions in the JS runtime, or one native extension.
pub struct ExtensionHost {
    instance: Instance,
    bridge: Arc<SessionBridge>,
    /// Where each extension, by id, comes from.
    sources: Vec<SourceInfo>,
    /// Descriptions of the loaded extensions, as the guest last reported them.
    loaded: Mutex<Vec<Value>>,
    errors: Vec<LoadError>,
    /// Sessions handed extensions so far.
    sessions: AtomicU64,
    /// One more than the session generation the guest is bound to; 0 before
    /// the first binding.
    bound: tokio::sync::Mutex<u64>,
}

impl std::fmt::Debug for ExtensionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionHost")
            .field("errors", &self.errors)
            .finish_non_exhaustive()
    }
}

impl ExtensionHost {
    /// Loads pi extension files (TypeScript or JavaScript) into a new
    /// instance of the JS runtime; each source's `path` names the file, and
    /// the rest says where it comes from.
    pub async fn load(
        engine: &Engine,
        options: Options,
        sources: &[SourceInfo],
    ) -> Result<Arc<ExtensionHost>, Error> {
        let bridge = SessionBridge::new(engine);
        let instance = Instance::start(engine, options.clone(), bridge.clone()).await?;
        ExtensionHost::start(instance, bridge, options, sources).await
    }

    /// Loads a native extension, a WebAssembly component built with the Rust
    /// SDK, into its own instance.
    pub async fn load_native(
        engine: &Engine,
        options: Options,
        source: &SourceInfo,
    ) -> Result<Arc<ExtensionHost>, Error> {
        let component = engine.native(std::path::Path::new(&source.path))?;
        let bridge = SessionBridge::new(engine);
        let instance =
            Instance::start_component(engine, component, options.clone(), bridge.clone()).await?;
        ExtensionHost::start(instance, bridge, options, std::slice::from_ref(source)).await
    }

    async fn start(
        instance: Instance,
        bridge: Arc<SessionBridge>,
        options: Options,
        sources: &[SourceInfo],
    ) -> Result<Arc<ExtensionHost>, Error> {
        let entries: Vec<Value> = sources
            .iter()
            .enumerate()
            .map(|(id, source)| json!({"id": id, "path": source.path}))
            .collect();
        let result = instance
            .call("load", &json!({"cwd": options.cwd, "extensions": entries}))
            .await?;
        let (loaded, errors) = split(&result);
        let extensions = Arc::new(ExtensionHost {
            instance,
            bridge,
            sources: sources.to_vec(),
            loaded: Mutex::new(loaded),
            errors,
            sessions: AtomicU64::new(0),
            bound: tokio::sync::Mutex::new(0),
        });
        let _ = extensions.bridge.owner.set(Arc::downgrade(&extensions));
        Ok(extensions)
    }

    /// The files that failed to load.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// What each loaded extension registered, as the runtime describes it:
    /// `tools`, `commands`, `flags`, `shortcuts`, `events` and more.
    pub fn registrations(&self) -> Vec<Value> {
        lock(&self.loaded).clone()
    }

    /// The providers the loaded extensions registered with
    /// `pi.registerProvider(name, config)`, as names and configurations in
    /// `models.json`'s shape. Providers with their own `streamSimple` are
    /// left out: ri cannot stream through them.
    pub fn providers(&self) -> Vec<(String, Value)> {
        lock(&self.loaded)
            .iter()
            .flat_map(|extension| list(&extension["providers"]))
            .filter_map(|provider| {
                let mut config = provider["config"].as_object()?.clone();
                if config.remove("hasStreamSimple") == Some(Value::Bool(true)) {
                    return None;
                }
                // Sign-in and image or classifier implementations are code.
                for key in ["oauth", "images", "classifiers"] {
                    config.remove(key);
                }
                Some((text(&provider["name"]), Value::Object(config)))
            })
            .collect()
    }

    /// The flags the loaded extensions registered.
    pub fn flags(&self) -> Vec<Flag> {
        lock(&self.loaded)
            .iter()
            .flat_map(|extension| {
                let path = text(&extension["path"]);
                list(&extension["flags"])
                    .into_iter()
                    .map(move |flag| (path.clone(), flag))
            })
            .map(|(extension_path, flag)| Flag {
                name: text(&flag["name"]),
                takes_value: flag["type"] == "string",
                description: flag["description"].as_str().map(str::to_owned),
                extension_path,
            })
            .collect()
    }

    /// Sets flag values from the command line: `true` for boolean flags,
    /// strings for the others.
    pub async fn set_flags(&self, values: serde_json::Map<String, Value>) -> Result<(), Error> {
        self.instance
            .call("flags", &json!({"values": values}))
            .await
            .map(|_| ())
    }

    /// The extensions for a new session, one per loaded file.
    pub fn for_session(self: &Arc<Self>) -> Vec<Arc<dyn Extension>> {
        let generation = self.sessions.fetch_add(1, Ordering::SeqCst);
        lock(&self.loaded)
            .iter()
            .map(|description| {
                Arc::new(JsExtension {
                    shared: self.clone(),
                    generation,
                    id: description["id"].as_u64().unwrap_or_default(),
                    path: PathBuf::from(text(&description["path"])),
                    description: description.clone(),
                    completions: Arc::default(),
                }) as Arc<dyn Extension>
            })
            .collect()
    }

    /// Binds the guest to session `generation`, running the factories again
    /// for every session after the first. `false` when a later session is
    /// already bound, so `generation`'s extensions are stale.
    async fn bind(&self, generation: u64, ctx: &Context) -> bool {
        let mut bound = self.bound.lock().await;
        if *bound > generation + 1 {
            return false;
        }
        if *bound == generation + 1 {
            return true;
        }
        *lock(&self.bridge.session) = ctx.session.clone();
        if generation > 0 {
            match self.instance.call("reload", &Value::Null).await {
                Ok(result) => *lock(&self.loaded) = split(&result).0,
                Err(err) => {
                    ctx.ui
                        .extension_error("ri-js", "session_start", &err.to_string(), None)
                }
            }
        }
        if let Err(err) = self.instance.call("bind", &Value::Null).await {
            ctx.ui
                .extension_error("ri-js", "session_start", &err.to_string(), None);
        }
        *bound = generation + 1;
        true
    }
}

fn split(result: &Value) -> (Vec<Value>, Vec<LoadError>) {
    let mut loaded = Vec::new();
    let mut errors = Vec::new();
    for extension in list(&result["extensions"]) {
        match extension["error"].as_str() {
            Some(error) => errors.push(LoadError {
                path: PathBuf::from(text(&extension["path"])),
                error: error.to_owned(),
            }),
            None => loaded.push(extension),
        }
    }
    (loaded, errors)
}

fn list(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Tui => "tui",
        Mode::Rpc => "rpc",
        Mode::Print => "print",
        Mode::Json => "json",
    }
}

/// What the guest's `ctx` objects are built from.
fn ctx_data(
    session: Option<&AgentSession>,
    ui: &dyn ri_core::extensions::ExtensionUi,
    mode: Mode,
    trusted: bool,
    cancel: &CancellationToken,
) -> Value {
    json!({
        "hasUI": ui.has_ui(),
        "components": ui.shows_components(),
        "mode": mode_name(mode),
        "cwd": session.map(|session| session.cwd().to_path_buf()),
        "model": session.and_then(AgentSession::model),
        "thinkingLevel": session.map(|session| session.thinking_level().as_str()),
        "projectTrusted": trusted,
        "aborted": cancel.is_cancelled(),
    })
}

fn context_data(ctx: &Context) -> Value {
    ctx_data(
        ctx.session.upgrade().as_ref(),
        ctx.ui.as_ref(),
        ctx.mode,
        ctx.project_trusted,
        &ctx.cancel,
    )
}

/// One loaded extension file, for one session.
struct JsExtension {
    shared: Arc<ExtensionHost>,
    generation: u64,
    id: u64,
    path: PathBuf,
    description: Value,
    /// Argument completions by command and prefix, as fetched.
    completions: Arc<Mutex<HashMap<(String, String), Fetched>>>,
}

/// An argument completion request: under way, or answered at a time.
enum Fetched {
    Pending,
    Ready(std::time::Instant, Option<Vec<Completion>>),
}

/// How long fetched completions are reused before being asked for again.
const COMPLETIONS_TTL: std::time::Duration = std::time::Duration::from_secs(2);

impl JsExtension {
    async fn call(
        &self,
        ctx: &Context,
        kind: &str,
        payload: Value,
    ) -> Option<Result<Value, Error>> {
        if !self.shared.bind(self.generation, ctx).await {
            return None;
        }
        Some(self.shared.instance.call(kind, &payload).await)
    }

    fn path_text(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Extension for JsExtension {
    fn source(&self) -> SourceInfo {
        usize::try_from(self.id)
            .ok()
            .and_then(|id| self.shared.sources.get(id))
            .cloned()
            .unwrap_or_else(|| SourceInfo {
                path: self.path_text(),
                source: "local".into(),
                scope: "temporary".into(),
                origin: "top-level".into(),
                base_dir: None,
            })
    }

    fn load(&self, tools: &Tools) {
        for tool in list(&self.description["tools"]) {
            tools.register(js_tool(&self.shared, self.id, &tool));
        }
    }

    /// pi awaits `getArgumentCompletions`; the editor asks synchronously, so
    /// a request the guest has not answered yet starts it in the background,
    /// offers nothing, and has the editor ask again once it is answered.
    fn complete(&self, command: &str, prefix: &str) -> Option<Vec<Completion>> {
        let completes = list(&self.description["commands"])
            .iter()
            .any(|entry| entry["name"] == command && entry["hasCompletions"] == true);
        if !completes {
            return None;
        }
        let key = (command.to_owned(), prefix.to_owned());
        {
            let mut cache = lock(&self.completions);
            cache.retain(|_, fetched| match fetched {
                Fetched::Pending => true,
                Fetched::Ready(at, _) => at.elapsed() < COMPLETIONS_TTL,
            });
            match cache.get(&key) {
                Some(Fetched::Ready(_, items)) => return items.clone(),
                Some(Fetched::Pending) => return None,
                None => {
                    cache.insert(key.clone(), Fetched::Pending);
                }
            }
        }
        let (host, cache) = (Arc::clone(&self.shared), Arc::clone(&self.completions));
        let payload = json!({"extension": self.id, "name": command, "prefix": prefix});
        self.shared.bridge.runtime.spawn(async move {
            let items = host
                .instance
                .call("complete", &payload)
                .await
                .ok()
                .and_then(|value| {
                    value.as_array().map(|items| {
                        items
                            .iter()
                            .map(|item| Completion {
                                value: text(&item["value"]),
                                label: item["label"]
                                    .as_str()
                                    .map_or_else(|| text(&item["value"]), str::to_owned),
                                description: item["description"].as_str().map(str::to_owned),
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .filter(|items| !items.is_empty());
            let found = items.is_some();
            lock(&cache).insert(key, Fetched::Ready(std::time::Instant::now(), items));
            if found && let Some(session) = host.bridge.session() {
                session.extension_binding().0.refresh_completions();
            }
        });
        None
    }

    fn commands(&self) -> Vec<Command> {
        list(&self.description["commands"])
            .iter()
            .map(|command| Command {
                name: text(&command["name"]),
                description: text(&command["description"]),
            })
            .collect()
    }

    fn shortcuts(&self) -> Vec<ri_core::extensions::Shortcut> {
        list(&self.description["shortcuts"])
            .iter()
            .map(|shortcut| ri_core::extensions::Shortcut {
                key: text(&shortcut["shortcut"]),
                description: shortcut["description"].as_str().map(str::to_owned),
            })
            .collect()
    }

    fn run_shortcut<'a>(
        &'a self,
        key: &'a str,
        ctx: &'a Context,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let payload = json!({"extension": self.id, "shortcut": key, "ctx": context_data(ctx)});
            match self.call(ctx, "shortcut", payload).await {
                Some(Err(err)) => Err(err.to_string()),
                _ => Ok(()),
            }
        })
    }

    fn run_command<'a>(
        &'a self,
        command: &'a str,
        args: &'a str,
        ctx: &'a Context,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let payload = json!({"extension": self.id, "name": command, "args": args, "ctx": context_data(ctx)});
            // pi names the command in place of the extension.
            if let Some(Err(err)) = self.call(ctx, "command", payload).await {
                ctx.ui.extension_error(
                    &format!("command:{command}"),
                    "command",
                    &err.to_string(),
                    None,
                );
            }
        })
    }

    fn session_start<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.shared.bind(self.generation, ctx).await;
        })
    }

    fn renderers(&self) -> Renderers {
        Renderers {
            tools: list(&self.description["tools"])
                .iter()
                .filter(|tool| tool["hasRenderCall"] == true || tool["hasRenderResult"] == true)
                .map(|tool| {
                    let renderers = ToolRenderers {
                        call: tool["hasRenderCall"] == true,
                        result: tool["hasRenderResult"] == true,
                        own_shell: tool["renderShell"] == "self",
                    };
                    (text(&tool["name"]), renderers)
                })
                .collect(),
            messages: list(&self.description["messageRenderers"])
                .iter()
                .map(text)
                .collect(),
        }
    }

    fn component<'a>(&'a self, request: &'a Value) -> BoxFuture<'a, Option<RemoteComponent>> {
        Box::pin(async move {
            // Only the bound session's extensions draw.
            if *self.shared.bound.lock().await != self.generation + 1 {
                return None;
            }
            let mut payload = request.clone();
            payload["extension"] = json!(self.id);
            let result = self
                .shared
                .instance
                .call("component", &payload)
                .await
                .ok()?;
            self.shared.bridge.component(&result["handle"])
        })
    }

    fn handles(&self, kind: &str) -> bool {
        self.description["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| event == kind))
    }

    fn handle<'a>(&'a self, ctx: &'a Context, event: &'a Value) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let kind = event["type"].as_str().unwrap_or_default();
            let payload = json!({"extension": self.id, "event": event, "ctx": context_data(ctx)});
            match self.call(ctx, "emit", payload).await? {
                Ok(outcome) => {
                    for error in list(&outcome["errors"]) {
                        ctx.ui.extension_error(
                            &self.path_text(),
                            kind,
                            &text(&error["error"]),
                            error["stack"].as_str(),
                        );
                    }
                    Some(outcome["result"].clone()).filter(|result| !result.is_null())
                }
                // A failing `tool_call` handler blocks the call, as in pi.
                Err(err) if kind == "tool_call" => {
                    Some(json!({"block": true, "reason": err.to_string()}))
                }
                Err(err) => {
                    ctx.ui
                        .extension_error(&self.path_text(), kind, &err.to_string(), None);
                    None
                }
            }
        })
    }
}

fn js_tool(shared: &Arc<ExtensionHost>, extension: u64, tool: &Value) -> RegisteredTool {
    let exposure = serde_json::from_value(tool["exposure"].clone()).unwrap_or(Exposure::Direct);
    let declaration = ToolDeclaration {
        name: text(&tool["name"]),
        description: text(&tool["description"]),
        parameters: tool["parameters"].clone(),
        constrained_sampling: None,
    };
    let label = tool["label"]
        .as_str()
        .map_or_else(|| declaration.name.clone(), str::to_owned);
    let namespace = tool["namespace"]["name"].as_str().map(|name| Namespace {
        name: name.to_owned(),
        description: tool["namespace"]["description"].as_str().map(str::to_owned),
        instructions: tool["namespace"]["instructions"]
            .as_str()
            .map(str::to_owned),
    });
    RegisteredTool {
        tool: Arc::new(JsTool {
            shared: Arc::downgrade(shared),
            extension,
            label,
            declaration,
            output_schema: Some(tool["outputSchema"].clone()).filter(|schema| !schema.is_null()),
            sequential: tool["executionMode"] == "sequential",
        }),
        snippet: tool["promptSnippet"].as_str().map(str::to_owned),
        guidelines: list(&tool["promptGuidelines"])
            .iter()
            .filter_map(|line| line.as_str().map(str::to_owned))
            .collect(),
        exposure,
        namespace,
        default_active: tool["defaultActive"] != false,
    }
}

/// A tool an extension registered; runs in the guest.
struct JsTool {
    shared: Weak<ExtensionHost>,
    extension: u64,
    label: String,
    declaration: ToolDeclaration,
    output_schema: Option<Value>,
    sequential: bool,
}

impl Tool for JsTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn execution_mode(&self) -> ExecutionMode {
        if self.sequential {
            ExecutionMode::Sequential
        } else {
            ExecutionMode::Parallel
        }
    }

    fn execute(
        &self,
        call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let shared = self
                .shared
                .upgrade()
                .ok_or("The extension runtime has stopped")?;
            let bridge = &shared.bridge;
            lock(&bridge.updates).insert(call_id.clone(), (updates, cancel.clone()));
            let session = bridge.session();
            let (ui, mode) = session
                .as_ref()
                .map(AgentSession::extension_binding)
                .unwrap_or_else(|| (Arc::new(ri_core::extensions::NoUi), Mode::Print));
            let ctx = ctx_data(session.as_ref(), ui.as_ref(), mode, false, &cancel);
            let tools: Vec<Value> = session
                .as_ref()
                .map(AgentSession::callable_tools)
                .unwrap_or_default()
                .iter()
                .map(|tool| {
                    let declaration = tool.tool.declaration();
                    json!({"name": declaration.name, "label": tool.tool.label(), "description": declaration.description, "parameters": declaration.parameters})
                })
                .collect();
            let payload = json!({
                "extension": self.extension, "name": self.declaration.name,
                "toolCallId": call_id, "params": args, "ctx": ctx, "tools": tools,
            });
            let result = shared.instance.call("tool", &payload).await;
            lock(&bridge.updates).remove(&call_id);
            tool_result(result.map_err(|err| err.to_string())?)
        })
    }
}

/// An extension tool's result as ri's.
fn tool_result(mut value: Value) -> Result<ToolResult, String> {
    if value.get("content").is_none_or(Value::is_null) {
        value["content"] = json!([]);
    }
    serde_json::from_value(value).map_err(|err| format!("Invalid tool result: {err}"))
}

/// Answers the guest's requests from the bound session.
/// Numbers the runtimes of a process, so their component handles stay apart.
static RUNTIMES: AtomicU64 = AtomicU64::new(1);

struct SessionBridge {
    /// This runtime's number.
    runtime_id: u64,
    /// Where actions that outlive a request run.
    runtime: tokio::runtime::Handle,
    session: Mutex<WeakSession>,
    /// Update sinks and cancellation of running extension tools, by call id.
    updates: Mutex<HashMap<String, (UpdateSink, CancellationToken)>>,
    owner: OnceLock<Weak<ExtensionHost>>,
    /// Runs scripts of the codemode tool the facade's `createCodemodeExtension` registers.
    codemode: Arc<crate::codemode::Runner>,
}

fn not_bound() -> String {
    "Extension runtime not initialized. Action methods cannot be called during extension loading."
        .into()
}

impl SessionBridge {
    fn new(engine: &Engine) -> Arc<SessionBridge> {
        Arc::new(SessionBridge {
            codemode: Arc::new(crate::codemode::Runner::with_engine(
                engine.clone(),
                crate::codemode::docs(),
            )),
            runtime_id: RUNTIMES.fetch_add(1, Ordering::Relaxed),
            runtime: tokio::runtime::Handle::current(),
            session: Mutex::default(),
            updates: Mutex::default(),
            owner: OnceLock::new(),
        })
    }

    fn session(&self) -> Option<AgentSession> {
        lock(&self.session).upgrade()
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(future);
    }
}

/// A dialog's `timeout` (milliseconds) from its request.
fn dialog(payload: &Value) -> DialogOptions {
    DialogOptions {
        timeout: payload["timeout"]
            .as_f64()
            .filter(|millis| *millis > 0.0)
            .map(|millis| std::time::Duration::from_millis(millis as u64)),
        cancel: None,
    }
}

/// Renders the components of the instance that owns a bridge.
struct Components(Weak<ExtensionHost>);

impl ComponentHost for Components {
    fn render(&self, handle: u32, width: u16) -> BoxFuture<'static, Vec<String>> {
        let host = self.0.upgrade();
        Box::pin(async move {
            match host {
                Some(host) => host.instance.render(handle, u32::from(width)).await,
                None => Vec::new(),
            }
        })
    }

    fn input(&self, handle: u32, data: &str) {
        if let Some(host) = self.0.upgrade() {
            host.instance.input(handle, data);
        }
    }
}

impl SessionBridge {
    fn component(&self, handle: &Value) -> Option<RemoteComponent> {
        let handle = u32::try_from(handle.as_u64()?).ok()?;
        let owner = self.owner.get()?.clone();
        Some(RemoteComponent::new(
            self.runtime_id,
            handle,
            Arc::new(Components(owner)),
        ))
    }

    /// pi's `ctx.ui` methods that answer at once.
    fn ui_request(&self, session: &AgentSession, kind: &str, payload: &Value) -> Value {
        let (ui, _) = session.extension_binding();
        let optional = |key: &str| payload[key].as_str();
        match kind {
            "ui.notify" => {
                let kind = match payload["type"].as_str() {
                    Some("warning") => NotifyKind::Warning,
                    Some("error") => NotifyKind::Error,
                    Some(_) => NotifyKind::Info,
                    None => {
                        ui.notify_untyped(&text(&payload["message"]));
                        return Value::Null;
                    }
                };
                ui.notify(&text(&payload["message"]), kind);
            }
            "ui.setStatus" => ui.set_status(&text(&payload["key"]), optional("text")),
            "ui.setWidget" => {
                let widget = match &payload["lines"] {
                    Value::Array(lines) => Some(Widget::Lines(lines.iter().map(text).collect())),
                    _ => self.component(&payload["handle"]).map(Widget::Component),
                };
                let placement = match payload["options"]["placement"].as_str() {
                    Some("belowEditor") => Some(Placement::BelowEditor),
                    Some("aboveEditor") => Some(Placement::AboveEditor),
                    _ => None,
                };
                ui.set_widget(&text(&payload["key"]), widget, placement);
            }
            "ui.setFooter" => ui.set_footer(self.component(&payload["handle"])),
            "ui.setHeader" => ui.set_header(self.component(&payload["handle"])),
            "ui.setTitle" => ui.set_title(&text(&payload["title"])),
            "ui.setWorkingMessage" => ui.set_working_message(optional("message")),
            "ui.setWorkingVisible" => ui.set_working_visible(payload["visible"] != false),
            "ui.setWorkingIndicator" => {
                let options = &payload["options"];
                ui.set_working_indicator(options.is_object().then(|| {
                    WorkingIndicator {
                        frames: options["frames"]
                            .as_array()
                            .map(|frames| frames.iter().map(text).collect()),
                        interval_ms: options["intervalMs"]
                            .as_f64()
                            .filter(|ms| *ms > 0.0)
                            .map(|ms| ms.ceil() as u64),
                    }
                }));
            }
            "ui.setHiddenThinkingLabel" => ui.set_hidden_thinking_label(optional("label")),
            "ui.setEditorText" => ui.set_editor_text(&text(&payload["text"])),
            "ui.pasteToEditor" => ui.paste_to_editor(&text(&payload["text"])),
            "ui.getEditorText" => return Value::String(ui.editor_text()),
            "ui.custom" => {
                if let Some(component) = self.component(&payload["handle"]) {
                    let options = CustomOptions {
                        overlay: payload["overlay"] == true,
                        overlay_options: payload["overlayOptions"].clone(),
                    };
                    ui.custom(component, options);
                }
            }
            "ui.close" => {
                if let Some(component) = self.component(&payload["handle"]) {
                    ui.close(component);
                }
            }
            "ui.requestRender" => ui.request_render(),
            "ui.getToolsExpanded" => return Value::Bool(ui.tools_expanded()),
            "ui.setToolsExpanded" => ui.set_tools_expanded(payload["expanded"] == true),
            "ui.theme" => return ui.theme(),
            "ui.footerData" => return ui.footer_data(),
            // The working indicator's visibility and frames are not shown.
            _ => {}
        }
        Value::Null
    }
}

fn custom_message(message: &Value) -> CustomMessage {
    let content = match &message["content"] {
        Value::Null => json!([]),
        other => other.clone(),
    };
    CustomMessage {
        custom_type: text(&message["customType"]),
        content: serde_json::from_value(content).unwrap_or(Content::Blocks(Vec::new())),
        display: message["display"].as_bool().unwrap_or(false),
        details: Some(message["details"].clone()).filter(|details| !details.is_null()),
        timestamp: ri_core::time::now_ms(),
    }
}

/// `pi.sendUserMessage` content as prompt text and images.
fn user_content(content: &Value) -> (String, Vec<ImageContent>) {
    if let Some(text) = content.as_str() {
        return (text.to_owned(), Vec::new());
    }
    let blocks: Vec<ContentBlock> = serde_json::from_value(content.clone()).unwrap_or_default();
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => texts.push(text.text),
            ContentBlock::Image(image) => images.push(image),
            _ => {}
        }
    }
    (texts.join("\n"), images)
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn session_read(session: &AgentSession, method: &str, args: &Value) -> Result<Value, String> {
    let arg = |index: usize| args[index].as_str().map(str::to_owned);
    session.with_session(|file| {
        Ok(match method {
            "getCwd" => to_json(file.cwd()),
            "getSessionDir" => to_json(file.dir()),
            "getSessionId" => to_json(file.id()),
            "getSessionFile" => to_json(file.file()),
            "getLeafId" => to_json(file.leaf_id()),
            "getLeafEntry" => to_json(file.leaf_id().and_then(|id| file.entry(id))),
            "getEntry" => to_json(arg(0).and_then(|id| file.entry(&id).cloned())),
            "getLabel" => to_json(arg(0).and_then(|id| file.label(&id).map(str::to_owned))),
            "getBranch" => to_json(file.branch_path(arg(0).as_deref())),
            "getHeader" => to_json(file.header()),
            "getEntries" => to_json(file.entries().collect::<Vec<_>>()),
            "getSessionName" => to_json(file.name()),
            "isPersisted" => Value::Bool(file.is_persisted()),
            "getChildren" => {
                let parent = arg(0);
                to_json(
                    file.entries()
                        .filter(|entry| to_json(entry)["parentId"].as_str() == parent.as_deref())
                        .collect::<Vec<_>>(),
                )
            }
            "buildSessionContext" | "getContext" => {
                let context = file.build_context();
                json!({
                    "messages": context.messages,
                    "thinkingLevel": context.thinking_level,
                    "model": context.model.map(|(provider, id)| json!({"provider": provider, "modelId": id})),
                })
            }
            other => return Err(format!("sessionManager.{other}() is not available in ri extensions")),
        })
    })
}

impl Bridge for SessionBridge {
    fn request(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        if kind == "tool.update" {
            let sink = lock(&self.updates)
                .get(payload["toolCallId"].as_str().unwrap_or_default())
                .map(|(sink, _)| sink.clone());
            if let (Some(sink), Ok(partial)) = (sink, tool_result(payload["partial"].clone())) {
                sink(partial);
            }
            return Ok(Value::Null);
        }
        let session = self.session().ok_or_else(not_bound)?;
        match kind {
            "session.sendMessage" => {
                let message = custom_message(&payload["message"]);
                let options = &payload["options"];
                let trigger = options["triggerTurn"].as_bool();
                let deliver_as = options["deliverAs"].as_str();
                // pi records and reports the message before the call returns.
                if let Some(message) = session.deliver_custom_message(message, trigger, deliver_as)
                {
                    self.spawn(async move { session.run_triggered(message).await });
                }
                Ok(Value::Null)
            }
            "session.sendUserMessage" => {
                let (text, images) = user_content(&payload["content"]);
                let behavior = match payload["options"]["deliverAs"].as_str() {
                    Some("followUp") => Some(StreamingBehavior::FollowUp),
                    Some(_) => Some(StreamingBehavior::Steer),
                    None => None,
                };
                self.spawn(async move {
                    let source = ri_core::agent_session::InputSource::Extension;
                    if let Err(error) = session
                        .prompt_with(&text, images, behavior, source, |_| {})
                        .await
                    {
                        let (ui, _) = session.extension_binding();
                        ui.notify(&error, NotifyKind::Error);
                    }
                });
                Ok(Value::Null)
            }
            "session.appendEntry" => session
                .append_custom_entry(
                    &text(&payload["customType"]),
                    Some(payload["data"].clone()).filter(|data| !data.is_null()),
                )
                .map(|()| Value::Null),
            "session.setName" => {
                session.set_name(&text(&payload["name"]));
                Ok(Value::Null)
            }
            "session.getName" => Ok(to_json(session.with_session(|file| file.name()))),
            "session.setLabel" => session.with_session(|file| {
                file.append_label(&text(&payload["entryId"]), payload["label"].as_str().map(str::to_owned))
                    .map(|_| Value::Null)
                    .map_err(|err| err.to_string())
            }),
            "session.read" => session_read(&session, payload["method"].as_str().unwrap_or_default(), &payload["args"]),
            "tools.getActive" => Ok(to_json(session.active_tool_names())),
            "tools.getAll" => Ok(Value::Array(
                session
                    .tools()
                    .all()
                    .into_iter()
                    .map(|tool| json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters}))
                    .collect(),
            )),
            "tools.setActive" => {
                let names = list(&payload["names"]).iter().map(text).collect();
                session.set_active_tools(names);
                Ok(Value::Null)
            }
            "tools.refresh" => {
                let owner = self.owner.get().and_then(Weak::upgrade).ok_or_else(not_bound)?;
                let extension = payload["extension"].as_u64().unwrap_or_default();
                for tool in list(&payload["tools"]) {
                    session.tools().register(js_tool(&owner, extension, &tool));
                }
                Ok(Value::Null)
            }
            "commands.list" => Ok(Value::Array(
                session
                    .extension_commands()
                    .into_iter()
                    .map(|resolved| {
                        json!({"name": resolved.invocation, "description": resolved.command.description, "source": "extension", "sourceInfo": resolved.extension.source()})
                    })
                    .collect(),
            )),
            "thinking.get" => Ok(Value::String(session.thinking_level().as_str().into())),
            "thinking.set" => {
                if let Some(level) = payload["level"].as_str().and_then(ThinkingLevel::parse) {
                    session.set_thinking_level(level);
                }
                Ok(Value::Null)
            }
            "settings.get" => Ok(to_json(session.settings())),
            "models.find" => Ok(to_json(
                session
                    .registry()
                    .find(&text(&payload["provider"]), &text(&payload["id"]))
                    .cloned(),
            )),
            "models.all" => Ok(to_json(session.registry().models())),
            "models.available" => Ok(to_json(session.available_models())),
            "models.hasAuth" => Ok(Value::Bool(session.registry().has_auth(&text(&payload["provider"])))),
            "models.usingOAuth" => Ok(Value::Bool(
                session
                    .registry()
                    .is_using_oauth(&text(&payload["provider"])),
            )),
            "agent.isIdle" => Ok(Value::Bool(!session.is_streaming())),
            "agent.abort" => {
                session.abort();
                Ok(Value::Null)
            }
            "agent.hasPendingMessages" => Ok(Value::Bool(session.pending_message_count() > 0)),
            "agent.contextUsage" => Ok(session.context_usage().map_or(Value::Null, |usage| {
                json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent()})
            })),
            "agent.systemPrompt" => Ok(Value::String(
                ri_ai::transcript::current_system_message(&session.messages())
                    .map(|system| system.text())
                    .unwrap_or_default(),
            )),
            "agent.shutdown" => {
                session.extension_binding().0.shutdown();
                Ok(Value::Null)
            }
            _ if kind.starts_with("ui.") => Ok(self.ui_request(&session, kind, payload)),
            "util.convertToLlm" => {
                let messages = serde_json::from_value(payload["messages"].clone()).map_err(|err| err.to_string())?;
                Ok(to_json(ri_core::messages::convert_to_llm(messages)))
            }
            "util.serializeConversation" => {
                let messages: Vec<ri_types::message::Message> =
                    serde_json::from_value(payload["messages"].clone()).map_err(|err| err.to_string())?;
                Ok(Value::String(ri_core::compaction::serialize_conversation(&messages)))
            }
            _ => Err(format!("{kind} is not available in ri extensions yet")),
        }
    }

    fn start(&self, kind: &str, payload: Value) -> BoxFuture<'static, Result<Value, String>> {
        let Some(session) = self.session() else {
            return Box::pin(async { Err(not_bound()) });
        };
        // Nested calls and scripts take the calling tool's cancellation.
        let (caller_updates, caller_cancel) = lock(&self.updates)
            .get(payload["toolCallId"].as_str().unwrap_or_default())
            .cloned()
            .unwrap_or_else(|| (Arc::new(|_| {}), CancellationToken::new()));
        let codemode = self.codemode.clone();
        let kind = kind.to_owned();
        Box::pin(async move {
            match kind.as_str() {
                "codemode.execute" => codemode
                    .execute(
                        Some(session),
                        text(&payload["toolCallId"]),
                        payload["params"].clone(),
                        caller_cancel,
                        caller_updates,
                    )
                    .await
                    .map(to_json),
                "tool.execute" => {
                    let outcome = session
                        .execute_tool(
                            &text(&payload["toolCallId"]),
                            &text(&payload["name"]),
                            payload["args"].clone(),
                            caller_cancel,
                            None,
                        )
                        .await;
                    Ok(
                        json!({"toolCall": outcome.call, "result": outcome.result, "isError": outcome.is_error}),
                    )
                }
                "model.set" => {
                    let registry = session.registry();
                    let Some(model) = registry
                        .find(&text(&payload["provider"]), &text(&payload["id"]))
                        .cloned()
                    else {
                        return Ok(Value::Bool(false));
                    };
                    if !registry.has_auth(&model.provider) {
                        return Ok(Value::Bool(false));
                    }
                    Ok(Value::Bool(session.set_model(model).is_ok()))
                }
                "models.apiKey" => {
                    let registry = session.registry();
                    let provider = text(&payload["provider"]);
                    let model = match payload["id"].as_str() {
                        Some(id) => registry.find(&provider, id).cloned(),
                        None => registry
                            .models()
                            .iter()
                            .find(|model| model.provider == provider)
                            .cloned(),
                    };
                    match model {
                        Some(model) => Ok(to_json(registry.auth(&model).await.api_key)),
                        None => Ok(Value::Null),
                    }
                }
                "agent.waitForIdle" => {
                    session.wait_for_idle().await;
                    Ok(Value::Null)
                }
                "agent.compact" => session
                    .compact(payload["customInstructions"].as_str())
                    .await
                    .map(to_json),
                "ui.select" => {
                    let (ui, _) = session.extension_binding();
                    let options = list(&payload["options"]).iter().map(text).collect();
                    Ok(to_json(
                        ui.select(&text(&payload["title"]), options, dialog(&payload))
                            .await,
                    ))
                }
                "ui.confirm" => {
                    let (ui, _) = session.extension_binding();
                    Ok(Value::Bool(
                        ui.confirm(
                            &text(&payload["title"]),
                            &text(&payload["message"]),
                            dialog(&payload),
                        )
                        .await,
                    ))
                }
                "ui.input" => {
                    let (ui, _) = session.extension_binding();
                    let placeholder = payload["placeholder"].as_str().map(str::to_owned);
                    Ok(to_json(
                        ui.input(
                            &text(&payload["title"]),
                            placeholder.as_deref(),
                            dialog(&payload),
                        )
                        .await,
                    ))
                }
                "ui.editor" => {
                    let (ui, _) = session.extension_binding();
                    let prefill = payload["prefill"].as_str().map(str::to_owned);
                    Ok(to_json(
                        ui.editor(&text(&payload["title"]), prefill.as_deref())
                            .await,
                    ))
                }
                other => Err(format!("{other} is not available in ri extensions yet")),
            }
        })
    }
}
