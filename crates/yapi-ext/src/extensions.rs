//! pi extensions loaded into one `yapi-js` instance, as yapi-core extensions.
//!
//! The instance loads every extension once. Each session gets one
//! [`Extension`] per loaded file; when a later session starts, the guest runs
//! the extensions' factories again, as pi does for every session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::tool::{ExecutionMode, Tool, UpdateSink};
use yapi_ai::auth::{
    AuthError, AuthEvent, AuthPrompt, Interaction, LoginOptions, OAuthAuth, OAuthProvider,
};
use yapi_ai::model_catalog::RefreshOptions;
use yapi_ai::registry::ModelRegistry;
use yapi_ai::stream::{
    EventSender, EventStream, Provider, ProviderResponse, Request, RequestHooks, StreamEvent,
    new_output, now_ms, send_error,
};
use yapi_core::agent_session::{AgentSession, WeakSession};
use yapi_core::extensions::{
    Command, ComponentHost, Context, CustomOptions, DialogOptions, Extension, Mode, ModelList,
    NotifyKind, Placement, RemoteComponent, Renderers, SessionAction, ToolRenderers, Tools, Widget,
    WorkingIndicator,
};
use yapi_core::tools::{Exposure, Namespace, RegisteredTool};
use yapi_types::auth::{Credential, OAuthCredential};
use yapi_types::autocomplete::{ArgumentCompletions, AutocompleteItem};
use yapi_types::event::ToolResult;
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, CustomMessage, ImageContent, ThinkingLevel,
    ToolDeclaration,
};
use yapi_types::models::ModelDefinition;
use yapi_types::rpc::{SourceInfo, StreamingBehavior};
use yapi_types::sync::lock;

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

/// Extensions loaded into one runtime instance, shared by the sessions of a
/// process: pi extensions in the JS runtime, or one native extension.
pub struct ExtensionHost {
    instance: Instance,
    bridge: Arc<SessionBridge>,
    /// Where each extension, by id, comes from.
    sources: Vec<SourceInfo>,
    /// Descriptions of the loaded extensions, as the guest last reported them.
    loaded: Mutex<Vec<Value>>,
    /// Wire APIs registered with pi-ai's `registerApiProvider` while the
    /// extensions loaded.
    apis: Vec<String>,
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
            apis: list(&result["apis"]).iter().map(text).collect(),
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
    /// `pi.registerProvider(name, config)`, in registration order.
    pub fn providers(self: &Arc<Self>) -> Vec<RegisteredProvider> {
        lock(&self.loaded)
            .iter()
            .flat_map(|extension| list(&extension["providers"]))
            .filter_map(|provider| {
                let name = text(&provider["name"]);
                let mut config = provider["config"].as_object()?.clone();
                let streams = config.remove("hasStreamSimple") == Some(Value::Bool(true));
                config.remove("hasRefreshModels");
                let oauth = config
                    .remove("oauth")
                    .filter(Value::is_object)
                    .map(|oauth| {
                        Arc::new(JsOAuth {
                            host: Arc::downgrade(self),
                            provider: name.clone(),
                            name: text(&oauth["name"]),
                            subscription: oauth["isSubscription"] == true,
                        }) as Arc<dyn OAuthProvider>
                    });
                // Image and classifier implementations are code.
                for key in ["images", "classifiers"] {
                    config.remove(key);
                }
                let stream = config
                    .get("api")
                    .and_then(Value::as_str)
                    .filter(|_| streams)
                    .map(|api| self.js_stream(api));
                Some(RegisteredProvider {
                    name,
                    config: Value::Object(config),
                    stream,
                    oauth,
                })
            })
            .collect()
    }

    /// The wire APIs extensions implement with pi-ai's `registerApiProvider`
    /// that yapi has no provider for, to stream the session's models of
    /// those APIs.
    pub fn apis(self: &Arc<Self>) -> Vec<Arc<dyn Provider>> {
        self.apis
            .iter()
            .filter(|api| yapi_ai::api::builtin(api).is_none())
            .map(|api| self.js_stream(api))
            .collect()
    }

    fn js_stream(self: &Arc<Self>, api: &str) -> Arc<dyn Provider> {
        Arc::new(JsStream {
            host: Arc::downgrade(self),
            api: api.to_owned(),
        })
    }

