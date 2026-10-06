//! A conversation: the agent loop bound to a session file, tools, the system prompt
//! and the model registry.
//!
//! Port of the core of `packages/coding-agent/src/core/agent-session.ts` in pi
//! `v1.0.0`: prompts with template expansion, the system prompt as transcript
//! messages, persistence of every message, queues, abort and model selection.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use yapi_agent::hooks::AgentHooks;
use yapi_agent::{
    AgentContext, ExecutionMode, LoopConfig, Tool, ToolCallOutcome, ToolCallScope, UpdateSink,
};
use yapi_ai::api::Apis;
use yapi_ai::registry::ModelRegistry;
use yapi_ai::stream::{Hook, RequestHeaders, RequestHooks, StreamOptions, ThinkingBudgets};
use yapi_types::event::{AgentEvent, SummarySource, ToolResult};
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, ImageContent, Message, StopReason, SystemMessage,
    ThinkingLevel, ToolCall, UserMessage,
};
use yapi_types::model::Model;
use yapi_types::rpc::{PromptDisposition, StreamingBehavior};
use yapi_types::session::FileEntry;
use yapi_types::settings::QueueMode;
use yapi_types::sync::{lock, read, write};

use crate::compaction::{BranchSummary, CompactionSettings};
use crate::extensions::{BashOperations, Extension, ExtensionUi, Loadout, Mode, NoUi, Tools};
use crate::resources::{ContextFile, PromptTemplate, Skill, expand_prompt_template};
use crate::session::SessionManager;
use crate::settings::SettingsManager;
use crate::system_prompt::{PromptOptions, build_sections, diff_sections};
use crate::time::now_ms;
use crate::tools::registry::ToolRegistry;
use crate::tools::{BUILTIN_TOOLS, Described, Exposure, RegisteredTool, Runtime, ToolEnv, builtin};

mod boundary;
mod extensions;
mod models;
mod recovery;
mod stats;

pub use boundary::Boundary;
use extensions::Hooks;
pub use models::CatalogRefresh;
pub use stats::{ContextUsage, SessionStats, UsageTotals};

/// Where user input comes from, as pi's `InputSource` tells extensions'
/// `input` handlers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// Typed in the terminal.
    Interactive,
    /// An RPC client.
    Rpc,
    /// An extension's `sendUserMessage`.
    Extension,
}

impl InputSource {
    fn as_str(self) -> &'static str {
        match self {
            InputSource::Interactive => "interactive",
            InputSource::Rpc => "rpc",
            InputSource::Extension => "extension",
        }
    }
}

/// Why a session takes over from another: pi's `session_start` and
/// `session_shutdown` reasons other than `startup` and `quit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// `/new` or `newSession`.
    New,
    /// `/resume` or `switchSession`.
    Resume,
    /// `/fork`, `/clone` or `fork`.
    Fork,
    /// `/reload` or `reload`.
    Reload,
}

impl Replacement {
    /// The reason as pi's events name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Replacement::New => "new",
            Replacement::Resume => "resume",
            Replacement::Fork => "fork",
            Replacement::Reload => "reload",
        }
    }

    /// The other session's file as pi's `session_start` and
    /// `session_shutdown` report it: not on reload.
    fn reported(self, file: Option<String>) -> Option<String> {
        file.filter(|_| self != Replacement::Reload)
    }
}

/// A session change extensions may cancel, as pi's `session_before_switch`
/// and `session_before_fork` report it.
#[derive(Clone, Debug)]
pub enum SessionChange {
    /// A new session.
    New,
    /// A switch to this session file.
    Resume(String),
    /// A fork at an entry, keeping the entry when `at` (`/clone`) or ending
    /// the fork before it.
    Fork {
        /// The entry.
        entry_id: String,
        /// Keep the entry.
        at: bool,
    },
}

impl SessionChange {
    /// The pi event that may cancel the change.
    pub fn event(&self) -> &'static str {
        match self {
            SessionChange::Fork { .. } => "session_before_fork",
            _ => "session_before_switch",
        }
    }

    /// Why the replacement session starts.
    pub fn reason(&self) -> Replacement {
        match self {
            SessionChange::New => Replacement::New,
            SessionChange::Resume(_) => Replacement::Resume,
            SessionChange::Fork { .. } => Replacement::Fork,
        }
    }
}

/// What `user_bash` handlers decided for a `!` command.
pub enum UserBash {
    /// Run it with the session's shell.
    Local,
    /// Run it through an extension's operations.
    Operations(BashOperations),
    /// An extension ran it; its result, not yet recorded.
    Done(crate::bash_executor::BashResult),
}

/// Receives every session event.
pub type Listener = Box<dyn Fn(&AgentEvent) + Send + Sync>;

/// Discovered resources the prompt uses.
#[derive(Clone, Debug, Default)]
pub struct Resources {
    /// `AGENTS.md` and `CLAUDE.md` files.
    pub context_files: Vec<ContextFile>,
    /// Skills.
    pub skills: Vec<Skill>,
    /// Problems found while loading skills.
    pub skill_diagnostics: Vec<crate::resources::Diagnostic>,
    /// The skill files and directories `skills` came from, each path once.
    pub skill_sources: Vec<crate::resources::SourceInfo>,
    /// Prompt templates.
    pub templates: Vec<PromptTemplate>,
    /// Problems found while loading prompt templates.
    pub template_diagnostics: Vec<crate::resources::Diagnostic>,
    /// The template files and directories `templates` came from, each path
    /// once.
    pub template_sources: Vec<crate::resources::SourceInfo>,
    /// Replaces the default prompt (`SYSTEM.md`, `--system-prompt`).
    pub custom_prompt: Option<String>,
    /// Appended to the prompt (`APPEND_SYSTEM.md`, `--append-system-prompt`).
    pub append_prompt: Option<String>,
    /// Theme files and directories with their sources, in precedence order:
    /// the first file to declare a name wins.
    pub themes: Vec<crate::resources::SourceInfo>,
}

/// What a session starts with.
pub struct SessionConfig {
    /// Working directory.
    pub cwd: PathBuf,
    /// Agent directory.
    pub agent_dir: PathBuf,
    /// Settings.
    pub settings: SettingsManager,
    /// Models and credentials.
    pub registry: ModelRegistry,
    /// Wire APIs.
    pub apis: Apis,
    /// The session file.
    pub session: SessionManager,
    /// The model, if one could be chosen.
    pub model: Option<Model>,
    /// The thinking level.
    pub thinking_level: ThinkingLevel,
    /// Active tool names.
    pub tools: Vec<String>,
    /// The session's extensions, which load before the active tools are set.
    pub extensions: Vec<Arc<dyn Extension>>,
    /// Activate the tools extensions register as they load, unless they opt
    /// out; pi does unless `--tools` names the tools.
    pub include_extension_tools: bool,
    /// The only tools that may exist, from `--tools` (none for `--no-tools`);
    /// every tool when `None`.
    pub allowed_tools: Option<Vec<String>>,
    /// Tools that may never exist, from `--exclude-tools`.
    pub excluded_tools: Vec<String>,
    /// Prompt resources.
    pub resources: Resources,
    /// Where the model and the sign-in help find the docs.
    pub docs: crate::docs::Locations,
}

struct State {
    session: SessionManager,
    model: Option<Model>,
    thinking_level: ThinkingLevel,
    streaming: bool,
}

