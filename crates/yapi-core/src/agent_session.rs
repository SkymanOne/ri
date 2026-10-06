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
use yapi_ai::errors::{is_context_overflow, is_recoverable_length, is_retryable_assistant_error};
use yapi_ai::registry::{Auth, ModelRegistry};
use yapi_ai::stream::{StreamOptions, ThinkingBudgets};
use yapi_types::event::{AgentEvent, ToolResult};
use yapi_types::event::{CompactionReason, CompactionResult, SummarySource};
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, ImageContent, Message, StopReason, SystemMessage,
    ThinkingLevel, ToolCall, ToolResultMessage, UserMessage,
};
use yapi_types::model::Model;
use yapi_types::rpc::{PromptDisposition, StreamingBehavior};
use yapi_types::session::FileEntry;
use yapi_types::settings::QueueMode;
use yapi_types::sync::{lock, read, write};

use crate::compaction::{
    BranchSummary, CompactionSettings, Preparation, RetryPolicy, Summarizer, SummaryRetry,
    calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
    estimate_tokens, prepare_compaction, should_compact,
};
use crate::extensions::{
    Context, Extension, ExtensionUi, Loadout, Mode, NoUi, ToolRenderers, Tools,
};
use crate::messages::convert_to_llm;
use crate::resources::{ContextFile, PromptTemplate, Skill, expand_prompt_template};
use crate::session::{SessionManager, build_projection};
use crate::settings::SettingsManager;
use crate::system_prompt::{PromptOptions, build_sections, diff_sections};
use crate::time::{now_ms, parse_iso};
use crate::tools::registry::ToolRegistry;
use crate::tools::{BUILTIN_TOOLS, Described, Exposure, RegisteredTool, Runtime, ToolEnv, builtin};

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

fn behavior_name(behavior: StreamingBehavior) -> &'static str {
    match behavior {
        StreamingBehavior::Steer => "steer",
        StreamingBehavior::FollowUp => "followUp",
    }
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
    /// Prompt templates.
    pub templates: Vec<PromptTemplate>,
    /// Problems found while loading prompt templates.
    pub template_diagnostics: Vec<crate::resources::Diagnostic>,
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
    resources: Resources,
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
}

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

/// How full the context window is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    /// Estimated context tokens; unknown right after a compaction.
    pub tokens: Option<u64>,
    /// The model's context window.
    pub context_window: u64,
}

impl ContextUsage {
    /// Percent of the window in use, when known.
    pub fn percent(&self) -> Option<f64> {
        self.tokens
            .map(|tokens| tokens as f64 / self.context_window as f64 * 100.0)
    }
}

/// Session-wide token and cost totals.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotals {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache read tokens.
    pub cache_read: u64,
    /// Cache write tokens.
    pub cache_write: u64,
    /// Cost in US dollars.
    pub cost: f64,
    /// Cache hit rate of the latest assistant message, in percent.
    pub cache_hit_rate: Option<f64>,
}

/// Message counts and per-model cost, as `/session` shows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    /// Message entries.
    pub total_messages: usize,
    /// User messages.
    pub user_messages: usize,
    /// Assistant messages.
    pub assistant_messages: usize,
    /// Tool calls in assistant messages.
    pub tool_calls: usize,
    /// Tool results.
    pub tool_results: usize,
    /// Cost and tokens by `provider/model`, costliest first; summaries and
    /// tool usage are grouped as `Tools/summaries`.
    pub breakdown: Vec<(String, f64, u64)>,
}

/// What the compaction check decided.
enum CompactionCheck {
    None,
    Overflow { will_retry: bool },
    OverflowFailed(String),
    Threshold,
}