    /// A new id for an operation the host may abort in the guest.
    fn next_id(&self) -> u64 {
        self.bridge.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Runs guest call `kind` for operation `payload.id`. When `cancel`
    /// fires first, aborts the operation's signal and waits for the call to
    /// end, as pi awaits an aborted operation.
    async fn call_abortable(
        &self,
        kind: &str,
        payload: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value, Error> {
        let mut call = std::pin::pin!(self.instance.call(kind, payload));
        match cancel.run_until_cancelled(&mut call).await {
            Some(result) => result,
            None => {
                let _ = self
                    .instance
                    .call("abort", &json!({"id": payload["id"]}))
                    .await;
                call.await
            }
        }
    }

    /// Streams `request` through the extension that implements its API, as
    /// the guest emits the events, and aborts the guest's stream when the
    /// request is cancelled.
    async fn stream(&self, request: Request, sender: EventSender) {
        let id = self.next_id();
        let cancel = request.options.cancel.clone();
        let output = new_output(&request.model, now_ms());
        let hooks = &request.options.hooks;
        let payload = json!({
            "id": id,
            "model": request.model,
            "context": {"messages": request.messages},
            "options": crate::streams::options_json(&request.options),
            "hooks": {
                "payload": hooks.payload.is_some(),
                "response": hooks.response.is_some(),
                "streamEvent": hooks.stream_event.is_some(),
            },
        });
        let stream = RunningStream {
            sender: sender.clone(),
            output: Some(output),
            hooks: hooks.clone(),
        };
        lock(&self.bridge.streams).insert(id, stream);
        let result = self.call_abortable("stream", &payload, &cancel).await;
        let running = lock(&self.bridge.streams)
            .remove(&id)
            .and_then(|stream| stream.output);
        if let (Err(err), Some(output)) = (result, running) {
            send_error(&sender, output, &cancel, err.to_string());
        }
    }

    /// The flags the loaded extensions registered.
    pub fn flags(&self) -> Vec<Flag> {
        lock(&self.loaded)
            .iter()
            .flat_map(|extension| {
                let path = text(&extension["path"]);
                list(&extension["flags"])
                    .iter()
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
                        .extension_error("yapi-js", "session_start", &err.to_string(), None)
                }
            }
        }
        if let Err(err) = self.instance.call("bind", &Value::Null).await {
            ctx.ui
                .extension_error("yapi-js", "session_start", &err.to_string(), None);
        }
        *bound = generation + 1;
        true
    }
}

/// A provider an extension registered with `pi.registerProvider(name, config)`.
pub struct RegisteredProvider {
    /// The provider id.
    pub name: String,
    /// Its configuration in `models.json`'s shape.
    pub config: Value,
    /// Its `streamSimple`, which streams the provider's models of the
    /// configuration's `api`.
    pub stream: Option<Arc<dyn Provider>>,
    /// Its `oauth` sign-in.
    pub oauth: Option<Arc<dyn OAuthProvider>>,
}

/// An extension stream the host is running.
struct RunningStream {
    sender: EventSender,
    /// An empty message for failures, until the final event.
    output: Option<AssistantMessage>,
    /// The request's hooks, which the stream's pi options call.
    hooks: RequestHooks,
}

/// An extension provider's `oauth`: pi's legacy sign-in, whose login,
/// refresh and API key run in the guest.
struct JsOAuth {
    host: Weak<ExtensionHost>,
    provider: String,
    name: String,
    subscription: bool,
}

impl JsOAuth {
    /// Runs guest call `kind` for this provider with `payload`, as an
    /// operation that `cancel` aborts and whose prompts go to `interaction`.
    async fn call(
        &self,
        kind: &str,
        mut payload: Value,
        interaction: Option<&Interaction>,
        cancel: &CancellationToken,
    ) -> Result<Value, AuthError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| AuthError::Failed(Error::Stopped.to_string()))?;
        let id = host.next_id();
        payload["provider"] = json!(self.provider);
        payload["id"] = json!(id);
        if let Some(interaction) = interaction {
            lock(&host.bridge.logins).insert(id, interaction.clone());
        }
        let result = host.call_abortable(kind, &payload, cancel).await;
        lock(&host.bridge.logins).remove(&id);
        result.map_err(|err| {
            if cancel.is_cancelled() {
                AuthError::Cancelled
            } else {
                AuthError::Failed(err.to_string())
            }
        })
    }

    async fn credential(
        &self,
        kind: &str,
        payload: Value,
        interaction: Option<&Interaction>,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, AuthError> {
        let value = self.call(kind, payload, interaction, cancel).await?;
        let mut credential: OAuthCredential = serde_json::from_value(value)
            .map_err(|err| AuthError::Failed(format!("Invalid OAuth credentials: {err}")))?;
        credential.extra.remove("type");
        Ok(credential)
    }
}