struct Inner {
    cwd: PathBuf,
    settings: Mutex<SettingsManager>,
    registry: RwLock<Arc<ModelRegistry>>,
    /// Serializes model catalog refreshes.
    catalog_refresh: tokio::sync::Mutex<()>,
    apis: Apis,
    state: Mutex<State>,
    /// The models cycling moves through; empty means every available one.
    scoped_models: Mutex<Vec<crate::model_resolver::ScopedModel>>,
    tools: Tools,
    extensions: Vec<Arc<dyn Extension>>,
    /// The UI and mode extensions see.
    binding: Mutex<(Arc<dyn ExtensionUi>, Mode)>,
    /// Extension sections of the current run's system prompt.
    run_sections: Mutex<IndexMap<String, String>>,
    /// The system prompt a `before_agent_start` handler forced for the run.
    forced_prompt: Mutex<Option<String>>,
    /// Extension messages sent with the next prompt.
    next_turn: Mutex<Vec<Message>>,
    /// Extension messages sent during a turn, appended when it ends.
    pending_custom: Mutex<Vec<Message>>,
    resources: RwLock<Resources>,
    runtime: Arc<RwLock<Runtime>>,
    listeners: Mutex<Vec<Listener>>,
    steering: Mutex<VecDeque<Message>>,
    follow_up: Mutex<VecDeque<Message>>,
    /// Texts of queued steering and follow-up messages until they start, as
    /// `queue_update` reports them.
    queued: Mutex<(Vec<String>, Vec<String>)>,
    cancel: Mutex<Option<CancellationToken>>,
    recovery: Mutex<Recovery>,
    bash: Mutex<Vec<(u64, CancellationToken)>>,
    pending_bash: Mutex<Vec<Message>>,
    agent_dir: PathBuf,
    /// Where the prompt points the model to the docs, fixed for the session
    /// so a first-run download does not change it mid-session.
    docs: crate::docs::Locations,
    /// Woken when a run ends.
    idle: tokio::sync::Notify,
    /// Compactions and branch summaries in progress.
    compacting: AtomicUsize,
    /// A manual compaction is running.
    manual_compaction: std::sync::atomic::AtomicBool,
    /// Cancels the wait before an automatic retry.
    retry_cancel: Mutex<Option<CancellationToken>>,
    /// Calls tools made through [`AgentSession::execute_tool`].
    nested: crate::nested::NestedCalls,
    /// Events for extensions that are delivered in the background, in order.
    announcements: Mutex<VecDeque<Value>>,
    /// Held while announcements are delivered.
    announcing: tokio::sync::Mutex<()>,
    /// How the last turn ended, as pi's boundary events report it.
    outcome: Mutex<&'static str>,
    /// Blocking extension dialogs open, and the kind and title of the
    /// outermost.
    ui_prompts: Mutex<(usize, Option<UiPrompt>)>,
}

/// A blocking extension dialog's kind and title.
type UiPrompt = (String, Option<String>);

/// Counts an operation in [`Inner::compacting`] while alive.
struct Compacting<'a>(&'a AtomicUsize);

impl<'a> Compacting<'a> {
    fn start(counter: &'a AtomicUsize) -> Compacting<'a> {
        counter.fetch_add(1, Ordering::SeqCst);
        Compacting(counter)
    }
}

impl Drop for Compacting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Post-run bookkeeping for retries and overflow recovery, as pi keeps it.
#[derive(Default)]
struct Recovery {
    retry_attempt: u32,
    overflow_recovery_attempted: bool,
    /// The last assistant message of the run and its entry id.
    last_assistant: Option<(AssistantMessage, Option<String>)>,
    /// Entry ids of tool results since the last assistant message.
    turn_tool_results: Vec<String>,
    /// Entry ids of the last completed turn's tool results.
    last_tool_results: Vec<String>,
}

/// Options for [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default)]
pub struct TreeNavigation {
    /// Summarize the branch being left.
    pub summarize: bool,
    /// Extra or replacement summary instructions.
    pub custom_instructions: Option<String>,
    /// Use `custom_instructions` as the whole instruction.
    pub replace_instructions: bool,
    /// Label for the summary, or the target without one.
    pub label: Option<String>,
}

/// What [`AgentSession::navigate_tree`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeOutcome {
    /// Text of a user or custom message target, for the editor.
    pub editor_text: Option<String>,
    /// Nothing changed.
    pub cancelled: bool,
    /// The summary request was cancelled.
    pub aborted: bool,
    /// The branch summary entry, when one was made.
    pub summary_entry: Option<FileEntry>,
}

/// A running conversation. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct AgentSession {
    inner: Arc<Inner>,
}

/// A handle to a session that does not keep it alive.
#[derive(Clone, Default)]
pub struct WeakSession(std::sync::Weak<Inner>);

impl WeakSession {
    /// The session, while it exists.
    pub fn upgrade(&self) -> Option<AgentSession> {
        self.0.upgrade().map(|inner| AgentSession { inner })
    }
}

impl AgentSession {
    /// Starts a session. A new session records its model and thinking level.
    pub fn new(config: SessionConfig) -> AgentSession {
        let SessionConfig {
            cwd,
            agent_dir,
            settings,
            registry,
            apis,
            mut session,
            model,
            thinking_level,
            tools,
            extensions,
            include_extension_tools,
            allowed_tools,
            excluded_tools,
            resources,
            docs,
        } = config;
        let runtime = Arc::new(RwLock::new(Runtime::default()));
        let env = ToolEnv {
            cwd: cwd.clone(),
            runtime: runtime.clone(),
            bin_dir: crate::config::bin_dir(&agent_dir),
        };
        let available: Vec<RegisteredTool> = BUILTIN_TOOLS
            .iter()
            .filter_map(|name| builtin(name, &env))
            .collect();
        let mut tool_set = ToolRegistry::new(available, Vec::new());
        tool_set.restrict(allowed_tools, excluded_tools);
        let tool_registry = Tools::new(tool_set);
        for extension in &extensions {
            extension.load(&tool_registry);
        }
        let mut tools = tools;
        if include_extension_tools {
            tool_registry.with(|registry| {
                for tool in registry.all() {
                    let name = tool.name().to_owned();
                    let builtin = BUILTIN_TOOLS.contains(&name.as_str());
                    if !builtin && tool.exposure.declarable() && tool.default_active {
                        tools.push(name);
                    }
                }
            });
        }
        tool_registry.set_active(tools);

        // As pi: a session with messages gains a missing thinking level; any
        // other records its model and level.
        let has_messages = !session.build_context().messages.is_empty();
        if has_messages {
            let has_thinking = session.branch_path(None).iter().any(|entry| {
                matches!(
                    entry,
                    yapi_types::session::FileEntry::ThinkingLevelChange(_)
                )
            });
            if !has_thinking {
                let _ = session.append_thinking_level_change(thinking_level.as_str());
            }
        } else {
            if let Some(model) = &model {
                let _ = session.append_model_change(&model.provider, &model.id);
            }
            let _ = session.append_thinking_level_change(thinking_level.as_str());
        }

        AgentSession {
            inner: Arc::new(Inner {
                cwd,
                settings: Mutex::new(settings),
                registry: RwLock::new(Arc::new(registry)),
                catalog_refresh: tokio::sync::Mutex::new(()),
                apis,
                state: Mutex::new(State {
                    session,
                    model,
                    thinking_level,
                    streaming: false,
                }),
                scoped_models: Mutex::new(Vec::new()),
                tools: tool_registry,
                extensions,
                binding: Mutex::new((Arc::new(NoUi), Mode::Print)),
                run_sections: Mutex::new(IndexMap::new()),
                forced_prompt: Mutex::new(None),
                next_turn: Mutex::new(Vec::new()),
                pending_custom: Mutex::new(Vec::new()),
                resources: RwLock::new(resources),
                runtime,
                listeners: Mutex::new(Vec::new()),
                steering: Mutex::new(VecDeque::new()),
                follow_up: Mutex::new(VecDeque::new()),
                queued: Mutex::new((Vec::new(), Vec::new())),
                cancel: Mutex::new(None),
                recovery: Mutex::new(Recovery::default()),
                bash: Mutex::new(Vec::new()),
                pending_bash: Mutex::new(Vec::new()),
                docs,
                agent_dir,
                idle: tokio::sync::Notify::new(),
                compacting: AtomicUsize::new(0),
                manual_compaction: std::sync::atomic::AtomicBool::new(false),
                retry_cancel: Mutex::new(None),
                announcements: Mutex::new(VecDeque::new()),
                announcing: tokio::sync::Mutex::new(()),
                outcome: Mutex::new("completed"),
                ui_prompts: Mutex::new((0, None)),
                nested: crate::nested::NestedCalls::default(),
            }),
        }
    }

    /// Where the model and the sign-in help find the docs.
    pub fn docs(&self) -> &crate::docs::Locations {
        &self.inner.docs
    }

    /// Adds a listener for every event.
    pub fn subscribe(&self, listener: Listener) {
        lock(&self.inner.listeners).push(listener);
    }

    fn emit(&self, event: &AgentEvent) {
        for listener in lock(&self.inner.listeners).iter() {
            listener(event);
        }
    }