/// Outcome of a model catalog refresh.
#[derive(Clone, Debug, Default)]
pub struct CatalogRefresh {
    /// Providers whose refresh failed, with why.
    pub errors: Vec<(String, String)>,
    /// Whether the refresh was cancelled before it finished.
    pub aborted: bool,
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
                resources,
                runtime,
                listeners: Mutex::new(Vec::new()),
                steering: Mutex::new(VecDeque::new()),
                follow_up: Mutex::new(VecDeque::new()),
                queued: Mutex::new((Vec::new(), Vec::new())),
                cancel: Mutex::new(None),
                recovery: Mutex::new(Recovery::default()),
                bash: Mutex::new(Vec::new()),
                pending_bash: Mutex::new(Vec::new()),
                agent_dir,
                idle: tokio::sync::Notify::new(),
                compacting: AtomicUsize::new(0),
                manual_compaction: std::sync::atomic::AtomicBool::new(false),
                retry_cancel: Mutex::new(None),
                nested: crate::nested::NestedCalls::default(),
            }),
        }
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

    /// Context use of the current branch: `tokens` is `None` after a compaction
    /// until the model responds again. `None` without a model or window.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.model()?;
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }
        self.with_session(|session| {
            let branch = session.branch_path(None);
            let projection = build_projection(&branch);
            let latest_compaction = branch
                .iter()
                .rposition(|entry| matches!(entry, FileEntry::Compaction(_)));
            if let Some(compaction) = latest_compaction {
                let has_usage = |entry: &&FileEntry| {
                    let Some(id) = entry.meta().map(|meta| meta.id.as_str()) else {
                        return false;
                    };
                    projection.entries.iter().any(|projected| {
                        projected.source.meta().is_some_and(|meta| meta.id == id)
                            && projected.messages.iter().any(|message| {
                                matches!(message, Message::Assistant(assistant)
                                    if !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error)
                                        && calculate_context_tokens(&assistant.usage) > 0)
                            })
                    })
                };
                if !branch[compaction + 1..].iter().any(has_usage) {
                    return Some(ContextUsage {
                        tokens: None,
                        context_window,
                    });
                }
            }
            let tokens = estimate_projected_context_tokens(&projection, &branch).tokens;
            Some(ContextUsage {
                tokens: Some(tokens),
                context_window,
            })
        })
    }

    /// Token and cost totals over every entry of the session file, and the
    /// cache hit rate of the latest assistant message.
    pub fn usage_totals(&self) -> UsageTotals {
        self.with_session(|session| {
            let mut totals = UsageTotals::default();
            for entry in session.entries() {
                let usage = match entry {
                    FileEntry::Usage(entry) => Some(&entry.usage),
                    FileEntry::Message(entry) => match &entry.message {
                        Message::Assistant(assistant) => {
                            let usage = &assistant.usage;
                            let prompt = usage.input + usage.cache_read + usage.cache_write;
                            totals.cache_hit_rate = (prompt > 0)
                                .then(|| usage.cache_read as f64 / prompt as f64 * 100.0);
                            Some(usage)
                        }
                        Message::ToolResult(result) => result.usage.as_ref(),
                        _ => None,
                    },
                    FileEntry::Compaction(entry) => entry.usage.as_ref(),
                    FileEntry::BranchSummary(entry) => entry.usage.as_ref(),
                    _ => None,
                };
                if let Some(usage) = usage {
                    totals.input += usage.input;
                    totals.output += usage.output;
                    totals.cache_read += usage.cache_read;
                    totals.cache_write += usage.cache_write;
                    totals.cost += usage.cost.total;
                }
            }
            totals
        })
    }

    /// pi's `getSessionStats` counts and `getUsageCostBreakdown`, over every
    /// entry of the session file.
    pub fn session_stats(&self) -> SessionStats {
        self.with_session(|session| {
            let mut stats = SessionStats::default();
            let mut breakdown: Vec<(String, f64, u64)> = Vec::new();
            let mut add = |key: String, usage: &yapi_types::message::Usage| {
                let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
                match breakdown
                    .iter_mut()
                    .find(|(existing, _, _)| *existing == key)
                {
                    Some((_, cost, total)) => {
                        *cost += usage.cost.total;
                        *total += tokens;
                    }
                    None => breakdown.push((key, usage.cost.total, tokens)),
                }
            };
            for entry in session.entries() {
                match entry {
                    FileEntry::Usage(entry) => {
                        add(format!("{}/{}", entry.provider, entry.model), &entry.usage);
                    }
                    FileEntry::Compaction(entry) => {
                        if let Some(usage) = &entry.usage {
                            add("Tools/summaries".into(), usage);
                        }
                    }
                    FileEntry::BranchSummary(entry) => {
                        if let Some(usage) = &entry.usage {
                            add("Tools/summaries".into(), usage);
                        }
                    }
                    FileEntry::Message(entry) => {
                        stats.total_messages += 1;
                        match &entry.message {
                            Message::User(_) => stats.user_messages += 1,
                            Message::ToolResult(result) => {
                                stats.tool_results += 1;
                                if let Some(usage) = &result.usage {
                                    add("Tools/summaries".into(), usage);
                                }
                            }
                            Message::Assistant(assistant) => {
                                stats.assistant_messages += 1;
                                stats.tool_calls += assistant
                                    .content
                                    .iter()
                                    .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                                    .count();
                                let model = assistant
                                    .response_model
                                    .as_deref()
                                    .unwrap_or(&assistant.model);
                                add(format!("{}/{model}", assistant.provider), &assistant.usage);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            breakdown.retain(|(_, cost, tokens)| *cost > 0.0 || *tokens > 0);
            breakdown.sort_by(|a, b| b.1.total_cmp(&a.1));
            stats.breakdown = breakdown;
            stats
        })
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

    /// pi's `setModel`: switches to `model` and records it, then applies the
    /// thinking level for it. Fails when its provider has no credential.
    pub fn set_model(&self, model: Model) -> Result<(), String> {
        if !self.registry().has_auth(&model.provider) {
            return Err(format!("No API key for {}/{}", model.provider, model.id));
        }
        self.apply_model(model, None);
        Ok(())
    }

    /// Takes over the model, thinking level and scope of the session this one
    /// replaces on `/reload`, which pi keeps without recording anything.
    pub fn keep_selection(&self, from: &AgentSession) {
        let (model, level) = {
            let state = lock(&from.inner.state);
            (state.model.clone(), state.thinking_level)
        };
        {
            let mut state = lock(&self.inner.state);
            state.model = model;
            state.thinking_level = level;
        }
        self.set_scoped_models(from.scoped_models());
    }

    /// The models cycling moves through, from `--models` or the
    /// `enabledModels` setting; empty when every available model is in scope.
    pub fn scoped_models(&self) -> Vec<crate::model_resolver::ScopedModel> {
        lock(&self.inner.scoped_models).clone()
    }

    /// The scoped models, or every available model when the scope is empty.
    pub fn models_in_scope(&self) -> Vec<Model> {
        let scoped = self.scoped_models();
        if scoped.is_empty() {
            self.available_models()
        } else {
            scoped.into_iter().map(|entry| entry.model).collect()
        }
    }

    /// Replaces the scope; an empty list puts every available model in it.
    pub fn set_scoped_models(&self, models: Vec<crate::model_resolver::ScopedModel>) {
        *lock(&self.inner.scoped_models) = models;
    }

    /// Saves `model` as the default in the global settings. A non-empty scope
    /// gains it, and so does a non-empty `enabledModels` setting, as in pi.
    pub fn save_default_model(&self, model: &Model) {
        let _ = self.set_global_setting(
            "defaultProvider",
            Some(serde_json::Value::String(model.provider.clone())),
        );
        let _ = self.set_global_setting(
            "defaultModel",
            Some(serde_json::Value::String(model.id.clone())),
        );
        {
            let mut scoped = lock(&self.inner.scoped_models);
            if scoped.is_empty()
                || scoped
                    .iter()
                    .any(|entry| entry.model.is(&model.provider, &model.id))
            {
                return;
            }
            scoped.push(crate::model_resolver::ScopedModel {
                model: model.clone(),
                thinking_level: None,
            });
        }
        let Some(enabled) = self
            .settings()
            .enabled_models
            .filter(|list| !list.is_empty())
        else {
            return;
        };
        let reference = model.reference();
        if enabled
            .iter()
            .any(|pattern| pattern.eq_ignore_ascii_case(&reference))
        {
            return;
        }
        let mut list: Vec<serde_json::Value> =
            enabled.into_iter().map(serde_json::Value::String).collect();
        list.push(serde_json::Value::String(reference));
        let _ = self.set_global_setting("enabledModels", Some(serde_json::Value::Array(list)));
    }

    /// Records `model` and applies the thinking level for it: `explicit`,
    /// else its level in settings, else the default level, else the current
    /// one.
    fn apply_model(&self, model: Model, explicit: Option<ThinkingLevel>) {
        let settings = self.settings();
        let level = explicit
            .or_else(|| {
                settings
                    .model_thinking_levels
                    .as_ref()
                    .and_then(|levels| levels.get(&model.reference()))
                    .copied()
            })
            .or_else(|| lock(&self.inner.settings).default_thinking_level())
            .unwrap_or_else(|| self.thinking_level());
        {
            let mut state = lock(&self.inner.state);
            let _ = state
                .session
                .append_model_change(&model.provider, &model.id);
            state.model = Some(model);
        }
        self.set_thinking_level(level);
    }

    /// pi's `cycleModel`, forward or backward, over the scoped models that
    /// have credentials, or over every available model when the scope is
    /// empty. Returns the new model and whether the scope applied; `None`
    /// when there is at most one model to cycle through.
    pub fn cycle_model(&self, forward: bool) -> Option<(Model, bool)> {
        let scoped = self.scoped_models();
        let available = self.available_models();
        let is_scoped = !scoped.is_empty();
        let models: Vec<(Model, Option<ThinkingLevel>)> = if is_scoped {
            scoped
                .into_iter()
                .filter(|entry| {
                    available
                        .iter()
                        .any(|m| m.is(&entry.model.provider, &entry.model.id))
                })
                .map(|entry| (entry.model, entry.thinking_level))
                .collect()
        } else {
            available.into_iter().map(|model| (model, None)).collect()
        };
        if models.len() <= 1 {
            return None;
        }
        let index = self
            .model()
            .and_then(|current| {
                models
                    .iter()
                    .position(|(model, _)| model.is(&current.provider, &current.id))
            })
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % models.len()
        } else {
            (index + models.len() - 1) % models.len()
        };
        let (model, level) = models[next].clone();
        self.apply_model(model.clone(), level);
        Some((model, is_scoped))
    }

    /// Sets the thinking level, clamped to what the model supports; a change
    /// is recorded and announced.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let mut state = lock(&self.inner.state);
        let level = match &state.model {
            Some(model) => yapi_ai::thinking::clamp_level(model, level),
            None => level,
        };
        if level == state.thinking_level {
            return;
        }
        state.thinking_level = level;
        let _ = state.session.append_thinking_level_change(level.as_str());
        drop(state);
        self.emit(&AgentEvent::ThinkingLevelChanged { level });
    }

    /// Names the session.
    pub fn set_name(&self, name: &str) {
        let name = self.with_session(|session| {
            let _ = session.append_session_info(name);
            session.name()
        });
        self.emit(&AgentEvent::SessionInfoChanged { name });
    }

    /// A snapshot of the merged settings.
    pub fn settings(&self) -> yapi_types::settings::Settings {
        lock(&self.inner.settings).settings().clone()
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

    /// The prompt resources the session was built with.
    pub fn resources(&self) -> &Resources {
        &self.inner.resources
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

    /// Cancels the wait before an automatic retry; the failed response stands.
    pub fn abort_retry(&self) {
        let cancel = lock(&self.inner.retry_cancel).clone();
        if let Some(cancel) = cancel {
            // The retry's end is reported before this returns, as pi reports
            // it before answering `abort_retry`; the waiting retry then finds
            // nothing left to report.
            self.finish_cancelled_retry();
            cancel.cancel();
        }
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

    /// The thinking levels the current model supports; all of them without a
    /// model.
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            Some(model) => yapi_ai::thinking::supported_levels(&model),
            None => ThinkingLevel::ALL.to_vec(),
        }
    }

    /// pi's `cycleThinkingLevel`: the next supported level, wrapping. `None`
    /// when the model does not reason.
    pub fn cycle_thinking_level(&self) -> Option<ThinkingLevel> {
        let model = self.model()?;
        if !model.reasoning {
            return None;
        }
        let levels = yapi_ai::thinking::supported_levels(&model);
        let current = self.thinking_level();
        let index = levels.iter().position(|level| *level == current);
        let next = levels[index.map_or(0, |index| (index + 1) % levels.len())];
        self.set_thinking_level(next);
        Some(self.thinking_level())
    }

    /// Models with credentials, in catalog order.
    pub fn available_models(&self) -> Vec<Model> {
        read(&self.inner.registry)
            .available()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Runs a user `!` command in the session's directory, streaming output to
    /// `on_chunk` and as `bash_execution_update` events tagged `id`, and
    /// records it. `exclude_from_context` (`!!`) keeps the output from the
    /// model; it is recorded as given. While a run streams, the record waits
    /// for its end.
    pub async fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: Option<bool>,
        id: Option<String>,
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
        let result = crate::bash_executor::execute(
            &resolved,
            &cwd,
            settings.shell_path.as_deref(),
            &crate::config::bin_dir(&self.inner.agent_dir),
            cancel.clone(),
            |delta| {
                on_chunk(delta);
                self.emit(&AgentEvent::BashExecutionUpdate {
                    id: id.clone(),
                    delta: delta.to_owned(),
                });
            },
        )
        .await;
        lock(&self.inner.bash).retain(|(running, _)| *running != token);
        let result = result?;
        self.record_bash(command, &result, exclude_from_context);
        Ok(result)
    }

    fn record_bash(
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
            if let Some(index) = steering.iter().position(|queued| *queued == text) {
                steering.remove(index);
                true
            } else if let Some(index) = follow_up.iter().position(|queued| *queued == text) {
                follow_up.remove(index);
                true
            } else {
                false
            }
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
            match behavior {
                StreamingBehavior::Steer => queued.0.push(text.clone()),
                StreamingBehavior::FollowUp => queued.1.push(text.clone()),
            }
        }
        let queue = match behavior {
            StreamingBehavior::Steer => &self.inner.steering,
            StreamingBehavior::FollowUp => &self.inner.follow_up,
        };
        lock(queue).push_back(Self::user_message(text, images));
        self.emit_queue_update();
    }

    fn expand(&self, text: &str) -> String {
        let text = self.expand_skill(text);
        expand_prompt_template(&text, &self.inner.resources.templates)
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
        let Some(skill) = self
            .inner
            .resources
            .skills
            .iter()
            .find(|skill| skill.name == name)
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
        let mut options = PromptOptions {
            custom_prompt: self.inner.resources.custom_prompt.clone(),
            selected_tools: active.to_vec(),
            append: self.inner.resources.append_prompt.clone(),
            cwd: self.inner.cwd.clone(),
            context_files: self.inner.resources.context_files.clone(),
            skills: self.inner.resources.skills.clone(),
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
            .ok_or_else(|| crate::auth_guidance::no_api_key_found("unknown"))?;
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
            return Err(crate::auth_guidance::no_api_key_found(&model.provider));
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
            if cancel.is_cancelled() || !self.has_queued() {
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

    fn stream_options(&self, session_id: String, cancel: &CancellationToken) -> StreamOptions {
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
            ..StreamOptions::default()
        }
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
        *write(&self.inner.runtime) = Runtime {
            model: Some(model.clone()),
            thinking_level: Some(thinking_level),
            session_id: Some(session_id.clone()),
            session_file,
        };
        let settings = lock(&self.inner.settings).settings().clone();
        let apis = self.inner.apis.clone();
        let config = LoopConfig {
            model,
            thinking_level,
            stream: Arc::new(move |request| apis.stream(request)),
            options: self.stream_options(session_id, cancel),
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

    /// The model whose limits apply to a response: the current one when it made it.
    fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        self.model()
            .filter(|model| model.is(&message.provider, &message.model))
    }

    fn is_retryable(&self, message: &AssistantMessage) -> bool {
        let window = self
            .model_for_message(message)
            .or_else(|| self.model())
            .map_or(0, |model| model.context_window);
        !is_context_overflow(message, window) && is_retryable_assistant_error(message)
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::resolve(lock(&self.inner.settings).settings())
    }

    /// Whether the run that just ended will be retried, for `agent_end`.
    fn will_retry_after(&self, messages: &[Message]) -> bool {
        let cancelled = lock(&self.inner.cancel)
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);
        let policy = self.retry_policy();
        if cancelled
            || !policy.enabled
            || lock(&self.inner.recovery).retry_attempt >= policy.max_retries
        {
            return false;
        }
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(self.is_retryable(assistant)),
                _ => None,
            })
            .unwrap_or(false)
    }

    fn finish_cancelled_retry(&self) {
        let attempt = std::mem::take(&mut lock(&self.inner.recovery).retry_attempt);
        if attempt > 0 {
            self.emit(&AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".into()),
            });
        }
    }

    /// Drops a failed or truncated attempt from the model's context while keeping
    /// it in the session history.
    fn omit_recovery_attempt(&self, entry_ids: &[String]) {
        for id in entry_ids {
            let entry = self.with_session(|session| {
                let edit = session.append_context_edit(id, None).ok()?;
                session.entry(&edit).cloned()
            });
            if let Some(entry) = entry {
                self.emit(&AgentEvent::EntryAppended { entry });
            }
        }
    }

    async fn prepare_retry(
        &self,
        message: &AssistantMessage,
        entry_id: Option<String>,
        cancel: &CancellationToken,
    ) -> bool {
        let policy = self.retry_policy();
        if !policy.enabled {
            return false;
        }
        let attempt = {
            let mut recovery = lock(&self.inner.recovery);
            if recovery.retry_attempt >= policy.max_retries {
                return false;
            }
            recovery.retry_attempt += 1;
            recovery.retry_attempt
        };
        let delay_ms = policy.delay_ms(attempt);
        self.emit(&AgentEvent::AutoRetryStart {
            attempt,
            max_attempts: policy.max_retries,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "Unknown error".into()),
        });
        self.omit_recovery_attempt(&entry_id.into_iter().collect::<Vec<_>>());
        let retry = cancel.child_token();
        *lock(&self.inner.retry_cancel) = Some(retry.clone());
        let waited = tokio::select! {
            () = retry.cancelled() => {
                self.finish_cancelled_retry();
                false
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
        };
        *lock(&self.inner.retry_cancel) = None;
        waited
    }

    async fn handle_post_run(&self, cancel: &CancellationToken) -> bool {
        let (last, tool_results) = {
            let mut recovery = lock(&self.inner.recovery);
            (
                recovery.last_assistant.take(),
                std::mem::take(&mut recovery.last_tool_results),
            )
        };
        if cancel.is_cancelled() {
            self.finish_cancelled_retry();
            return false;
        }
        let Some((message, entry_id)) = last else {
            return self.has_queued();
        };
        if self.is_retryable(&message)
            && self.prepare_retry(&message, entry_id.clone(), cancel).await
        {
            return !cancel.is_cancelled();
        }
        if cancel.is_cancelled() {
            self.finish_cancelled_retry();
            return false;
        }
        if message.stop_reason == StopReason::Error {
            let attempt = std::mem::take(&mut lock(&self.inner.recovery).retry_attempt);
            if attempt > 0 {
                self.emit(&AgentEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: message.error_message.clone(),
                });
            }
        }
        if self
            .check_compaction(&message, entry_id, tool_results, cancel)
            .await
        {
            return !cancel.is_cancelled();
        }
        !cancel.is_cancelled() && self.has_queued()
    }

    /// pi's `_checkCompaction`: overflow recovery, then threshold compaction.
    async fn check_compaction(
        &self,
        message: &AssistantMessage,
        entry_id: Option<String>,
        tool_results: Vec<String>,
        cancel: &CancellationToken,
    ) -> bool {
        let Some(model) = self.model() else {
            return false;
        };
        let settings =
            CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
        if !settings.enabled || message.stop_reason == StopReason::Aborted {
            return false;
        }
        let message_model = self.model_for_message(message);
        let context_window = message_model.as_ref().unwrap_or(&model).context_window;
        let overflow_attempted = lock(&self.inner.recovery).overflow_recovery_attempted;
        let check = self.with_session(|session| {
            let branch = session.branch_path(None);
            let latest_compaction = branch.iter().rev().find_map(|entry| match entry {
                FileEntry::Compaction(compaction) => Some(compaction),
                _ => None,
            });
            let compaction_time =
                latest_compaction.and_then(|compaction| parse_iso(&compaction.meta.timestamp));
            if compaction_time.is_some_and(|time| message.timestamp <= time) {
                return CompactionCheck::None;
            }
            let projection = build_projection(&branch);
            let entry_id = entry_id.as_deref();
            let projected = entry_id.is_none_or(|id| {
                projection.entries.iter().any(|entry| {
                    entry.source.meta().is_some_and(|meta| meta.id == id)
                        && entry
                            .messages
                            .iter()
                            .any(|message| matches!(message, Message::Assistant(_)))
                })
            });
            let after: Vec<&FileEntry> = entry_id
                .and_then(|id| {
                    branch
                        .iter()
                        .position(|entry| entry.meta().is_some_and(|meta| meta.id == id))
                })
                .map(|index| branch[index + 1..].to_vec())
                .unwrap_or_default();
            let post_edit = after
                .iter()
                .any(|entry| matches!(entry, FileEntry::ContextEdit(_)));
            let latest_edit = after.iter().rev().find_map(|entry| match entry {
                FileEntry::ContextEdit(edit) if Some(edit.target_id.as_str()) == entry_id => {
                    Some(edit)
                }
                _ => None,
            });
            let retained = entry_id.is_none()
                || (!after
                    .iter()
                    .any(|entry| matches!(entry, FileEntry::Compaction(_)))
                    && latest_edit.is_none_or(|edit| edit.replacement.is_some()));
            let usage_matches = projected && !post_edit;
            let explicit = message.stop_reason == StopReason::Error && is_context_overflow(message, 0);
            let same_model = message_model.is_some();
            let overflow = same_model
                && ((explicit && retained)
                    || (usage_matches && is_context_overflow(message, context_window)));
            let length = message_model.as_ref().is_some_and(|model| {
                projected && is_recoverable_length(message, model.max_tokens)
            });
            if overflow || length {
                let will_retry = message.stop_reason != StopReason::Stop;
                if !will_retry {
                    return CompactionCheck::Overflow { will_retry: false };
                }
                if overflow_attempted {
                    return CompactionCheck::OverflowFailed(if overflow {
                        "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.".into()
                    } else {
                        "Truncated response recovery failed after one compact-and-retry attempt.".into()
                    });
                }
                return CompactionCheck::Overflow { will_retry: true };
            }
            let has_edits = projection
                .entries
                .iter()
                .any(|entry| matches!(entry.source, FileEntry::ContextEdit(_)));
            let direct = calculate_context_tokens(&message.usage);
            let tokens = if has_edits {
                estimate_projected_context_tokens(&projection, &branch).tokens
            } else if message.stop_reason == StopReason::Error || direct == 0 {
                let estimate = estimate_context_tokens(&projection.messages);
                if let Some(index) = estimate.last_usage_index
                    && let Message::Assistant(usage_message) = &projection.messages[index]
                    && compaction_time.is_some_and(|time| usage_message.timestamp <= time)
                {
                    return CompactionCheck::None;
                }
                estimate.tokens
            } else {
                direct
            };
            if should_compact(tokens, context_window, &settings) {
                CompactionCheck::Threshold
            } else {
                CompactionCheck::None
            }
        });
        match check {
            CompactionCheck::None => false,
            CompactionCheck::Threshold => {
                self.run_auto_compaction(CompactionReason::Threshold, false, cancel)
                    .await
            }
            CompactionCheck::OverflowFailed(error_message) => {
                self.emit_compaction_end(
                    CompactionReason::Overflow,
                    Err(Some(error_message)),
                    false,
                );
                false
            }
            CompactionCheck::Overflow { will_retry } => {
                if will_retry {
                    lock(&self.inner.recovery).overflow_recovery_attempted = true;
                    let ids: Vec<String> = entry_id.into_iter().chain(tool_results).collect();
                    self.omit_recovery_attempt(&ids);
                }
                self.run_auto_compaction(CompactionReason::Overflow, will_retry, cancel)
                    .await
            }
        }
    }

    /// A summarizer for `model` with the session's credentials and retry
    /// policy, reporting retries as `source` summaries.
    async fn summarizer<'a>(
        &'a self,
        model: &'a Model,
        thinking_level: ThinkingLevel,
        source: SummarySource,
        reason: Option<CompactionReason>,
        cancel: CancellationToken,
    ) -> Summarizer<'a> {
        Summarizer {
            model,
            apis: &self.inner.apis,
            auth: self.registry().auth(model).await,
            thinking_level,
            retry: self.retry_policy(),
            cancel,
            on_retry: Box::new(move |retry| self.emit_summary_retry(retry, source, reason)),
        }
    }

    /// What manual and automatic compaction share: summarizes `preparation`
    /// and records the compaction with the estimate after it, unless `cancel`
    /// fired meanwhile.
    async fn run_compaction(
        &self,
        model: &Model,
        preparation: &Preparation,
        custom_instructions: Option<&str>,
        reason: CompactionReason,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, String> {
        let summarizer = self
            .summarizer(
                model,
                self.thinking_level(),
                SummarySource::Compaction,
                Some(reason),
                cancel.clone(),
            )
            .await;
        let mut result = summarizer.compact(preparation, custom_instructions).await?;
        // As pi, a summary cut short by an abort is not recorded.
        if cancel.is_cancelled() {
            return Err("Compaction cancelled".to_owned());
        }
        let estimate = self.with_session(|session| {
            let _ = session.append_compaction(
                result.summary.clone(),
                Some(result.first_kept_entry_id.clone()),
                result.tokens_before,
                result.details.clone(),
                Some(false),
                result.usage.clone(),
            );
            session
                .build_context()
                .messages
                .iter()
                .map(estimate_tokens)
                .sum()
        });
        result.estimated_tokens_after = Some(estimate);
        Ok(result)
    }

    /// pi's `compaction_end` event: with the result, or aborted (`Err(None)`),
    /// or failed with an error message.
    fn emit_compaction_end(
        &self,
        reason: CompactionReason,
        outcome: Result<CompactionResult, Option<String>>,
        will_retry: bool,
    ) {
        let (result, aborted, error_message) = match outcome {
            Ok(result) => (Some(result), false, None),
            Err(None) => (None, true, None),
            Err(Some(error)) => (None, false, Some(error)),
        };
        self.emit(&AgentEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        });
    }

    /// pi's `summarization_retry_*` events for a summary request's retries.
    fn emit_summary_retry(
        &self,
        retry: SummaryRetry,
        source: SummarySource,
        reason: Option<CompactionReason>,
    ) {
        self.emit(&match retry {
            SummaryRetry::Scheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => AgentEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            },
            SummaryRetry::AttemptStart => {
                AgentEvent::SummarizationRetryAttemptStart { source, reason }
            }
            SummaryRetry::Finished => AgentEvent::SummarizationRetryFinished,
        });
    }

    async fn run_auto_compaction(
        &self,
        reason: CompactionReason,
        will_retry: bool,
        cancel: &CancellationToken,
    ) -> bool {
        let Some(model) = self.model() else {
            return false;
        };
        let settings =
            CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
        let Some(preparation) =
            self.with_session(|session| prepare_compaction(&session.branch_path(None), settings))
        else {
            return false;
        };
        self.emit(&AgentEvent::CompactionStart { reason });
        let compacting = Compacting::start(&self.inner.compacting);
        let outcome = self
            .run_compaction(&model, &preparation, None, reason, cancel)
            .await;
        drop(compacting);
        self.inner.idle.notify_waiters();
        match outcome {
            Ok(result) => {
                self.emit_compaction_end(reason, Ok(result), will_retry);
                will_retry || self.has_queued()
            }
            Err(message) => {
                let error_message = (!cancel.is_cancelled()).then(|| match reason {
                    CompactionReason::Overflow => {
                        format!("Context overflow recovery failed: {message}")
                    }
                    _ => format!("Auto-compaction failed: {message}"),
                });
                self.emit_compaction_end(reason, Err(error_message), false);
                false
            }
        }
    }

    /// Compacts the session now, optionally focused by `custom_instructions`.
    /// Aborts a running response first.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, String> {
        self.abort();
        self.wait_for_idle().await;
        let cancel = CancellationToken::new();
        *lock(&self.inner.cancel) = Some(cancel.clone());
        let compacting = Compacting::start(&self.inner.compacting);
        self.inner.manual_compaction.store(true, Ordering::SeqCst);
        self.emit(&AgentEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });
        let outcome = async {
            let model = self
                .model()
                .ok_or_else(crate::auth_guidance::no_model_selected)?;
            let settings =
                CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
            let preparation = self.with_session(|session| {
                let branch = session.branch_path(None);
                match prepare_compaction(&branch, settings) {
                    Some(preparation) => Ok(preparation),
                    None if matches!(branch.last(), Some(FileEntry::Compaction(_))) => {
                        Err("Already compacted".to_owned())
                    }
                    None => Err("Nothing to compact (session too small)".to_owned()),
                }
            })?;
            self.run_compaction(
                &model,
                &preparation,
                custom_instructions,
                CompactionReason::Manual,
                &cancel,
            )
            .await
        }
        .await;
        *lock(&self.inner.cancel) = None;
        self.inner.manual_compaction.store(false, Ordering::SeqCst);
        drop(compacting);
        // Waiters for idle run once this returns, after its own outcome.
        self.inner.idle.notify_waiters();
        let reason = CompactionReason::Manual;
        if cancel.is_cancelled() {
            self.emit_compaction_end(reason, Err(None), false);
            // pi reports the summarizer's error, such as an aborted request.
            return Err(outcome
                .err()
                .unwrap_or_else(|| "Compaction cancelled".into()));
        }
        match &outcome {
            Ok(result) => self.emit_compaction_end(reason, Ok(result.clone()), false),
            Err(message) => self.emit_compaction_end(
                reason,
                Err(Some(format!("Compaction failed: {message}"))),
                false,
            ),
        }
        outcome
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
        let (entries, _common) = self.with_session(|session| {
            crate::compaction::collect_branch_entries(session, old_leaf.as_deref(), target_id)
        });
        let mut summary = None;
        if options.summarize
            && !entries.is_empty()
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
                        serde_json::json!({"readFiles": read_files, "modifiedFiles": modified_files}),
                        usage,
                    ));
                }
            }
        }
        let (new_leaf, editor_text) = match &target {
            FileEntry::Message(entry) if matches!(entry.message, Message::User(_)) => {
                let text = match &entry.message {
                    Message::User(user) => match &user.content {
                        Content::Text(text) => text.clone(),
                        Content::Blocks(blocks) => yapi_types::message::blocks_text(blocks, ""),
                    },
                    _ => String::new(),
                };
                (entry.meta.parent_id.clone(), Some(text))
            }
            FileEntry::CustomMessage(entry) => {
                let text = match &entry.content {
                    Content::Text(text) => text.clone(),
                    Content::Blocks(blocks) => yapi_types::message::blocks_text(blocks, ""),
                };
                (entry.meta.parent_id.clone(), Some(text))
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
                            Some(details),
                            Some(false),
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

    /// The model registry; credentials it reads stay current with `auth.json`.
    pub fn registry(&self) -> Arc<ModelRegistry> {
        Arc::clone(&read(&self.inner.registry))
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

    /// Refreshes the model catalogs that change between releases: restores
    /// the stored ones and, when `options` allow the network, fetches those of
    /// configured providers, as pi's `ModelRuntime.refresh`. The session then
    /// lists the new models. Concurrent refreshes run one after another.
    pub async fn refresh_model_catalogs(
        &self,
        options: yapi_ai::model_catalog::RefreshOptions,
    ) -> CatalogRefresh {
        let _running = self.inner.catalog_refresh.lock().await;
        let registry = self.registry();
        let Some(store) = registry.models_store().cloned() else {
            return CatalogRefresh::default();
        };
        let targets = registry.catalog_targets().await;
        let refreshed = yapi_ai::model_catalog::refresh(&targets, &store, &options).await;
        // Apply to the registry current now; credentials may have changed.
        let mut slot = write(&self.inner.registry);
        let mut next = (**slot).clone();
        next.apply_catalogs(refreshed.models);
        *slot = Arc::new(next);
        drop(slot);
        CatalogRefresh {
            errors: refreshed.errors,
            aborted: refreshed.aborted,
        }
    }

    /// Model registry access; always `Some`.
    pub fn with_registry<T>(&self, f: impl FnOnce(&ModelRegistry) -> T) -> Option<T> {
        Some(f(&read(&self.inner.registry)))
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

    /// The context extensions get, with `cancel` for the operation at hand.
    fn extension_context(&self, cancel: CancellationToken) -> Context {
        let (ui, mode) = lock(&self.inner.binding).clone();
        Context {
            cwd: self.inner.cwd.clone(),
            agent_dir: self.inner.agent_dir.clone(),
            project_trusted: lock(&self.inner.settings).project_trusted(),
            mode,
            ui,
            tools: self.inner.tools.clone(),
            cancel,
            session: self.downgrade(),
        }
    }

    /// A handle that does not keep the session alive.
    pub fn downgrade(&self) -> WeakSession {
        WeakSession(Arc::downgrade(&self.inner))
    }

    /// The UI and mode extensions are bound to.
    pub fn extension_binding(&self) -> (Arc<dyn ExtensionUi>, Mode) {
        lock(&self.inner.binding).clone()
    }

    /// The extensions with handlers for pi events of type `kind`.
    fn handlers_of(&self, kind: &str) -> Vec<&Arc<dyn Extension>> {
        self.inner
            .extensions
            .iter()
            .filter(|extension| extension.handles(kind))
            .collect()
    }

    /// Whether any extension handles pi events of type `kind`.
    pub fn has_handlers(&self, kind: &str) -> bool {
        self.inner
            .extensions
            .iter()
            .any(|extension| extension.handles(kind))
    }

    /// Delivers pi event `event` to every extension that handles it, in
    /// order, for events whose results pi ignores.
    pub async fn emit_extension_event(&self, event: &Value, cancel: CancellationToken) {
        let kind = event["type"].as_str().unwrap_or_default();
        let handlers = self.handlers_of(kind);
        if handlers.is_empty() {
            return;
        }
        let ctx = self.extension_context(cancel);
        for extension in handlers {
            extension.handle(&ctx, event).await;
        }
    }

    /// pi's `input` event: `None` when a handler handled the input,
    /// otherwise the possibly transformed text and images.
    async fn input_handlers(
        &self,
        text: &str,
        images: Vec<ImageContent>,
        source: InputSource,
        streaming: Option<StreamingBehavior>,
    ) -> Option<(String, Vec<ImageContent>)> {
        let handlers = self.handlers_of("input");
        if handlers.is_empty() {
            return Some((text.to_owned(), images));
        }
        let ctx = self.extension_context(CancellationToken::new());
        let mut text = text.to_owned();
        let mut images = images;
        for extension in handlers {
            let mut event = serde_json::json!({
                "type": "input", "text": text, "images": images, "source": source.as_str(),
            });
            if let Some(behavior) = streaming {
                event["streamingBehavior"] = behavior_name(behavior).into();
            }
            let Some(result) = extension.handle(&ctx, &event).await else {
                continue;
            };
            match result["action"].as_str() {
                Some("handled") => return None,
                Some("transform") => {
                    if let Some(next) = result["text"].as_str() {
                        next.clone_into(&mut text);
                    }
                    if let Ok(next) = serde_json::from_value(result["images"].clone()) {
                        images = next;
                    }
                }
                _ => {}
            }
        }
        Some((text, images))
    }

    /// pi's `before_agent_start` event for extensions handling it: the
    /// custom messages to send with the prompt, and the system prompt a
    /// handler forced, if any.
    async fn before_agent_start_handlers(
        &self,
        prompt: &str,
        images: &[ImageContent],
        system_prompt: &str,
    ) -> (Vec<Message>, Option<String>) {
        let handlers = self.handlers_of("before_agent_start");
        let mut messages = Vec::new();
        let mut forced: Option<String> = None;
        if handlers.is_empty() {
            return (messages, forced);
        }
        let ctx = self.extension_context(CancellationToken::new());
        for extension in handlers {
            let event = serde_json::json!({
                "type": "before_agent_start",
                "prompt": prompt,
                "images": images,
                "systemPrompt": forced.as_deref().unwrap_or(system_prompt),
                "systemPromptOptions": {},
            });
            let Some(result) = extension.handle(&ctx, &event).await else {
                continue;
            };
            for message in result["messages"].as_array().into_iter().flatten() {
                let content = match &message["content"] {
                    Value::Null => Value::Array(Vec::new()),
                    other => other.clone(),
                };
                messages.push(Message::Custom(yapi_types::message::CustomMessage {
                    custom_type: message["customType"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    content: serde_json::from_value(content).unwrap_or(Content::Blocks(Vec::new())),
                    display: message["display"].as_bool().unwrap_or(false),
                    details: (!message["details"].is_null()).then(|| message["details"].clone()),
                    timestamp: now_ms(),
                }));
            }
            if let Some(prompt) = result["systemPrompt"].as_str() {
                forced = Some(prompt.to_owned());
            }
        }
        (messages, forced)
    }

    /// Gives extensions their UI and mode and starts them: pi's
    /// `bindExtensions`, which emits `session_start`.
    pub async fn bind_extensions(&self, ui: Arc<dyn ExtensionUi>, mode: Mode) {
        *lock(&self.inner.binding) = (ui, mode);
        let ctx = self.extension_context(CancellationToken::new());
        for extension in &self.inner.extensions {
            extension.session_start(&ctx).await;
        }
        let event = serde_json::json!({"type": "session_start", "reason": "startup"});
        self.emit_extension_event(&event, CancellationToken::new())
            .await;
    }

    /// Stops extensions before the session ends or is replaced.
    pub async fn shutdown(&self) {
        let event = serde_json::json!({"type": "session_shutdown"});
        self.emit_extension_event(&event, CancellationToken::new())
            .await;
        let ctx = self.extension_context(CancellationToken::new());
        for extension in &self.inner.extensions {
            extension.session_shutdown(&ctx).await;
        }
    }

    /// The session's extensions.
    pub fn extensions(&self) -> &[Arc<dyn Extension>] {
        &self.inner.extensions
    }

    /// The extension that draws tool `name`, and how.
    pub fn tool_renderer(&self, name: &str) -> Option<(Arc<dyn Extension>, ToolRenderers)> {
        self.inner.extensions.iter().find_map(|extension| {
            let renderers = extension.renderers().tools.get(name).copied()?;
            Some((extension.clone(), renderers))
        })
    }

    /// The extension that draws custom messages of type `custom_type`.
    pub fn message_renderer(&self, custom_type: &str) -> Option<Arc<dyn Extension>> {
        self.inner
            .extensions
            .iter()
            .find(|extension| {
                extension
                    .renderers()
                    .messages
                    .iter()
                    .any(|kind| kind == custom_type)
            })
            .cloned()
    }

    /// The shortcuts the session's extensions bind, given the keys of each
    /// built-in action, and pi's warnings about conflicts.
    pub fn extension_shortcuts(
        &self,
        builtin: &[(String, Vec<String>)],
    ) -> (
        Vec<crate::extensions::ShortcutBinding>,
        Vec<(String, String)>,
    ) {
        crate::extensions::resolve_shortcuts(&self.inner.extensions, builtin)
    }

    /// Runs an extension shortcut's handler; the error is the handler's.
    pub async fn run_shortcut(
        &self,
        binding: &crate::extensions::ShortcutBinding,
    ) -> Result<(), String> {
        let ctx = self.extension_context(CancellationToken::new());
        binding
            .extension
            .run_shortcut(&binding.registered, &ctx)
            .await
    }

    /// The extensions' commands in load order, each with the name it is
    /// invoked by; pi's `resolveRegisteredCommands`. A name registered more
    /// than once is invoked as `name:1`, `name:2` and so on.
    pub fn extension_commands(&self) -> Vec<crate::extensions::ResolvedCommand> {
        let all: Vec<(Arc<dyn Extension>, crate::extensions::Command)> = self
            .inner
            .extensions
            .iter()
            .flat_map(|extension| {
                extension
                    .commands()
                    .into_iter()
                    .map(move |command| (Arc::clone(extension), command))
            })
            .collect();
        let count = |name: &str| {
            all.iter()
                .filter(|(_, command)| command.name == name)
                .count()
        };
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        all.iter()
            .map(|(extension, command)| {
                let occurrence = {
                    let entry = seen.entry(command.name.clone()).or_default();
                    *entry += 1;
                    *entry
                };
                let mut invocation = if count(&command.name) > 1 {
                    format!("{}:{occurrence}", command.name)
                } else {
                    command.name.clone()
                };
                let mut suffix = occurrence;
                while taken.contains(&invocation) {
                    suffix += 1;
                    invocation = format!("{}:{suffix}", command.name);
                }
                taken.insert(invocation.clone());
                crate::extensions::ResolvedCommand {
                    invocation,
                    command: command.clone(),
                    extension: Arc::clone(extension),
                }
            })
            .collect()
    }

    /// Whether `text` invokes an extension command.
    pub fn is_extension_command(&self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let name = rest.split(' ').next().unwrap_or_default();
        self.extension_commands()
            .iter()
            .any(|command| command.invocation == name)
    }

    /// Runs `/name args` when an extension command is invoked by `name`;
    /// whether it did. Errors are the extension's to report.
    async fn run_extension_command(&self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let Some(resolved) = self
            .extension_commands()
            .into_iter()
            .find(|command| command.invocation == name)
        else {
            return false;
        };
        let ctx = self.extension_context(CancellationToken::new());
        resolved
            .extension
            .run_command(&resolved.command.name, args, &ctx)
            .await;
        true
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

struct Hooks {
    session: AgentSession,
    steering_mode: Option<QueueMode>,
    follow_up_mode: Option<QueueMode>,
    cancel: CancellationToken,
    /// pi's turn index for extension events: turns since `agent_start`.
    turn_index: std::sync::atomic::AtomicU64,
}

/// The pi extension event type a loop event is delivered as, if any.
fn extension_event_kind(event: &AgentEvent) -> Option<&'static str> {
    Some(match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        _ => return None,
    })
}

/// A loop event as pi's extension event of type `kind`
/// (`_emitExtensionEvent` in pi's agent session).
fn extension_event(event: &AgentEvent, kind: &str, turn_index: u64) -> Option<Value> {
    let value = serde_json::to_value(event).ok()?;
    let keys: &[&str] = match kind {
        "agent_end" => &["messages"],
        "turn_end" => &["message", "toolResults"],
        "message_start" | "message_end" => &["message"],
        "message_update" => &["message", "assistantMessageEvent"],
        "tool_execution_start" => &["toolCallId", "toolName", "args", "parentToolCallId"],
        "tool_execution_update" => &[
            "toolCallId",
            "toolName",
            "args",
            "partialResult",
            "parentToolCallId",
        ],
        "tool_execution_end" => &[
            "toolCallId",
            "toolName",
            "result",
            "isError",
            "parentToolCallId",
        ],
        _ => &[],
    };
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String(kind.to_owned()));
    if matches!(kind, "turn_start" | "turn_end") {
        out.insert("turnIndex".into(), Value::from(turn_index));
    }
    if kind == "turn_start" {
        out.insert("timestamp".into(), Value::from(now_ms()));
    }
    for key in keys {
        if let Some(field) = value.get(*key) {
            out.insert((*key).to_owned(), field.clone());
        }
    }
    Some(Value::Object(out))
}

fn drain(queue: &Mutex<VecDeque<Message>>, mode: Option<QueueMode>) -> Vec<Message> {
    let mut queue = lock(queue);
    match mode {
        Some(QueueMode::All) => queue.drain(..).collect(),
        _ => queue.pop_front().into_iter().collect(),
    }
}

impl AgentHooks for Hooks {
    fn on_event<'a>(&'a self, event: &'a AgentEvent) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let session = &self.session;
            if let AgentEvent::MessageStart { message } = event {
                session.dequeue_started(message);
            }
            // pi's interactive mode rewrites an aborted response's error before
            // the session records it.
            let rewritten = match event {
                AgentEvent::MessageEnd {
                    message: Message::Assistant(assistant),
                } if assistant.stop_reason == StopReason::Aborted
                    && lock(&session.inner.binding).1 == Mode::Tui =>
                {
                    let attempt = lock(&session.inner.recovery).retry_attempt;
                    let mut assistant = assistant.clone();
                    assistant.error_message = Some(if attempt > 0 {
                        let plural = if attempt > 1 { "s" } else { "" };
                        format!("Aborted after {attempt} retry attempt{plural}")
                    } else {
                        "Operation aborted".to_owned()
                    });
                    Some(AgentEvent::MessageEnd {
                        message: Message::Assistant(assistant),
                    })
                }
                _ => None,
            };
            let event = rewritten.as_ref().unwrap_or(event);
            let kind = extension_event_kind(event).filter(|kind| session.has_handlers(kind));
            if matches!(event, AgentEvent::AgentStart) {
                self.turn_index.store(0, Ordering::SeqCst);
            }
            let turn_index = self.turn_index.load(Ordering::SeqCst);
            if matches!(event, AgentEvent::TurnEnd { .. }) {
                self.turn_index.fetch_add(1, Ordering::SeqCst);
            }
            // As in pi, extensions see the event first, then listeners; then the
            // session records it, so entries extensions append come before it.
            if let Some(extension_event) =
                kind.and_then(|kind| extension_event(event, kind, turn_index))
            {
                session
                    .emit_extension_event(&extension_event, self.cancel.clone())
                    .await;
            }
            match event {
                AgentEvent::AgentEnd { messages, .. } => {
                    session.inner.nested.clear();
                    session.emit(&AgentEvent::AgentEnd {
                        messages: messages.clone(),
                        will_retry: session.will_retry_after(messages),
                    });
                }
                _ => session.emit(event),
            }
            match event {
                AgentEvent::MessageEnd { message } => {
                    let entry_id = session.with_session(|file| match message {
                        Message::Custom(custom) => file
                            .append_custom_message(
                                &custom.custom_type,
                                custom.content.clone(),
                                custom.display,
                                custom.details.clone(),
                            )
                            .ok(),
                        Message::System(_)
                        | Message::User(_)
                        | Message::Assistant(_)
                        | Message::ToolResult(_) => file.append_message(message.clone()).ok(),
                        _ => None,
                    });
                    match message {
                        Message::Assistant(assistant) => {
                            let finished_retry = {
                                let mut recovery = lock(&session.inner.recovery);
                                recovery.last_assistant = Some(((**assistant).clone(), entry_id));
                                recovery.turn_tool_results.clear();
                                if !matches!(
                                    assistant.stop_reason,
                                    StopReason::Error | StopReason::Length
                                ) {
                                    recovery.overflow_recovery_attempted = false;
                                }
                                if assistant.stop_reason != StopReason::Error {
                                    std::mem::take(&mut recovery.retry_attempt)
                                } else {
                                    0
                                }
                            };
                            if finished_retry > 0 {
                                session.emit(&AgentEvent::AutoRetryEnd {
                                    success: true,
                                    attempt: finished_retry,
                                    final_error: None,
                                });
                            }
                        }
                        Message::ToolResult(_) => {
                            if let Some(id) = entry_id {
                                lock(&session.inner.recovery).turn_tool_results.push(id);
                            }
                        }
                        _ => {}
                    }
                }
                AgentEvent::TurnEnd { .. } => {
                    let mut recovery = lock(&session.inner.recovery);
                    recovery.last_tool_results = std::mem::take(&mut recovery.turn_tool_results);
                }
                _ => {}
            }
            // Custom messages sent during the turn are appended once its handlers
            // ran, so those that turn_end handlers send join them, as in pi.
            if matches!(event, AgentEvent::TurnEnd { .. }) {
                session.flush_pending_custom();
            }
        })
    }

    fn transform_context(&self, messages: Vec<Message>) -> BoxFuture<'_, Vec<Message>> {
        Box::pin(async move {
            let mut messages = messages;
            let session = &self.session;
            let handlers = session.handlers_of("context");
            if !handlers.is_empty() {
                let ctx = session.extension_context(self.cancel.clone());
                for extension in handlers {
                    let event = serde_json::json!({"type": "context", "messages": messages});
                    if let Some(result) = extension.handle(&ctx, &event).await
                        && let Ok(next) = serde_json::from_value(result["messages"].clone())
                    {
                        messages = next;
                    }
                }
            }
            let forced = lock(&session.inner.forced_prompt).clone();
            if let Some(forced) = forced {
                // pi's forced prompt projection: one system message with the
                // forced text replaces the transcript's system messages.
                let current = yapi_ai::transcript::current_system_message(&messages);
                let head = Message::System(SystemMessage {
                    content: Content::Text(forced),
                    sections: None,
                    timestamp: current
                        .as_ref()
                        .map_or_else(now_ms, |system| system.timestamp),
                    tools_added: current.and_then(|system| system.tools_added),
                    tools_removed: None,
                });
                messages.retain(|message| !matches!(message, Message::System(_)));
                messages.insert(0, head);
            }
            messages
        })
    }

    fn convert_to_llm(&self, messages: Vec<Message>) -> Vec<Message> {
        let messages = convert_to_llm(messages);
        // Read on every request, so a change applies mid-session.
        let blocked = self
            .session
            .settings()
            .images
            .and_then(|images| images.block_images)
            == Some(true);
        if blocked {
            crate::messages::block_images(messages)
        } else {
            messages
        }
    }

    fn auth<'a>(&'a self, model: &'a Model) -> BoxFuture<'a, Auth> {
        // The registry is read under a short lock; credential commands run outside it.
        let registry = self.session.registry();
        Box::pin(async move { registry.auth(model).await })
    }

    fn current_tools(&self) -> Option<Vec<Arc<dyn Tool>>> {
        let declared = self
            .session
            .inner
            .tools
            .with(|registry| registry.declared());
        Some(self.session.loadout(declared))
    }

    fn before_tool_call<'a>(
        &'a self,
        call: yapi_agent::hooks::BeforeToolCall<'a>,
    ) -> BoxFuture<'a, Option<yapi_agent::hooks::Block>> {
        Box::pin(async move {
            let ctx = self.session.extension_context(self.cancel.clone());
            for extension in &self.session.inner.extensions {
                if let Some(reason) = extension
                    .tool_call(&ctx, &call.tool_call.name, call.args)
                    .await
                {
                    return Some(yapi_agent::hooks::Block {
                        reason: Some(reason),
                        terminate: false,
                    });
                }
            }
            let mut event = serde_json::json!({
                "type": "tool_call",
                "toolName": call.tool_call.name,
                "toolCallId": call.tool_call.id,
            });
            if let Some(parent) = call.parent_tool_call_id {
                event["parentToolCallId"] = Value::String(parent.to_owned());
            }
            event["input"] = call.args.clone();
            for extension in self.session.handlers_of("tool_call") {
                if let Some(result) = extension.handle(&ctx, &event).await
                    && result["block"] == true
                {
                    return Some(yapi_agent::hooks::Block {
                        reason: result["reason"].as_str().map(str::to_owned),
                        terminate: false,
                    });
                }
            }
            None
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        call: yapi_agent::hooks::AfterToolCall<'a>,
    ) -> BoxFuture<'a, Option<yapi_agent::hooks::ResultPatch>> {
        Box::pin(async move {
            let handlers = self.session.handlers_of("tool_result");
            if handlers.is_empty() {
                return None;
            }
            let ctx = self.session.extension_context(self.cancel.clone());
            let mut event = serde_json::json!({
                "type": "tool_result",
                "toolName": call.tool_call.name,
                "toolCallId": call.tool_call.id,
            });
            if let Some(parent) = call.parent_tool_call_id {
                event["parentToolCallId"] = Value::String(parent.to_owned());
            }
            event["input"] = call.args.clone();
            event["content"] = serde_json::json!(call.result.content);
            event["details"] = serde_json::json!(call.result.details);
            event["isError"] = Value::Bool(call.is_error);
            let mut modified = false;
            for extension in handlers {
                let Some(result) = extension.handle(&ctx, &event).await else {
                    continue;
                };
                for key in ["content", "details", "isError"] {
                    if let Some(value) = result.get(key).filter(|value| !value.is_null()) {
                        event[key] = value.clone();
                        modified = true;
                    }
                }
            }
            modified.then(|| yapi_agent::hooks::ResultPatch {
                content: serde_json::from_value(event["content"].clone()).ok(),
                details: Some(event["details"].clone()).filter(|details| !details.is_null()),
                is_error: event["isError"].as_bool(),
                terminate: None,
            })
        })
    }

    fn complete_tool_result(&self, message: &mut ToolResultMessage) {
        let Some(summary) = self.session.inner.nested.take(&message.tool_call_id) else {
            return;
        };
        if summary.calls.is_some() {
            message.nested_calls = summary.calls;
        }
        if let Some(usage) = summary.usage {
            message.usage = Some(match &message.usage {
                Some(own) => own.combine(&usage),
                None => usage,
            });
        }
    }

    fn steering_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        let messages = drain(&self.session.inner.steering, self.steering_mode);
        Box::pin(async move { messages })
    }

    fn follow_up_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        let messages = drain(&self.session.inner.follow_up, self.follow_up_mode);
        Box::pin(async move { messages })
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
        assert_eq!(
            session.with_registry(|registry| registry.has_auth("p")),
            Some(true)
        );
    }
}