impl OAuthProvider for JsOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> bool {
        self.subscription
    }

    fn login<'a>(
        &'a self,
        interaction: &'a Interaction,
        _options: &'a LoginOptions,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.credential(
            "oauthLogin",
            json!({}),
            Some(interaction),
            interaction.cancel(),
        ))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.credential(
            "oauthRefresh",
            json!({"credential": credential}),
            None,
            cancel,
        ))
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<OAuthAuth, AuthError>> {
        Box::pin(async move {
            let key = self
                .call(
                    "oauthApiKey",
                    json!({"credential": credential}),
                    None,
                    &CancellationToken::new(),
                )
                .await?;
            Ok(OAuthAuth {
                api_key: key.as_str().map(str::to_owned),
                ..OAuthAuth::default()
            })
        })
    }
}

/// A sign-in question from the guest. pi asks a prompt of an unknown type
/// as a text prompt.
fn auth_prompt(prompt: &Value) -> AuthPrompt {
    serde_json::from_value(prompt.clone()).unwrap_or_else(|_| AuthPrompt::Text {
        message: text(&prompt["message"]),
        placeholder: prompt["placeholder"].as_str().map(str::to_owned),
    })
}

/// A wire API an extension implements, with a provider's `streamSimple` or
/// pi-ai's `registerApiProvider`.
struct JsStream {
    host: Weak<ExtensionHost>,
    api: String,
}