    /// Runs `f` with the session file.
    pub fn with_session<T>(&self, f: impl FnOnce(&mut SessionManager) -> T) -> T {
        f(&mut lock(&self.inner.state).session)
    }

    /// Moves the session file out, leaving an empty in-memory one, so a new
    /// `AgentSession` can take it over.
    pub fn take_session(&self) -> SessionManager {
        let mut state = lock(&self.inner.state);
        let placeholder = SessionManager::in_memory(state.session.cwd());
        std::mem::replace(&mut state.session, placeholder)
    }

    /// The current model.
    pub fn model(&self) -> Option<Model> {
        lock(&self.inner.state).model.clone()
    }

    /// The current thinking level.
    pub fn thinking_level(&self) -> ThinkingLevel {
        lock(&self.inner.state).thinking_level
    }

    /// Whether a run is in progress.
    pub fn is_streaming(&self) -> bool {
        lock(&self.inner.state).streaming
    }

    /// The model context of the current branch.
    pub fn messages(&self) -> Vec<Message> {
        self.with_session(|session| session.build_context().messages)
    }

    /// The text of the last assistant message.
    pub fn last_assistant(&self) -> Option<yapi_types::message::AssistantMessage> {
        self.messages()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(*assistant),
                _ => None,
            })
    }

    /// Names the session; extensions hear of it as pi's
    /// `session_info_changed`.
    pub fn set_name(&self, name: &str) {
        let name = self.with_session(|session| {
            let _ = session.append_session_info(name);
            session.name()
        });
        self.emit(&AgentEvent::SessionInfoChanged { name: name.clone() });
        let event = serde_json::json!({"type": "session_info_changed", "name": name});
        self.announce(extensions::defined(event, &["name"]));
    }

    /// A snapshot of the merged settings.
    pub fn settings(&self) -> yapi_types::settings::Settings {
        lock(&self.inner.settings).settings().clone()
    }

    /// The `images.autoResize` setting and the current model's resize
    /// profile, for images entering the conversation.
    fn image_options(&self) -> (bool, Option<yapi_types::models::ImageResize>) {
        let auto_resize = lock(&self.inner.settings).settings().image_auto_resize();
        let limits = lock(&self.inner.state)
            .model
            .as_ref()
            .and_then(|model| model.image_resize().cloned());
        (auto_resize, limits)
    }

    /// pi's `getHttpIdleTimeoutMs` for these settings.
    pub fn http_idle_timeout_ms(&self) -> u64 {
        lock(&self.inner.settings).http_idle_timeout_ms()
    }

    /// Settings files that failed to load, as pi's warnings word them.
    pub fn settings_errors(&self) -> Vec<String> {
        lock(&self.inner.settings).errors().to_vec()
    }

    /// Whether this session loads the project's settings and resources.
    pub fn project_trusted(&self) -> bool {
        lock(&self.inner.settings).project_trusted()
    }

    /// Sets a global setting and writes `settings.json`.
    pub fn set_global_setting(
        &self,
        key: &str,
        value: Option<serde_json::Value>,
    ) -> Result<(), String> {
        lock(&self.inner.settings)
            .set(crate::settings::Scope::Global, key, value)
            .map_err(|err| err.to_string())
    }

    /// The prompt resources: those the session was built with, and those
    /// its extensions discovered.
    pub fn resources(&self) -> Resources {
        read(&self.inner.resources).clone()
    }

    /// Empties the steering and follow-up queues and returns their texts.
    pub fn clear_queues(&self) -> (Vec<String>, Vec<String>) {
        lock(&self.inner.steering).clear();
        lock(&self.inner.follow_up).clear();
        let queued = std::mem::take(&mut *lock(&self.inner.queued));
        self.emit_queue_update();
        queued
    }

    /// Queued steering and follow-up messages.
    pub fn pending_message_count(&self) -> usize {
        let queued = lock(&self.inner.queued);
        queued.0.len() + queued.1.len()
    }

    /// How queued steering messages are delivered.
    pub fn steering_mode(&self) -> QueueMode {
        self.settings()
            .steering_mode
            .unwrap_or(QueueMode::OneAtATime)
    }

    /// How queued follow-up messages are delivered.
    pub fn follow_up_mode(&self) -> QueueMode {
        self.settings()
            .follow_up_mode
            .unwrap_or(QueueMode::OneAtATime)
    }

    /// Sets the steering mode in the global settings.
    pub fn set_steering_mode(&self, mode: QueueMode) -> Result<(), String> {
        self.set_global_setting("steeringMode", serde_json::to_value(mode).ok())
    }

    /// Sets the follow-up mode in the global settings.
    pub fn set_follow_up_mode(&self, mode: QueueMode) -> Result<(), String> {
        self.set_global_setting("followUpMode", serde_json::to_value(mode).ok())
    }

    /// Whether threshold and overflow compaction run.
    pub fn auto_compaction_enabled(&self) -> bool {
        CompactionSettings::resolve(lock(&self.inner.settings).settings(), None).enabled
    }

    /// Turns automatic compaction on or off in the global settings.
    pub fn set_auto_compaction(&self, enabled: bool) -> Result<(), String> {
        self.set_nested_global_setting("compaction", "enabled", enabled.into())
    }

    /// Turns automatic retries on or off in the global settings.
    pub fn set_auto_retry(&self, enabled: bool) -> Result<(), String> {
        self.set_nested_global_setting("retry", "enabled", enabled.into())
    }

    /// Sets `key` inside the global object setting `field` and writes
    /// `settings.json`.
    pub fn set_nested_global_setting(
        &self,
        field: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        lock(&self.inner.settings)
            .set_nested(crate::settings::Scope::Global, field, key, value)
            .map_err(|err| err.to_string())
    }

    /// Whether a compaction or branch summary is running.
    pub fn is_compacting(&self) -> bool {
        self.inner.compacting.load(Ordering::SeqCst) > 0
    }

    /// Waits until no run is in progress.
    pub async fn wait_for_idle(&self) {
        loop {
            let idle = self.inner.idle.notified();
            // As pi's `isIdle`, a compaction keeps the session busy.
            if !self.is_streaming() && !self.is_compacting() {
                return;
            }
            idle.await;
        }
    }

    /// Runs a user `!` command in the session's directory, with the
    /// session's shell or through an extension's `operations`, streaming
    /// output to `on_chunk` and as `bash_execution_update` events tagged
    /// `id`, and records it. `exclude_from_context` (`!!`) keeps the output
    /// from the model; it is recorded as given. While a run streams, the
    /// record waits for its end.
    pub async fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: Option<bool>,
        id: Option<String>,
        operations: Option<BashOperations>,
        mut on_chunk: impl FnMut(&str),
    ) -> Result<crate::bash_executor::BashResult, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let token = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = CancellationToken::new();
        lock(&self.inner.bash).push((token, cancel.clone()));
        let settings = self.settings();
        let resolved = match settings.shell_command_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}\n{command}"),
            _ => command.to_owned(),
        };
        let cwd = self.with_session(|session| session.cwd().to_path_buf());
        let on_chunk = |delta: &str| {
            on_chunk(delta);
            self.emit(&AgentEvent::BashExecutionUpdate {
                id: id.clone(),
                delta: delta.to_owned(),
            });
        };
        let result = match &operations {
            None => {
                crate::bash_executor::execute(
                    &resolved,
                    &cwd,
                    settings.shell_path.as_deref(),
                    &crate::config::bin_dir(&self.inner.agent_dir),
                    cancel.clone(),
                    on_chunk,
                )
                .await
            }
            Some(operations) => {
                let run = |output| {
                    operations.extension.run_bash(
                        &operations.handle,
                        &resolved,
                        &cwd,
                        output,
                        cancel.clone(),
                    )
                };
                crate::bash_executor::execute_with(run, &cancel, on_chunk).await
            }
        };
        lock(&self.inner.bash).retain(|(running, _)| *running != token);
        let result = result?;
        self.record_bash(command, &result, exclude_from_context);
        Ok(result)
    }

    /// pi's `user_bash` event for a `!` command: whether an extension ran
    /// it, runs it through its operations, or leaves it to the session's
    /// shell. Fails with the error of a handler, which extension error
    /// reporting has already shown.
    pub async fn user_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<UserBash, String> {
        let handlers = self.handlers_of("user_bash");
        if handlers.is_empty() {
            return Ok(UserBash::Local);
        }
        let event = serde_json::json!({
            "type": "user_bash",
            "command": command,
            "excludeFromContext": exclude_from_context,
            "cwd": self.with_session(|session| session.cwd().to_path_buf()),
        });
        let ctx = self.extension_context(CancellationToken::new());
        for extension in handlers {
            let Some(result) = extension.handle(&ctx, &event).await else {
                continue;
            };
            if let Some(error) = result["error"].as_str() {
                return Err(error.to_owned());
            }
            if !result["operations"].is_null() {
                return Ok(UserBash::Operations(BashOperations {
                    extension: Arc::clone(extension),
                    handle: result["operations"].clone(),
                }));
            }
            if let Ok(done) = serde_json::from_value(result["result"].clone()) {
                return Ok(UserBash::Done(done));
            }
            let error = "Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object";
            ctx.ui
                .extension_error(&extension.source().path, "user_bash", error, None);
            return Err(error.to_owned());
        }
        Ok(UserBash::Local)
    }

    /// pi's `recordBashResult`: records a `!` command's result, after the
    /// current run when one streams.
    pub fn record_bash(
        &self,
        command: &str,
        result: &crate::bash_executor::BashResult,
        exclude_from_context: Option<bool>,
    ) {
        let message = Message::BashExecution(yapi_types::message::BashExecutionMessage {
            command: command.to_owned(),
            output: result.output.clone(),
            exit_code: result.exit_code.map(i64::from),
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.clone(),
            timestamp: now_ms(),
            exclude_from_context,
        });
        if self.is_streaming() {
            lock(&self.inner.pending_bash).push(message);
        } else {
            let _ = self.with_session(|session| session.append_message(message));
        }
    }

    fn flush_pending_bash(&self) {
        let pending: Vec<Message> = lock(&self.inner.pending_bash).drain(..).collect();
        for message in pending {
            let _ = self.with_session(|session| session.append_message(message));
        }
    }

    /// Cancels running `!` commands.
    pub fn abort_bash(&self) {
        for (_, token) in lock(&self.inner.bash).iter() {
            token.cancel();
        }
    }

    /// Whether a `!` command is running.
    pub fn is_bash_running(&self) -> bool {
        !lock(&self.inner.bash).is_empty()
    }

    /// User messages of every branch with text, in file order, as fork points.
    pub fn user_messages_for_forking(&self) -> Vec<(String, String)> {
        self.with_session(|session| {
            session
                .entries()
                .filter_map(|entry| match entry {
                    FileEntry::Message(entry) => match &entry.message {
                        Message::User(user) => {
                            let text = user.content.text("");
                            (!text.is_empty()).then(|| (entry.meta.id.clone(), text))
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        })
    }

    /// pi's `getLastAssistantText`: the text of the last assistant message,
    /// skipping aborted ones without content.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.messages().into_iter().rev().find_map(|message| {
            let Message::Assistant(assistant) = message else {
                return None;
            };
            if assistant.stop_reason == StopReason::Aborted && assistant.content.is_empty() {
                return None;
            }
            let text = assistant_text(&assistant);
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        })
    }

    /// Cancels the current run.
    pub fn abort(&self) {
        if let Some(cancel) = lock(&self.inner.cancel).as_ref() {
            cancel.cancel();
        }
    }

    fn user_message(text: String, images: Vec<ImageContent>) -> Message {
        let mut content = vec![ContentBlock::text(text)];
        content.extend(images.into_iter().map(ContentBlock::Image));
        Message::User(UserMessage {
            content: Content::Blocks(content),
            timestamp: now_ms(),
        })
    }

    /// The part of pi's `sendCustomMessage` that happens at once: the message
    /// is queued, held or recorded and reported before this returns.
    /// `deliver_as` `nextTurn` holds the message for the next prompt; while a
    /// run streams it steers (or follows up) unless `trigger_turn` is false, in
    /// which case it is appended when the turn ends; otherwise `trigger_turn`
    /// returns it to start a run with [`AgentSession::run_triggered`], and
    /// without one it is appended at once.
    pub fn deliver_custom_message(
        &self,
        message: yapi_types::message::CustomMessage,
        trigger_turn: Option<bool>,
        deliver_as: Option<&str>,
    ) -> Option<Message> {
        let message = Message::Custom(message);
        if deliver_as == Some("nextTurn") {
            lock(&self.inner.next_turn).push(message);
        } else if self.is_streaming() && trigger_turn != Some(false) {
            let queue = if deliver_as == Some("followUp") {
                &self.inner.follow_up
            } else {
                &self.inner.steering
            };
            lock(queue).push_back(message);
        } else if trigger_turn == Some(true) {
            return Some(message);
        } else if self.is_streaming() {
            lock(&self.inner.pending_custom).push(message);
        } else {
            self.append_custom_message(&message);
        }
        None
    }

    /// Runs the turn an extension message triggers.
    pub async fn run_triggered(&self, message: Message) {
        self.run(vec![message]).await;
    }

    /// Records an extension message and reports it as pi's `_appendCustomMessage` does.
    fn append_custom_message(&self, message: &Message) {
        let Message::Custom(custom) = message else {
            return;
        };
        self.with_session(|file| {
            let _ = file.append_custom_message(
                &custom.custom_type,
                custom.content.clone(),
                custom.display,
                custom.details.clone(),
            );
        });
        self.emit(&AgentEvent::MessageStart {
            message: message.clone(),
        });
        self.emit(&AgentEvent::MessageEnd {
            message: message.clone(),
        });
    }

    /// Appends extension messages held while a turn ran.
    fn flush_pending_custom(&self) {
        let pending = std::mem::take(&mut *lock(&self.inner.pending_custom));
        for message in &pending {
            self.append_custom_message(message);
        }
    }

    fn emit_queue_update(&self) {
        let (steering, follow_up) = lock(&self.inner.queued).clone();
        self.emit(&AgentEvent::QueueUpdate {
            steering,
            follow_up,
        });
    }

    /// pi's queue display: a queued message leaves it when it starts.
    fn dequeue_started(&self, message: &Message) {
        let Message::User(user) = message else {
            return;
        };
        let text = user.content.text("");
        if text.is_empty() {
            return;
        }
        let removed = {
            let mut queued = lock(&self.inner.queued);
            let (steering, follow_up) = &mut *queued;
            [steering, follow_up].into_iter().any(|texts| {
                let index = texts.iter().position(|queued| *queued == text);
                index.map(|index| texts.remove(index)).is_some()
            })
        };
        if removed {
            self.emit_queue_update();
        }
    }

    /// pi's `steer` and `followUp`: an extension command cannot be queued;
    /// otherwise the `input` handlers see the message first, then it is
    /// queued with templates and skills expanded.
    pub async fn queue_input(
        &self,
        text: &str,
        images: Vec<ImageContent>,
        behavior: StreamingBehavior,
        source: InputSource,
    ) -> Result<PromptDisposition, String> {
        if self.is_extension_command(text) {
            let name = text[1..].split(' ').next().unwrap_or_default();
            return Err(format!(
                "Extension command \"/{name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            ));
        }
        let streaming = self.is_streaming().then_some(behavior);
        let Some((text, images)) = self.input_handlers(text, images, source, streaming).await
        else {
            return Ok(PromptDisposition::Handled);
        };
        self.queue(behavior, self.expand(&text), images);
        Ok(PromptDisposition::Queued)
    }

    /// Queues a message to steer the current run after its current tool calls.
    pub fn steer(&self, text: &str, images: Vec<ImageContent>) {
        self.queue(StreamingBehavior::Steer, self.expand(text), images);
    }

    /// Queues a message for when the current run would otherwise end.
    pub fn follow_up(&self, text: &str, images: Vec<ImageContent>) {
        self.queue(StreamingBehavior::FollowUp, self.expand(text), images);
    }

    fn queue(&self, behavior: StreamingBehavior, text: String, images: Vec<ImageContent>) {
        {
            let mut queued = lock(&self.inner.queued);
            let (texts, queue) = match behavior {
                StreamingBehavior::Steer => (&mut queued.0, &self.inner.steering),
                StreamingBehavior::FollowUp => (&mut queued.1, &self.inner.follow_up),
            };
            texts.push(text.clone());
            lock(queue).push_back(Self::user_message(text, images));
        }
        self.emit_queue_update();
    }

    fn expand(&self, text: &str) -> String {
        let text = self.expand_skill(text);
        expand_prompt_template(&text, &read(&self.inner.resources).templates)
    }

    /// `/skill:name args` becomes the skill's content with the arguments.
    fn expand_skill(&self, text: &str) -> String {
        let Some(rest) = text.strip_prefix("/skill:") else {
            return text.to_owned();
        };
        let (name, args) = match rest.find(char::is_whitespace) {
            Some(index) => (&rest[..index], rest[index..].trim()),
            None => (rest, ""),
        };
        let Some(skill) = read(&self.inner.resources)
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .cloned()
        else {
            return text.to_owned();
        };
        let Ok(content) = std::fs::read_to_string(&skill.file_path) else {
            return text.to_owned();
        };
        let (_, body) = crate::resources::split_frontmatter(&content);
        let mut expanded = format!(
            "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{body}\n</skill>",
            skill.name,
            skill.file_path.display(),
            skill.base_dir.display()
        );
        if !args.is_empty() {
            expanded += &format!("\n\n{args}");
        }
        expanded
    }

    fn prompt_options(&self, active: &[String]) -> PromptOptions {
        let resources = self.resources();
        let mut options = PromptOptions {
            custom_prompt: resources.custom_prompt,
            selected_tools: active.to_vec(),
            append: resources.append_prompt,
            cwd: self.inner.cwd.clone(),
            docs: self.inner.docs.clone(),
            context_files: resources.context_files,
            skills: resources.skills,
            sections: lock(&self.inner.run_sections).clone(),
            ..PromptOptions::default()
        };
        for tool in self.active_tools(active) {
            let name = tool.tool.declaration().name.clone();
            if let Some(snippet) = &tool.snippet {
                options.tool_snippets.insert(name.clone(), snippet.clone());
            }
            if !tool.guidelines.is_empty() {
                options
                    .tool_guidelines
                    .insert(name, tool.guidelines.clone());
            }
        }
        options
    }

    /// The `declared` tools as the model sees them, with the descriptions
    /// the extensions' loadout hooks give them; pi's `_applyToolLoadout`.
    fn loadout(&self, declared: Vec<RegisteredTool>) -> Vec<Arc<dyn Tool>> {
        let loadout = Loadout {
            declared,
            callable: self.callable_tools(),
        };
        let mut descriptions = std::collections::HashMap::new();
        for extension in &self.inner.extensions {
            descriptions.extend(extension.prepare_loadout(&loadout));
        }
        loadout
            .declared
            .into_iter()
            .map(|tool| match descriptions.remove(tool.name()) {
                Some(description) if description != tool.tool.declaration().description => {
                    Arc::new(Described::new(tool.tool, description)) as Arc<dyn Tool>
                }
                _ => tool.tool,
            })
            .collect()
    }

    fn active_tools(&self, active: &[String]) -> Vec<RegisteredTool> {
        self.inner.tools.with(|registry| {
            active
                .iter()
                .filter_map(|name| registry.get(name).cloned())
                .collect()
        })
    }

    /// The system message patch that brings the transcript's prompt up to date.
    fn system_update(
        &self,
        messages: &[Message],
        active: &[String],
    ) -> Result<Option<Message>, String> {
        let sections = build_sections(&self.prompt_options(active))?;
        let current = yapi_ai::transcript::current_system_message(messages)
            .and_then(|system| system.sections)
            .unwrap_or_default();
        Ok(diff_sections(&current, &sections).map(|patch| {
            Message::System(SystemMessage {
                content: Content::Text(String::new()),
                sections: Some(patch),
                timestamp: now_ms(),
                tools_added: None,
                tools_removed: None,
            })
        }))
    }

    /// The system prompt for `active` tools as the model reads it.
    fn system_prompt_text(&self, active: &[String]) -> Result<String, String> {
        let sections = build_sections(&self.prompt_options(active))?;
        Ok(SystemMessage {
            content: Content::Text(String::new()),
            sections: Some(
                sections
                    .into_iter()
                    .map(|(name, text)| (name, Some(text)))
                    .collect(),
            ),
            timestamp: 0,
            tools_added: None,
            tools_removed: None,
        }
        .text())
    }

    /// Sends a user prompt and runs until the agent settles. Templates and
    /// `/skill:` commands expand first.
    pub async fn prompt(&self, text: &str, images: Vec<ImageContent>) -> Result<(), String> {
        self.prompt_with(text, images, None, InputSource::Interactive, |_| {})
            .await
    }

    /// pi's `prompt`: while a run streams, `behavior` queues the message, and
    /// without one it is an error; otherwise the message starts a run that
    /// lasts until the agent settles. `preflight` learns which, before any
    /// event of the run.
    pub async fn prompt_with(
        &self,
        text: &str,
        images: Vec<ImageContent>,
        behavior: Option<StreamingBehavior>,
        source: InputSource,
        preflight: impl FnOnce(PromptDisposition),
    ) -> Result<(), String> {
        if self.run_extension_command(text).await {
            preflight(PromptDisposition::Handled);
            return Ok(());
        }
        if self.inner.manual_compaction.load(Ordering::SeqCst) {
            return Err("Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.".into());
        }
        let streaming = behavior.filter(|_| self.is_streaming());
        let Some((text, images)) = self.input_handlers(text, images, source, streaming).await
        else {
            preflight(PromptDisposition::Handled);
            return Ok(());
        };
        let expanded = self.expand(&text);
        if self.is_streaming() {
            let behavior = behavior.ok_or("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.")?;
            self.queue(behavior, expanded, images);
            preflight(PromptDisposition::Queued);
            return Ok(());
        }
        self.flush_pending_bash();
        self.flush_pending_custom();
        // Without a model pi's agent holds a placeholder from provider
        // "unknown", which has no credential.
        let model = lock(&self.inner.state)
            .model
            .clone()
            .ok_or_else(|| crate::auth_guidance::no_api_key_found("unknown", &self.inner.docs))?;
        let registry = self.registry();
        if let Some(error) = registry.store_error(&model.provider) {
            return Err(error);
        }
        let mut has_auth = registry.has_auth(&model.provider);
        // pi checks a key a command supplies by running it: a command that
        // fails or prints nothing leaves the provider without a key.
        if has_auth && registry.uses_command_key(&model.provider) {
            let auth = registry.auth(&model).await;
            if let Some(error) = auth.error {
                return Err(error);
            }
            has_auth = auth.source.is_some();
        }
        if !has_auth {
            return Err(crate::auth_guidance::no_api_key_found(
                &model.provider,
                &self.inner.docs,
            ));
        }
        let mut sections = IndexMap::new();
        let ctx = self.extension_context(CancellationToken::new());
        for extension in &self.inner.extensions {
            extension.before_agent_start(&ctx, &mut sections).await;
        }
        *lock(&self.inner.run_sections) = sections;
        let active = self.inner.tools.active();
        let (custom, forced) = if self.has_handlers("before_agent_start") {
            let prompt = self.system_prompt_text(&active)?;
            self.before_agent_start_handlers(&expanded, &images, &prompt)
                .await
        } else {
            (Vec::new(), None)
        };
        *lock(&self.inner.forced_prompt) = forced;
        // After the handlers, so a model they select sets the resize profile.
        let (images, hints) = if images.is_empty() {
            (images, Vec::new())
        } else {
            let (auto_resize, limits) = self.image_options();
            tokio::task::spawn_blocking(move || {
                crate::images::normalize_prompt(images, auto_resize, limits.as_ref())
            })
            .await
            .map_err(|err| err.to_string())?
        };
        let expanded = if hints.is_empty() {
            expanded
        } else {
            format!("{expanded}\n\n{}", hints.join("\n"))
        };
        let messages = self.messages();
        let mut prompts = Vec::new();
        if let Some(update) = self.system_update(&messages, &active)? {
            prompts.push(update);
        }
        prompts.push(Self::user_message(expanded, images));
        prompts.append(&mut lock(&self.inner.next_turn));
        prompts.extend(custom);
        preflight(PromptDisposition::Started);
        self.run(prompts).await;
        Ok(())
    }

    /// Runs prompts, then pi's post-run loop: retries, overflow recovery and
    /// compaction, and queued messages, until the agent settles.
    async fn run(&self, prompts: Vec<Message>) {
        let cancel = CancellationToken::new();
        *lock(&self.inner.cancel) = Some(cancel.clone());
        lock(&self.inner.state).streaming = true;
        self.run_agent(Some(prompts), &cancel).await;
        while !cancel.is_cancelled() {
            if self.handle_post_run(&cancel).await {
                if cancel.is_cancelled() {
                    break;
                }
                self.run_agent(None, &cancel).await;
                continue;
            }
            if cancel.is_cancelled() || !self.before_settle(&cancel).await {
                break;
            }
            self.run_agent(None, &cancel).await;
        }
        if cancel.is_cancelled() {
            self.finish_cancelled_retry();
        }
        lock(&self.inner.state).streaming = false;
        self.flush_pending_bash();
        self.flush_pending_custom();
        *lock(&self.inner.cancel) = None;
        // Extensions hear it first, as in pi.
        self.emit_extension_event(
            &serde_json::json!({"type": "agent_settled"}),
            CancellationToken::new(),
        )
        .await;
        self.emit(&AgentEvent::AgentSettled);
        self.inner.idle.notify_waiters();
    }

    fn has_queued(&self) -> bool {
        !lock(&self.inner.steering).is_empty() || !lock(&self.inner.follow_up).is_empty()
    }

    fn stream_options(
        &self,
        model: &Model,
        session_id: String,
        cancel: &CancellationToken,
    ) -> StreamOptions {
        let settings = lock(&self.inner.settings).settings().clone();
        let budgets = settings.thinking_budgets.as_ref();
        let provider = settings
            .retry
            .as_ref()
            .and_then(|retry| retry.provider.as_ref());
        StreamOptions {
            session_id: Some(session_id),
            thinking_budgets: ThinkingBudgets {
                minimal: budgets.and_then(|b| b.minimal),
                low: budgets.and_then(|b| b.low),
                medium: budgets.and_then(|b| b.medium),
                high: budgets.and_then(|b| b.high),
            },
            max_retry_delay_ms: provider.and_then(|provider| provider.max_retry_delay_ms),
            max_retries: provider
                .and_then(|provider| provider.max_retries)
                .unwrap_or(0),
            cancel: cancel.clone(),
            hooks: self.request_hooks(model, cancel),
            ..StreamOptions::default()
        }
    }

    /// What extensions see of the run's provider requests to `model`: pi's
    /// `before_provider_request`, whose handlers each return the body the
    /// next one sees, `before_provider_headers`, `after_provider_response`
    /// and `provider_stream_event`.
    fn request_hooks(&self, model: &Model, cancel: &CancellationToken) -> RequestHooks {
        let model = model.clone();
        RequestHooks {
            payload: self.hook(
                "before_provider_request",
                cancel,
                |session, payload, cancel| {
                    Box::pin(async move {
                        session
                            .chain("before_provider_request", "payload", payload, cancel)
                            .await
                    })
                },
            ),
            headers: self.headers_hook(cancel),
            response: self.hook(
                "after_provider_response",
                cancel,
                |session, response: yapi_ai::stream::ProviderResponse, cancel| {
                    let event = serde_json::json!({
                        "type": "after_provider_response",
                        "status": response.status,
                        "headers": response.headers,
                    });
                    Box::pin(async move {
                        session.emit_extension_event(&event, cancel).await;
                    })
                },
            ),
            stream_event: self.hook(
                "provider_stream_event",
                cancel,
                move |session, data, cancel| {
                    let event = serde_json::json!({
                        "data": data,
                        "type": "provider_stream_event",
                        "provider": model.provider,
                        "api": model.api,
                        "model": model.id,
                    });
                    Box::pin(async move {
                        session.emit_extension_event(&event, cancel).await;
                    })
                },
            ),
        }
    }

    /// pi's `before_provider_headers` for every provider request, compaction
    /// and branch summaries included.
    pub(super) fn headers_hook(
        &self,
        cancel: &CancellationToken,
    ) -> Option<Hook<RequestHeaders, RequestHeaders>> {
        self.hook(
            "before_provider_headers",
            cancel,
            |session, headers, cancel| {
                Box::pin(async move { session.before_provider_headers(headers, cancel).await })
            },
        )
    }

    /// A request hook that runs `event` on this session, when extensions
    /// handle events of type `kind`.
    fn hook<A: 'static, R: 'static>(
        &self,
        kind: &str,
        cancel: &CancellationToken,
        event: impl Fn(AgentSession, A, CancellationToken) -> BoxFuture<'static, R>
        + Send
        + Sync
        + 'static,
    ) -> Option<Hook<A, R>> {
        if !self.has_handlers(kind) {
            return None;
        }
        let (session, cancel) = (self.clone(), cancel.clone());
        Some(Arc::new(move |argument| {
            event(session.clone(), argument, cancel.clone())
        }))
    }

    /// One agent run: with `prompts`, or continuing the transcript (or running
    /// queued messages when it ends with an assistant message).
    async fn run_agent(&self, prompts: Option<Vec<Message>>, cancel: &CancellationToken) {
        let active = self.inner.tools.active();
        let (model, thinking_level, messages, session_id, session_file) = {
            let state = lock(&self.inner.state);
            (
                state.model.clone(),
                state.thinking_level,
                state.session.build_context().messages,
                state.session.id().to_owned(),
                state.session.file().map(Path::to_path_buf),
            )
        };
        let Some(model) = model else {
            return;
        };
        let settings = lock(&self.inner.settings).settings().clone();
        *write(&self.inner.runtime) = Runtime {
            model: Some(model.clone()),
            thinking_level: Some(thinking_level),
            session_id: Some(session_id.clone()),
            session_file,
            auto_resize_images: settings.image_auto_resize(),
        };
        let apis = self.inner.apis.clone();
        let config = LoopConfig {
            options: self.stream_options(&model, session_id, cancel),
            model,
            thinking_level,
            stream: Arc::new(move |request| apis.stream(request)),
            tool_execution: ExecutionMode::Parallel,
        };
        let mut context = AgentContext {
            messages,
            tools: self.loadout(self.active_tools(&active)),
        };
        let cancel_token = cancel.clone();
        let hooks = Hooks {
            session: self.clone(),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            cancel: cancel_token,
            turn_index: std::sync::atomic::AtomicU64::new(0),
        };
        let prompts = match prompts {
            Some(prompts) => Some(prompts),
            None if matches!(context.messages.last(), Some(Message::Assistant(_))) => {
                let steering = drain(&self.inner.steering, settings.steering_mode);
                let queued = if steering.is_empty() {
                    drain(&self.inner.follow_up, settings.follow_up_mode)
                } else {
                    steering
                };
                if queued.is_empty() {
                    return;
                }
                Some(queued)
            }
            None => None,
        };
        match prompts {
            Some(prompts) => {
                yapi_agent::run(prompts, &mut context, config, &hooks).await;
            }
            None => {
                let _ = yapi_agent::run_continue(&mut context, config, &hooks).await;
            }
        }
    }

    /// Moves to another point of the session tree, as `/tree` does. A user or
    /// custom message target puts the leaf on its parent and returns its text for
    /// the editor. With `summarize`, the abandoned branch is summarized at the new
    /// position. `label` labels the summary, or the target without one.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: TreeNavigation,
    ) -> Result<TreeOutcome, String> {
        if self.is_streaming() {
            return Err(
                "Wait for the current response to finish before navigating the session tree."
                    .into(),
            );
        }
        let (old_leaf, target) = self.with_session(|session| {
            (
                session.leaf_id().map(str::to_owned),
                session.entry(target_id).cloned(),
            )
        });
        if old_leaf.as_deref() == Some(target_id) {
            return Ok(TreeOutcome::default());
        }
        let model = self.model();
        if options.summarize && model.is_none() {
            return Err("No model available for summarization".into());
        }
        let Some(target) = target else {
            return Err(format!("Entry {target_id} not found"));
        };
        let (entries, common) = self.with_session(|session| {
            crate::compaction::collect_branch_entries(session, old_leaf.as_deref(), target_id)
        });
        let mut options = options;
        let mut summary = None;
        let mut from_extension = false;
        if self.has_handlers("session_before_tree") {
            let mut preparation = serde_json::json!({
                "targetId": target_id,
                "oldLeafId": old_leaf,
                "commonAncestorId": common,
                "entriesToSummarize": entries,
                "userWantsSummary": options.summarize,
            });
            if let Some(instructions) = &options.custom_instructions {
                preparation["customInstructions"] = instructions.clone().into();
            }
            if options.replace_instructions {
                preparation["replaceInstructions"] = true.into();
            }
            if let Some(label) = &options.label {
                preparation["label"] = label.clone().into();
            }
            let event =
                serde_json::json!({"type": "session_before_tree", "preparation": preparation});
            if let Some(result) = self
                .emit_extension_event(&event, CancellationToken::new())
                .await
            {
                if result["cancel"] == true {
                    return Ok(TreeOutcome {
                        cancelled: true,
                        ..TreeOutcome::default()
                    });
                }
                if options.summarize
                    && let Some(text) = result["summary"]["summary"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                {
                    let details = Some(result["summary"]["details"].clone())
                        .filter(|details| !details.is_null());
                    let usage = serde_json::from_value(result["summary"]["usage"].clone()).ok();
                    summary = Some((text.to_owned(), details, usage));
                    from_extension = true;
                }
                if let Some(instructions) = result["customInstructions"].as_str() {
                    options.custom_instructions = Some(instructions.to_owned());
                }
                if let Some(replace) = result["replaceInstructions"].as_bool() {
                    options.replace_instructions = replace;
                }
                if let Some(label) = result["label"].as_str() {
                    options.label = Some(label.to_owned());
                }
            }
        }
        if options.summarize
            && !entries.is_empty()
            && summary.is_none()
            && let Some(model) = &model
        {
            let cancel = CancellationToken::new();
            *lock(&self.inner.cancel) = Some(cancel.clone());
            let compacting = Compacting::start(&self.inner.compacting);
            let summarizer = self
                .summarizer(
                    model,
                    ThinkingLevel::Off,
                    SummarySource::BranchSummary,
                    None,
                    cancel,
                )
                .await;
            let reserve = lock(&self.inner.settings)
                .settings()
                .branch_summary
                .as_ref()
                .and_then(|settings| settings.reserve_tokens)
                .unwrap_or(16384);
            let result = summarizer
                .branch_summary(
                    &entries,
                    options.custom_instructions.as_deref(),
                    options.replace_instructions,
                    reserve,
                )
                .await;
            *lock(&self.inner.cancel) = None;
            drop(compacting);
            match result? {
                BranchSummary::Aborted => {
                    return Ok(TreeOutcome {
                        cancelled: true,
                        aborted: true,
                        ..TreeOutcome::default()
                    });
                }
                BranchSummary::Done {
                    summary: text,
                    usage,
                    read_files,
                    modified_files,
                } => {
                    summary = Some((
                        text,
                        Some(
                            serde_json::json!({"readFiles": read_files, "modifiedFiles": modified_files}),
                        ),
                        usage,
                    ));
                }
            }
        }
        let (new_leaf, editor_text) = match &target {
            FileEntry::Message(yapi_types::session::MessageEntry {
                meta,
                message: Message::User(user),
            }) => (meta.parent_id.clone(), Some(user.content.text(""))),
            FileEntry::CustomMessage(entry) => {
                (entry.meta.parent_id.clone(), Some(entry.content.text("")))
            }
            _ => (Some(target_id.to_owned()), None),
        };
        let summary_entry = self.with_session(|session| -> Result<Option<FileEntry>, String> {
            let mut summary_entry = None;
            match summary {
                Some((text, details, usage)) => {
                    let id = session
                        .branch_with_summary(
                            new_leaf.as_deref(),
                            text,
                            details,
                            Some(from_extension),
                            usage,
                        )
                        .map_err(|err| err.to_string())?;
                    summary_entry = session.entry(&id).cloned();
                    if let Some(label) = &options.label {
                        session
                            .append_label(&id, Some(label.clone()))
                            .map_err(|err| err.to_string())?;
                    }
                }
                None => {
                    match &new_leaf {
                        None => session.reset_leaf(),
                        Some(id) => session.branch(id).map_err(|err| err.to_string())?,
                    }
                    if let Some(label) = &options.label {
                        session
                            .append_label(target_id, Some(label.clone()))
                            .map_err(|err| err.to_string())?;
                    }
                }
            }
            Ok(summary_entry)
        })?;
        self.restore_tools_from_transcript();
        // pi's `session_tree` event, after the leaf has moved.
        let mut event = serde_json::json!({
            "type": "session_tree",
            "newLeafId": self.with_session(|session| session.leaf_id().map(str::to_owned)),
            "oldLeafId": old_leaf,
        });
        if let Some(entry) = &summary_entry {
            event["summaryEntry"] = serde_json::to_value(entry).unwrap_or_default();
            event["fromExtension"] = from_extension.into();
        }
        self.emit_extension_event(&event, CancellationToken::new())
            .await;
        Ok(TreeOutcome {
            editor_text,
            cancelled: false,
            aborted: false,
            summary_entry,
        })
    }

    /// Activates the tools the current branch's transcript declares, if any.
    fn restore_tools_from_transcript(&self) {
        let messages = self.messages();
        let Some(system) = yapi_ai::transcript::current_system_message(&messages) else {
            return;
        };
        let names: Vec<String> = system
            .tools_added
            .iter()
            .flatten()
            .map(|tool| tool.name.clone())
            .collect();
        self.inner.tools.with(|registry| registry.restore(names));
    }

    /// The session header line as JSON mode prints it.
    pub fn header_json(&self) -> Option<String> {
        self.with_session(|session| {
            session.header().and_then(|header| {
                yapi_types::json::to_string(&yapi_types::session::FileEntry::Session(
                    header.clone(),
                ))
                .ok()
            })
        })
    }

    /// Sets the active tools; unknown and hidden names are ignored.
    pub fn set_active_tools(&self, names: Vec<String>) {
        self.inner.tools.set_active(names);
    }

    /// The active tool names.
    pub fn active_tool_names(&self) -> Vec<String> {
        self.inner.tools.active()
    }

    /// What pi's HTML export takes from a live session: the system prompt as
    /// the model reads it, and the active tools' names, descriptions and
    /// parameter schemas.
    pub fn export_context(&self) -> (Option<String>, Vec<serde_json::Value>) {
        let active = self.inner.tools.active();
        let all = self.inner.tools.all();
        let tools = active
            .iter()
            .filter_map(|name| all.iter().find(|tool| tool.name == *name))
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        (self.system_prompt_text(&active).ok(), tools)
    }

    /// The session's tools.
    pub fn tools(&self) -> &Tools {
        &self.inner.tools
    }

    /// pi's `appendEntry` for extensions: appends custom entry `custom_type`
    /// and reports it as `entry_appended`.
    pub fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<(), String> {
        let entry = self.with_session(|file| {
            let id = file
                .append_custom_entry(custom_type, data)
                .map_err(|err| err.to_string())?;
            Ok::<_, String>(file.entry(&id).cloned())
        })?;
        if let Some(entry) = entry {
            self.emit(&AgentEvent::EntryAppended { entry });
        }
        Ok(())
    }

    /// The tools other tools may call, in registration order: every
    /// `codemode` and `deferred` tool and the active `direct` ones; pi's
    /// `_getCallableTools`.
    pub fn callable_tools(&self) -> Vec<RegisteredTool> {
        self.inner.tools.with(|registry| {
            let active = registry.active();
            registry
                .all()
                .into_iter()
                .filter(|tool| match tool.exposure {
                    Exposure::Codemode | Exposure::Deferred => true,
                    Exposure::Direct => active.iter().any(|name| name == tool.name()),
                    Exposure::ModelOnly | Exposure::Hidden => false,
                })
                .collect()
        })
    }

    /// Runs tool `name` on behalf of the tool call `caller`, as pi's
    /// `ctx.executeTool()`: the call gets the id `<caller>/<n>`, runs through
    /// the tool pipeline and hooks against [`AgentSession::callable_tools`],
    /// emits `tool_execution_*` events with `parentToolCallId`, and is recorded
    /// on the caller's tool result message. Failures come back as error
    /// results. `updates` also receives the tool's partial results.
    pub async fn execute_tool(
        &self,
        caller: &str,
        name: &str,
        args: Value,
        cancel: CancellationToken,
        updates: Option<UpdateSink>,
    ) -> ToolCallOutcome {
        let mut call = ToolCall {
            id: String::new(),
            name: name.to_owned(),
            arguments: match args {
                Value::Object(arguments) => arguments,
                _ => serde_json::Map::new(),
            },
            thought_signature: None,
            namespace: None,
        };
        let nested = &self.inner.nested;
        let started = nested.start(caller, &mut call);
        let hooks = Hooks {
            session: self.clone(),
            steering_mode: None,
            follow_up_mode: None,
            cancel: cancel.clone(),
            turn_index: std::sync::atomic::AtomicU64::new(0),
        };
        let parent = Some(caller.to_owned());
        hooks
            .on_event(&AgentEvent::ToolExecutionStart {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                args: Value::Object(call.arguments.clone()),
                parent_tool_call_id: parent.clone(),
            })
            .await;
        // pi awaits the start event, which lets the caller's queued progress
        // update, such as codemode's running row, go out before the call runs.
        tokio::task::yield_now().await;
        let callable = self.callable_tools();
        let exclusive = !started.holds_queue
            && callable.iter().any(|tool| {
                tool.name() == name && tool.tool.execution_mode() == ExecutionMode::Sequential
            });
        let queue = if exclusive {
            Some(nested.queue.clone().lock_owned().await)
        } else {
            None
        };
        nested.enter(&started, started.holds_queue || exclusive);
        let outcome = match self.last_assistant() {
            None => ToolCallOutcome {
                call: call.clone(),
                result: ToolResult {
                    content: vec![ContentBlock::text("No assistant message issued this call")],
                    details: Some(Value::Object(serde_json::Map::new())),
                    ..ToolResult::default()
                },
                is_error: true,
            },
            Some(assistant) => {
                let tools: Vec<Arc<dyn Tool>> =
                    callable.into_iter().map(|tool| tool.tool).collect();
                let messages = self.messages();
                let scope = ToolCallScope {
                    tools: &tools,
                    assistant: &assistant,
                    messages: &messages,
                    parent: Some(caller),
                    cancel: &cancel,
                };
                let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
                let sink: UpdateSink = Arc::new(move |partial: ToolResult| {
                    if let Some(updates) = &updates {
                        updates(partial.clone());
                    }
                    let _ = sender.send(partial);
                });
                let update = |partial_result| AgentEvent::ToolExecutionUpdate {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    args: Value::Object(call.arguments.clone()),
                    partial_result,
                    parent_tool_call_id: parent.clone(),
                };
                let run = yapi_agent::run_tool_call(&scope, call.clone(), &hooks, sink);
                tokio::pin!(run);
                loop {
                    tokio::select! {
                        biased;
                        Some(partial) = receiver.recv() => hooks.on_event(&update(partial)).await,
                        outcome = &mut run => {
                            while let Ok(partial) = receiver.try_recv() {
                                hooks.on_event(&update(partial)).await;
                            }
                            break outcome;
                        }
                    }
                }
            }
        };
        drop(queue);
        nested.finish(
            started,
            outcome.is_error,
            &yapi_types::message::blocks_text(&outcome.result.content, "\n"),
            outcome.result.usage.as_ref(),
        );
        hooks
            .on_event(&AgentEvent::ToolExecutionEnd {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                result: outcome.result.clone(),
                is_error: outcome.is_error,
                parent_tool_call_id: parent,
            })
            .await;
        outcome
    }

    /// A handle that does not keep the session alive.
    pub fn downgrade(&self) -> WeakSession {
        WeakSession(Arc::downgrade(&self.inner))
    }

    /// The working directory.
    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }
}

/// A skill invocation as `/skill:<name>` expands it; pi's `ParsedSkillBlock`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillBlock {
    /// The skill's name.
    pub name: String,
    /// Where its file is.
    pub location: String,
    /// The text between the tags.
    pub content: String,
    /// What the user wrote after the command, if anything.
    pub user_message: Option<String>,
}