impl Provider for JsStream {
    fn api(&self) -> &str {
        &self.api
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        match self.host.upgrade() {
            Some(host) => {
                tokio::spawn(async move { host.stream(request, sender).await });
            }
            None => send_error(
                &sender,
                new_output(&request.model, now_ms()),
                &request.options.cancel,
                Error::Stopped.to_string(),
            ),
        }
        stream
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
            None => loaded.push(extension.clone()),
        }
    }
    (loaded, errors)
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
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
    ui: &dyn yapi_core::extensions::ExtensionUi,
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
    Ready(std::time::Instant, Option<Vec<AutocompleteItem>>),
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

    /// One `refreshModels` phase of `provider` in the guest: the model list
    /// it returned, if any. A catalog it published to persist is written to
    /// the registry's models store.
    async fn refresh_phase(
        &self,
        provider: &str,
        registry: &ModelRegistry,
        credential: Option<Credential>,
        allow_network: bool,
        options: &RefreshOptions,
    ) -> Result<Option<Vec<ModelDefinition>>, String> {
        let stored = registry
            .models_store()
            .and_then(|store| store.read(provider));
        let payload = json!({
            "provider": provider, "id": self.shared.next_id(), "credential": credential,
            "stored": stored, "allowNetwork": allow_network, "force": options.force,
        });
        let result = self
            .shared
            .call_abortable("refreshModels", &payload, &options.cancel)
            .await
            .map_err(|err| err.to_string())?;
        if let (Some(store), Some(entry)) = (registry.models_store(), result.get("persist")) {
            match entry {
                Value::Null => store.delete(provider, &options.cancel).await,
                entry => store.write(provider, entry.clone(), &options.cancel).await,
            }?;
        }
        let models = &result["models"];
        if models.is_null() {
            return Ok(None);
        }
        serde_json::from_value(models.clone())
            .map(Some)
            .map_err(|err| format!("Invalid models from {provider}: {err}"))
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
            tools.register(js_tool(&self.shared, self.id, tool));
        }
    }

    /// pi awaits `getArgumentCompletions`; the editor asks synchronously, so
    /// a request the guest has not answered yet starts it in the background,
    /// is pending, and has the editor ask again once it is answered.
    fn complete(&self, command: &str, prefix: &str) -> ArgumentCompletions {
        let completes = list(&self.description["commands"])
            .iter()
            .any(|entry| entry["name"] == command && entry["hasCompletions"] == true);
        if !completes {
            return ArgumentCompletions::Ready(None);
        }
        let key = (command.to_owned(), prefix.to_owned());
        {
            let mut cache = lock(&self.completions);
            cache.retain(|_, fetched| match fetched {
                Fetched::Pending => true,
                Fetched::Ready(at, _) => at.elapsed() < COMPLETIONS_TTL,
            });
            match cache.get(&key) {
                Some(Fetched::Ready(_, items)) => return ArgumentCompletions::Ready(items.clone()),
                Some(Fetched::Pending) => return ArgumentCompletions::Pending,
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
                .and_then(|value| serde_json::from_value::<Vec<AutocompleteItem>>(value).ok())
                .filter(|items| !items.is_empty());
            lock(&cache).insert(key, Fetched::Ready(std::time::Instant::now(), items));
            if let Some(session) = host.bridge.session() {
                session.extension_binding().0.refresh_completions();
            }
        });
        ArgumentCompletions::Pending
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

    fn shortcuts(&self) -> Vec<yapi_core::extensions::Shortcut> {
        list(&self.description["shortcuts"])
            .iter()
            .map(|shortcut| yapi_core::extensions::Shortcut {
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

    fn settle(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.shared.instance.settle())
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

    fn refresh_models<'a>(
        &'a self,
        registry: &'a ModelRegistry,
        options: &'a RefreshOptions,
    ) -> BoxFuture<'a, Vec<(String, ModelList)>> {
        Box::pin(async move {
            let providers: Vec<String> = list(&self.description["providers"])
                .iter()
                .filter(|provider| provider["config"]["hasRefreshModels"] == true)
                .map(|provider| text(&provider["name"]))
                .filter(|name| {
                    options
                        .providers
                        .as_ref()
                        .is_none_or(|selected| selected.contains(name))
                })
                .collect();
            let mut lists = Vec::new();
            for provider in providers {
                let credential = registry.store().get(&provider);
                let offline = self
                    .refresh_phase(&provider, registry, credential, false, options)
                    .await;
                let failed = offline.is_err();
                lists.extend(offline.transpose().map(|list| (provider.clone(), list)));
                if failed || !options.allow_network || options.cancel.is_cancelled() {
                    continue;
                }
                let Some(credential) = registry.refresh_credential(&provider).await else {
                    continue;
                };
                let online = self
                    .refresh_phase(&provider, registry, Some(credential), true, options)
                    .await;
                lists.extend(online.transpose().map(|list| (provider.clone(), list)));
            }
            lists
        })
    }

    fn handles(&self, kind: &str) -> bool {
        self.description["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| event == kind))
    }

    fn run_bash<'a>(
        &'a self,
        handle: &'a Value,
        command: &'a str,
        cwd: &'a std::path::Path,
        output: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Option<i32>, String>> {
        Box::pin(async move {
            let id = handle["id"].as_u64().unwrap_or_default();
            let bridge = &self.shared.bridge;
            lock(&bridge.bash).insert(id, output);
            let payload = json!({"id": id, "command": command, "cwd": cwd});
            let run = self.shared.instance.call("bash", &payload);
            tokio::pin!(run);
            let result = tokio::select! {
                result = &mut run => result,
                () = cancel.cancelled() => {
                    // The operations see their signal abort and finish.
                    let _ = self.shared.instance.call("bashAbort", &json!({"id": id})).await;
                    run.await
                }
            };
            lock(&bridge.bash).remove(&id);
            let result = result.map_err(|err| err.to_string())?;
            Ok(result["exitCode"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok()))
        })
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
                // A failing `user_bash` handler stops the command, as in pi.
                Err(err) if kind == "user_bash" => {
                    ctx.ui
                        .extension_error(&self.path_text(), kind, &err.to_string(), None);
                    Some(json!({"error": err.to_string()}))
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
            let (jobs, queued) = tokio::sync::mpsc::unbounded_channel();
            lock(&bridge.updates).insert(call_id.clone(), (updates, cancel.clone(), jobs));
            let session = bridge.session();
            let (ui, mode) = session
                .as_ref()
                .map(AgentSession::extension_binding)
                .unwrap_or_else(|| (Arc::new(yapi_core::extensions::NoUi), Mode::Print));
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
                "id": shared.next_id(), "extension": self.extension, "name": self.declaration.name,
                "toolCallId": call_id, "params": args, "ctx": ctx, "tools": tools,
            });
            // The tool's nested calls run on this task, which also emits the
            // tool's updates, so they report in the order the tool made them.
            let result =
                crate::ops::drive(shared.call_abortable("tool", &payload, &cancel), queued).await;
            lock(&bridge.updates).remove(&call_id);
            tool_result(result.map_err(|err| err.to_string())?)
        })
    }
}

/// An extension tool's result as yapi's.
fn tool_result(mut value: Value) -> Result<ToolResult, String> {
    if value.get("content").is_none_or(Value::is_null) {
        value["content"] = json!([]);
    }
    serde_json::from_value(value).map_err(|err| format!("Invalid tool result: {err}"))
}

/// Where a running extension tool's operations go: [`crate::ops::drive`]
/// runs them on the tool call's task.
type Jobs = tokio::sync::mpsc::UnboundedSender<crate::ops::Job>;

/// Numbers the runtimes of a process, so their component handles stay apart.
static RUNTIMES: AtomicU64 = AtomicU64::new(1);

/// Answers the guest's requests from the bound session.
struct SessionBridge {
    /// This runtime's number.
    runtime_id: u64,
    /// Where actions that outlive a request run.
    runtime: tokio::runtime::Handle,
    session: Mutex<WeakSession>,
    /// Update sinks, cancellation and job queues of running extension tools,
    /// by call id.
    updates: Mutex<HashMap<String, (UpdateSink, CancellationToken, Jobs)>>,
    /// Updates running tools published in the current guest step, held until
    /// the step ends.
    held_updates: Mutex<Vec<(Jobs, crate::ops::Job)>>,
    /// Custom components shown as blocking dialogs, by handle.
    prompts: Mutex<std::collections::HashSet<u64>>,
    /// Where the output of `!` commands that bash operations run goes, by
    /// their operations' id.
    bash: Mutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Interactions of running extension sign-ins, by id.
    logins: Mutex<HashMap<u64, Interaction>>,
    /// Running extension streams by id.
    streams: Mutex<HashMap<u64, RunningStream>>,
    /// The next id of an operation the host may abort in the guest: a
    /// stream, a sign-in or a model refresh.
    next_id: AtomicU64,
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
            held_updates: Mutex::default(),
            bash: Mutex::default(),
            prompts: Mutex::default(),
            logins: Mutex::default(),
            streams: Mutex::default(),
            next_id: AtomicU64::new(1),
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

    fn terminal_input(&self, keys: Vec<String>) -> BoxFuture<'static, Vec<String>> {
        let host = self.0.upgrade();
        Box::pin(async move {
            let Some(host) = host else {
                return keys;
            };
            match host
                .instance
                .call("terminalInput", &json!({"keys": keys}))
                .await
            {
                Ok(Value::Array(results)) => results
                    .into_iter()
                    .filter_map(|key| {
                        key.as_str()
                            .filter(|key| !key.is_empty())
                            .map(str::to_owned)
                    })
                    .collect(),
                // A runtime that cannot run its listeners lets the input through.
                _ => keys,
            }
        })
    }

    fn suggestions(&self, request: Value) -> BoxFuture<'static, Value> {
        let host = self.0.upgrade();
        Box::pin(async move {
            match host {
                Some(host) => host
                    .instance
                    .call("autocomplete", &request)
                    .await
                    .unwrap_or(Value::Null),
                None => Value::Null,
            }
        })
    }

    fn editor_op(&self, handle: u32, op: &Value) {
        if let Some(host) = self.0.upgrade() {
            let mut payload = op.clone();
            payload["handle"] = json!(handle);
            host.instance.post("editor", &payload);
        }
    }
}

impl SessionBridge {
    /// What renders and runs this runtime's UI parts.
    fn components(&self) -> Option<Arc<dyn ComponentHost>> {
        Some(Arc::new(Components(self.owner.get()?.clone())))
    }

    fn component(&self, handle: &Value) -> Option<RemoteComponent> {
        let handle = u32::try_from(handle.as_u64()?).ok()?;
        Some(RemoteComponent::new(
            self.runtime_id,
            handle,
            self.components()?,
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
                    session.ui_prompt_opened("custom", None);
                    lock(&self.prompts).insert(payload["handle"].as_u64().unwrap_or_default());
                    let options = CustomOptions {
                        overlay: payload["overlay"] == true,
                        overlay_options: payload["overlayOptions"].clone(),
                    };
                    ui.custom(component, options);
                }
            }
            "ui.close" => {
                if let Some(component) = self.component(&payload["handle"]) {
                    if lock(&self.prompts).remove(&payload["handle"].as_u64().unwrap_or_default()) {
                        session.ui_prompt_closed();
                    }
                    ui.close(component);
                }
            }
            "ui.setEditor" => ui.set_editor(
                self.component(&payload["handle"]),
                payload["embedsStatus"] == true,
            ),
            "ui.editorChange" => ui.editor_changed(&text(&payload["text"])),
            "ui.editorSubmit" => ui.editor_submit(&text(&payload["text"])),
            "ui.editorAction" => ui.editor_action(&text(&payload["action"])),
            "ui.setTerminalInput" => {
                ui.set_terminal_input(self.components().filter(|_| payload["listening"] == true));
            }
            "ui.setAutocomplete" => {
                if let Some(providers) = self.components() {
                    let triggers = list(&payload["triggerCharacters"])
                        .iter()
                        .map(text)
                        .collect();
                    ui.set_autocomplete(providers, triggers);
                }
            }
            "ui.applyCompletion" => return ui.apply_completion(payload),
            "ui.editorShortcut" => return Value::Bool(ui.editor_shortcut(&text(&payload["data"]))),
            "ui.keybindings" => return ui.keybindings(),
            "ui.requestRender" => ui.request_render(),
            "ui.setTheme" => {
                return match ui.set_theme(&payload["theme"]) {
                    Ok(()) => json!({"success": true}),
                    Err(error) => json!({"success": false, "error": error}),
                };
            }
            "ui.getToolsExpanded" => return Value::Bool(ui.tools_expanded()),
            "ui.setToolsExpanded" => ui.set_tools_expanded(payload["expanded"] == true),
            "ui.theme" => return ui.theme(),
            "ui.getTheme" => return ui.get_theme(&text(&payload["name"])),
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
        timestamp: yapi_core::time::now_ms(),
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
                        .filter(|entry| {
                            entry.meta().and_then(|meta| meta.parent_id.as_deref())
                                == parent.as_deref()
                        })
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
            other => return Err(format!("sessionManager.{other}() is not available in yapi extensions")),
        })
    })
}