/// pi's `parseSkillBlock`: the skill invocation `text` holds, when it is one.
pub fn parse_skill_block(text: &str) -> Option<SkillBlock> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(" location=\"")?;
    let (location, rest) = rest.split_once('"')?;
    let body = rest.strip_prefix(">\n")?;
    if name.is_empty() || location.is_empty() {
        return None;
    }
    // The content ends at the first closing tag that leaves nothing or a
    // blank line and a message after it.
    let mut from = 0;
    while let Some(found) = body[from..].find("\n</skill>") {
        let end = from + found;
        let after = &body[end + "\n</skill>".len()..];
        let user_message = match after {
            "" => Some(None),
            _ => after
                .strip_prefix("\n\n")
                .filter(|message| !message.is_empty())
                .map(|message| Some(message.trim()).filter(|message| !message.is_empty())),
        };
        if let Some(user_message) = user_message {
            return Some(SkillBlock {
                name: name.to_owned(),
                location: location.to_owned(),
                content: body[..end].to_owned(),
                user_message: user_message.map(str::to_owned),
            });
        }
        from = end + 1;
    }
    None
}

/// Text of an assistant message's text blocks, as print mode prints it.
pub fn assistant_text(message: &yapi_types::message::AssistantMessage) -> String {
    yapi_types::message::blocks_text(&message.content, "")
}