impl Bridge for SessionBridge {
    fn request(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        if kind == "bash.data" {
            let sink = lock(&self.bash)
                .get(&payload["id"].as_u64().unwrap_or_default())
                .cloned();
            if let (Some(sink), Some(data)) = (sink, payload["data"].as_str()) {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|err| err.to_string())?;
                let _ = sink.send(bytes);
            }
            return Ok(Value::Null);
        }
        if kind == "tool.update" {
            let caller = lock(&self.updates)
                .get(payload["toolCallId"].as_str().unwrap_or_default())
                .map(|(sink, _, jobs)| (sink.clone(), jobs.clone()));
            if let (Some((sink, jobs)), Ok(partial)) =
                (caller, tool_result(payload["partial"].clone()))
            {
                lock(&self.held_updates).push((jobs, Box::pin(async move { sink(partial) })));
            }
            return Ok(Value::Null);
        }
        if kind == "oauth.notify" {
            let interaction = payload["id"]
                .as_u64()
                .and_then(|id| lock(&self.logins).get(&id).cloned());
            // pi shows no notification of an unknown type.
            let event = serde_json::from_value::<AuthEvent>(payload["event"].clone()).ok();
            if let (Some(interaction), Some(event)) = (interaction, event) {
                interaction.notify(event);
            }
            return Ok(Value::Null);
        }
        if kind == "provider.event" {
            let mut streams = lock(&self.streams);
            let Some(stream) = payload["id"].as_u64().and_then(|id| streams.get_mut(&id)) else {
                return Ok(Value::Null);
            };
            // Events after the final one are dropped, as pi's streams drop them.
            if stream.output.is_none() {
                return Ok(Value::Null);
            }
            match crate::streams::event_from_json(payload) {
                Ok(event) => {
                    if matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_)) {
                        stream.output = None;
                    }
                    stream.sender.send(event);
                }
                // An invalid event fails the stream, without an abort.
                Err(message) => {
                    if let Some(output) = stream.output.take() {
                        send_error(&stream.sender, output, &CancellationToken::new(), message);
                    }
                }
            }
            return Ok(Value::Null);
        }
        let session = self.session().ok_or_else(not_bound)?;
        match kind {
            "mcp.servers" => {
                let servers =
                    serde_json::from_value(payload["servers"].clone()).map_err(|err| err.to_string())?;
                // The change reaches extensions from the runtime.
                let _runtime = self.runtime.enter();
                session.set_mcp_servers(self.runtime_id, servers);
                Ok(Value::Null)
            }
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
                    let source = yapi_core::agent_session::InputSource::Extension;
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
            "session.boundaryContext" => {
                let boundary = yapi_core::agent_session::Boundary::parse(&text(&payload["type"]))
                    .ok_or("Unknown boundary")?;
                let drafts = list(&payload["entries"]);
                session.boundary_context(boundary, drafts)
            }
            "session.context" => {
                let (ui, mode) = session.extension_binding();
                let trusted = session.project_trusted();
                Ok(ctx_data(Some(&session), ui.as_ref(), mode, trusted, &CancellationToken::new()))
            }
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
                    session.tools().register(js_tool(&owner, extension, tool));
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
            "models.error" => Ok(to_json(session.registry().error())),
            "models.providerName" => Ok(Value::String(
                session.registry().provider_name(&text(&payload["provider"])),
            )),
            "models.ofType" => {
                let kind = crate::codemode::model_type(&payload["type"])?;
                Ok(Value::Array(crate::codemode::models_of_type(
                    &session.registry(),
                    kind,
                    payload["provider"].as_str(),
                    false,
                )))
            }
            "agent.isIdle" => Ok(Value::Bool(!session.is_streaming())),
            "agent.systemPromptOptions" => Ok(to_json(session.base_prompt_options())),
            "agent.abort" => {
                session.abort();
                Ok(Value::Null)
            }
            "agent.hasPendingMessages" => Ok(Value::Bool(session.pending_message_count() > 0)),
            "agent.contextUsage" => Ok(session.context_usage().map_or(Value::Null, |usage| {
                json!({"tokens": usage.tokens, "contextWindow": usage.context_window, "percent": usage.percent()})
            })),
            "agent.systemPrompt" => Ok(Value::String(
                yapi_ai::transcript::current_system_message(&session.messages())
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
                Ok(to_json(yapi_core::messages::convert_to_llm(messages)))
            }
            "util.serializeConversation" => {
                let messages: Vec<yapi_types::message::Message> =
                    serde_json::from_value(payload["messages"].clone()).map_err(|err| err.to_string())?;
                Ok(Value::String(yapi_core::compaction::serialize_conversation(&messages)))
            }
            _ => Err(format!("{kind} is not available in yapi extensions yet")),
        }
    }

    /// pi emits a tool's update a few microtasks after the tool publishes it:
    /// after the start of a nested call the tool began in the same step, and
    /// before that call ends. So the step's updates queue behind the jobs of
    /// its nested calls. This relies on [`crate::ops::drive`] polling a tool's
    /// jobs first in, first out: a nested call reports its start on its first
    /// poll, and the update goes out once that call waits, unless the call
    /// ends without waiting.
    fn step_ended(&self) {
        for (jobs, update) in std::mem::take(&mut *lock(&self.held_updates)) {
            // An update after the tool ended is dropped, as in pi.
            let _ = jobs.send(update);
        }
    }

    fn stream_hooks(&self, id: u64) -> RequestHooks {
        lock(&self.streams)
            .get(&id)
            .map(|stream| stream.hooks.clone())
            .unwrap_or_default()
    }

    fn start(&self, kind: &str, payload: Value) -> BoxFuture<'static, Result<Value, String>> {
        if matches!(
            kind,
            "provider.payload" | "provider.response" | "provider.streamEvent"
        ) {
            // A stream's `onPayload`, `onResponse` and `onProviderStreamEvent`.
            let hooks = payload["id"]
                .as_u64()
                .map(|id| self.stream_hooks(id))
                .unwrap_or_default();
            let kind = kind.to_owned();
            return Box::pin(async move {
                match kind.as_str() {
                    "provider.payload" => Ok(hooks.payload(payload["payload"].clone()).await),
                    "provider.response" => {
                        if let Some(response) = &hooks.response {
                            response(ProviderResponse {
                                status: payload["status"]
                                    .as_u64()
                                    .and_then(|status| u16::try_from(status).ok())
                                    .unwrap_or_default(),
                                headers: serde_json::from_value(payload["headers"].clone())
                                    .unwrap_or_default(),
                            })
                            .await;
                        }
                        Ok(Value::Null)
                    }
                    _ => {
                        hooks.stream_event(&payload["data"]).await;
                        Ok(Value::Null)
                    }
                }
            });
        }
        if kind == "oauth.prompt" {
            let interaction = payload["id"]
                .as_u64()
                .and_then(|id| lock(&self.logins).get(&id).cloned());
            return Box::pin(async move {
                let interaction = interaction.ok_or_else(|| AuthError::Cancelled.to_string())?;
                interaction
                    .prompt(auth_prompt(&payload["prompt"]))
                    .await
                    .map(Value::String)
                    .map_err(|err| err.to_string())
            });
        }
        let Some(session) = self.session() else {
            return Box::pin(async { Err(not_bound()) });
        };
        // Nested calls and scripts take the calling tool's cancellation, and
        // run as its jobs.
        let caller = lock(&self.updates)
            .get(payload["toolCallId"].as_str().unwrap_or_default())
            .cloned();
        let (caller_updates, caller_cancel, caller_jobs) = match caller {
            Some((updates, cancel, jobs)) => (updates, cancel, Some(jobs)),
            None => (
                Arc::new(|_| {}) as UpdateSink,
                CancellationToken::new(),
                None,
            ),
        };
        let codemode = self.codemode.clone();
        let kind = kind.to_owned();
        let operation = Box::pin(async move {
            match kind.as_str() {
                "ui.suggestions" => {
                    let (ui, _) = session.extension_binding();
                    Ok(ui.suggestions(payload).await)
                }
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
                "models.availableOfType" => {
                    let kind = crate::codemode::model_type(&payload["type"])?;
                    Ok(Value::Array(crate::codemode::models_of_type(
                        &session.registry(),
                        kind,
                        payload["provider"].as_str(),
                        true,
                    )))
                }
                "models.classify" | "models.generateImages" => {
                    let kind = if kind == "models.classify" {
                        "classifier"
                    } else {
                        "image"
                    };
                    crate::codemode::run_model(
                        &session.registry(),
                        kind,
                        (&text(&payload["provider"]), &text(&payload["id"])),
                        &payload["context"],
                        payload["temperature"].as_f64(),
                        caller_cancel,
                    )
                    .await
                }
                "models.refresh" => {
                    let providers = payload["providers"].as_array().map(|providers| {
                        providers
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    });
                    let options = yapi_ai::model_catalog::RefreshOptions {
                        providers,
                        allow_network: payload["allowNetwork"]
                            .as_bool()
                            .unwrap_or_else(|| std::env::var_os("PI_OFFLINE").is_none()),
                        force: payload["force"].as_bool().unwrap_or(false),
                        cancel: caller_cancel,
                        ..Default::default()
                    };
                    let result = session.refresh_model_catalogs(options).await;
                    let errors: serde_json::Map<String, Value> = result
                        .errors
                        .into_iter()
                        .map(|(provider, error)| (provider, Value::String(error)))
                        .collect();
                    Ok(json!({"aborted": result.aborted, "errors": errors}))
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
                    let set = session.set_model(model).is_ok();
                    // pi's `setModel` returns once `model_select` handlers ran.
                    session.flush_announcements().await;
                    Ok(Value::Bool(set))
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
                "session.new"
                | "session.fork"
                | "session.navigateTree"
                | "session.switch"
                | "session.reload"
                | "session.replaced" => {
                    let action = match kind.as_str() {
                        "session.new" => SessionAction::New {
                            parent: payload["parentSession"].as_str().map(str::to_owned),
                        },
                        "session.fork" => SessionAction::Fork {
                            entry_id: text(&payload["entryId"]),
                            at: payload["position"] == "at",
                        },
                        "session.navigateTree" => SessionAction::Tree {
                            target_id: text(&payload["targetId"]),
                            options: yapi_core::agent_session::TreeNavigation {
                                summarize: payload["summarize"] == true,
                                custom_instructions: payload["customInstructions"]
                                    .as_str()
                                    .map(str::to_owned),
                                replace_instructions: payload["replaceInstructions"] == true,
                                label: payload["label"].as_str().map(str::to_owned),
                            },
                        },
                        "session.switch" => SessionAction::Switch {
                            path: text(&payload["sessionPath"]),
                        },
                        "session.reload" => SessionAction::Reload,
                        _ => SessionAction::Replaced,
                    };
                    let cancelled = session.session_action(action).await?;
                    Ok(json!({ "cancelled": cancelled }))
                }
                "ui.select" | "ui.confirm" | "ui.input" | "ui.editor" => {
                    let (ui, _) = session.extension_binding();
                    let title = text(&payload["title"]);
                    let dialog_kind = &kind["ui.".len()..];
                    session.ui_prompt_opened(dialog_kind, Some(&title));
                    let answer = match dialog_kind {
                        "select" => {
                            let options = list(&payload["options"]).iter().map(text).collect();
                            to_json(ui.select(&title, options, dialog(&payload)).await)
                        }
                        "confirm" => Value::Bool(
                            ui.confirm(&title, &text(&payload["message"]), dialog(&payload))
                                .await,
                        ),
                        "input" => to_json(
                            ui.input(&title, payload["placeholder"].as_str(), dialog(&payload))
                                .await,
                        ),
                        _ => to_json(ui.editor(&title, payload["prefill"].as_str()).await),
                    };
                    session.ui_prompt_closed();
                    // pi's handlers hear of it before the dialog's caller resumes.
                    session.flush_announcements().await;
                    Ok(answer)
                }
                other => Err(format!("{other} is not available in yapi extensions yet")),
            }
        });
        match caller_jobs {
            Some(jobs) => crate::ops::queue(&jobs, operation),
            None => operation,
        }
    }
}