/// Whether an assistant message ended in failure.
pub fn failed(message: &yapi_types::message::AssistantMessage) -> bool {
    matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
}

fn drain(queue: &Mutex<VecDeque<Message>>, mode: Option<QueueMode>) -> Vec<Message> {
    let mut queue = lock(queue);
    match mode {
        Some(QueueMode::All) => queue.drain(..).collect(),
        _ => queue.pop_front().into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_blocks_as_pi() {
        let block = parse_skill_block(
            "<skill name=\"demo\" location=\"/s/SKILL.md\">\nbody\n</skill>\n\n  go now ",
        )
        .expect("a skill block");
        assert_eq!(block.name, "demo");
        assert_eq!(block.location, "/s/SKILL.md");
        assert_eq!(block.content, "body");
        assert_eq!(block.user_message.as_deref(), Some("go now"));
        let block =
            parse_skill_block("<skill name=\"a\" location=\"b\">\nx\n</skill>\ny\n</skill>")
                .expect("the later closing tag");
        assert_eq!(block.content, "x\n</skill>\ny");
        assert_eq!(block.user_message, None);
        assert!(
            parse_skill_block("<skill name=\"a\" location=\"b\">\nx\n</skill> trailing").is_none()
        );
        assert!(parse_skill_block("hello").is_none());
    }

    #[test]
    fn a_poisoned_registry_keeps_its_credentials() {
        let model: Model = serde_json::from_value(serde_json::json!({
            "id": "m", "name": "M", "api": "faux", "provider": "p", "baseUrl": "",
            "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000, "maxTokens": 100,
        }))
        .expect("a model");
        let mut registry = ModelRegistry::builtin();
        registry.register_provider("p", vec![model.clone()]);
        registry.set_runtime_key("p", "key".into());
        let session = AgentSession::new(SessionConfig {
            cwd: PathBuf::from("/work"),
            agent_dir: PathBuf::from("/agent"),
            settings: SettingsManager::in_memory(),
            registry,
            apis: Apis::default(),
            session: SessionManager::in_memory(Path::new("/work")),
            model: Some(model),
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            extensions: Vec::new(),
            include_extension_tools: false,
            allowed_tools: None,
            excluded_tools: Vec::new(),
            resources: Resources::default(),
            docs: crate::docs::Locations::default(),
        });
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = session.inner.registry.write();
            panic!("poison the registry");
        }));
        assert!(session.inner.registry.is_poisoned());
        assert!(session.registry().has_auth("p"));
        assert!(
            session
                .available_models()
                .iter()
                .any(|model| model.provider == "p")
        );
        assert!(session.with_registry(|registry| registry.has_auth("p")));
    }
}
