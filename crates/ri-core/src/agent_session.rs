//! A conversation: the agent loop bound to a session file, tools, the system prompt
//! and the model registry.
//!
//! Port of the core of `packages/coding-agent/src/core/agent-session.ts` in pi
//! `v1.0.0`: prompts with template expansion, the system prompt as transcript
//! messages, persistence of every message, queues, abort and model selection.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;
use ri_agent::hooks::AgentHooks;
use ri_agent::{AgentContext, ExecutionMode, LoopConfig, Tool};
use ri_ai::api::Apis;
use ri_ai::errors::{is_context_overflow, is_recoverable_length, is_retryable_assistant_error};
use ri_ai::registry::{Auth, ModelRegistry};
use ri_ai::stream::{StreamOptions, ThinkingBudgets};
use ri_types::event::AgentEvent;
use ri_types::event::{CompactionReason, CompactionResult};
use ri_types::message::{
    AssistantMessage, Content, ContentBlock, ImageContent, Message, StopReason, SystemMessage,
    TextContent, ThinkingLevel, UserMessage,
};
use ri_types::model::Model;
use ri_types::session::FileEntry;
use ri_types::settings::QueueMode;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    BranchSummary, CompactionSettings, Preparation, RetryPolicy, Summarizer,
    calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
    estimate_tokens, prepare_compaction, should_compact,
};
use crate::messages::convert_to_llm;
use crate::resources::{ContextFile, PromptTemplate, Skill, expand_prompt_template};
use crate::session::{SessionManager, build_projection};
use crate::settings::SettingsManager;
use crate::system_prompt::{PromptOptions, build_sections, diff_sections};
use crate::time::{now_ms, parse_iso};
use crate::tools::{BUILTIN_TOOLS, PromptTool, Runtime, ToolEnv, builtin};

/// Receives every session event.
pub type Listener = Box<dyn Fn(&AgentEvent) + Send + Sync>;

/// Discovered resources the prompt uses.
#[derive(Clone, Debug, Default)]
pub struct Resources {
    /// `AGENTS.md` and `CLAUDE.md` files.
    pub context_files: Vec<ContextFile>,
    /// Skills.
    pub skills: Vec<Skill>,
    /// Prompt templates.
    pub templates: Vec<PromptTemplate>,
    /// Replaces the default prompt (`SYSTEM.md`, `--system-prompt`).
    pub custom_prompt: Option<String>,
    /// Appended to the prompt (`APPEND_SYSTEM.md`, `--append-system-prompt`).
    pub append_prompt: Option<String>,
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
    /// Active built-in tool names.
    pub tools: Vec<String>,
    /// Prompt resources.
    pub resources: Resources,
}

struct State {
    session: SessionManager,
    model: Option<Model>,
    thinking_level: ThinkingLevel,
    active_tools: Vec<String>,
    streaming: bool,
}

struct Inner {
    cwd: PathBuf,
    settings: Mutex<SettingsManager>,
    registry: RwLock<Arc<ModelRegistry>>,
    apis: Apis,
    state: Mutex<State>,
    tools: Vec<PromptTool>,
    resources: Resources,
    runtime: Arc<RwLock<Runtime>>,
    listeners: Mutex<Vec<Listener>>,
    steering: Mutex<VecDeque<Message>>,
    follow_up: Mutex<VecDeque<Message>>,
    cancel: Mutex<Option<CancellationToken>>,
    recovery: Mutex<Recovery>,
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

/// What the compaction check decided.
enum CompactionCheck {
    None,
    Overflow { will_retry: bool },
    OverflowFailed(String),
    Threshold,
}

/// A running conversation. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct AgentSession {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// pi's message when no model is selected.
pub const NO_MODEL_MESSAGE: &str = "No model selected.\n\nSet an API key environment variable (e.g. ANTHROPIC_API_KEY) or use --model to select a model.";

/// pi's message when a provider has no credential.
pub fn no_api_key_message(provider: &str) -> String {
    format!(
        "No API key found for {provider}.\n\nUse /login or set an API key environment variable. See docs/providers.md for details."
    )
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
            resources,
        } = config;
        let runtime = Arc::new(RwLock::new(Runtime::default()));
        let env = ToolEnv {
            cwd: cwd.clone(),
            runtime: runtime.clone(),
            bin_dir: crate::config::bin_dir(&agent_dir),
        };
        let available: Vec<PromptTool> = BUILTIN_TOOLS
            .iter()
            .filter_map(|name| builtin(name, &env))
            .collect();

        let has_entries = session.entries().next().is_some();
        if has_entries {
            let has_thinking = session
                .entries()
                .any(|entry| matches!(entry, ri_types::session::FileEntry::ThinkingLevelChange(_)));
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
                apis,
                state: Mutex::new(State {
                    session,
                    model,
                    thinking_level,
                    active_tools: tools,
                    streaming: false,
                }),
                tools: available,
                resources,
                runtime,
                listeners: Mutex::new(Vec::new()),
                steering: Mutex::new(VecDeque::new()),
                follow_up: Mutex::new(VecDeque::new()),
                cancel: Mutex::new(None),
                recovery: Mutex::new(Recovery::default()),
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
    pub fn last_assistant(&self) -> Option<ri_types::message::AssistantMessage> {
        self.messages()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(*assistant),
                _ => None,
            })
    }

    /// Switches the model and records it.
    pub fn set_model(&self, model: Model) {
        let mut state = lock(&self.inner.state);
        let _ = state
            .session
            .append_model_change(&model.provider, &model.id);
        let level = ri_ai::thinking::clamp_level(&model, state.thinking_level);
        if level != state.thinking_level {
            state.thinking_level = level;
            let _ = state.session.append_thinking_level_change(level.as_str());
        }
        state.model = Some(model);
    }

    /// Sets the thinking level, clamped to what the model supports, and records it.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let mut state = lock(&self.inner.state);
        let level = match &state.model {
            Some(model) => ri_ai::thinking::clamp_level(model, level),
            None => level,
        };
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
    pub fn settings(&self) -> ri_types::settings::Settings {
        lock(&self.inner.settings).settings().clone()
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

    /// Empties the steering and follow-up queues and returns their texts,
    /// steering first.
    pub fn clear_queues(&self) -> Vec<String> {
        let text = |message: Message| match message {
            Message::User(user) => match user.content {
                Content::Text(text) => text,
                Content::Blocks(blocks) => blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            },
            _ => String::new(),
        };
        let mut texts: Vec<String> = lock(&self.inner.steering).drain(..).map(text).collect();
        texts.extend(lock(&self.inner.follow_up).drain(..).map(text));
        self.emit_queue_update();
        texts
    }

    /// The thinking levels the current model supports; all of them without a
    /// model.
    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            Some(model) => ri_ai::thinking::supported_levels(&model),
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
        let levels = ri_ai::thinking::supported_levels(&model);
        let current = self.thinking_level();
        let index = levels.iter().position(|level| *level == current);
        let next = levels[index.map_or(0, |index| (index + 1) % levels.len())];
        self.set_thinking_level(next);
        Some(self.thinking_level())
    }

    /// Models with credentials, in catalog order.
    pub fn available_models(&self) -> Vec<Model> {
        let registry = self
            .inner
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .models()
            .iter()
            .filter(|model| registry.has_auth(&model.provider))
            .cloned()
            .collect()
    }

    /// Switches to `model`, taking the thinking level from settings for that
    /// model, else the default, else the current one, as pi's `cycleModel`.
    pub fn switch_model(&self, model: Model) {
        let settings = self.settings();
        let level = settings
            .model_thinking_levels
            .as_ref()
            .and_then(|levels| levels.get(&model.reference()))
            .copied()
            .or(settings.default_thinking_level)
            .unwrap_or_else(|| self.thinking_level());
        self.set_model(model);
        self.set_thinking_level(level);
    }

    /// Cancels the current run.
    pub fn abort(&self) {
        if let Some(cancel) = lock(&self.inner.cancel).as_ref() {
            cancel.cancel();
        }
    }

    fn user_message(text: String, images: Vec<ImageContent>) -> Message {
        let mut content = vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })];
        content.extend(images.into_iter().map(ContentBlock::Image));
        Message::User(UserMessage {
            content: Content::Blocks(content),
            timestamp: now_ms(),
        })
    }

    fn emit_queue_update(&self) {
        let text = |queue: &VecDeque<Message>| {
            queue
                .iter()
                .map(|message| match message {
                    Message::User(user) => user.content.text("\n"),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };
        let steering = text(&lock(&self.inner.steering));
        let follow_up = text(&lock(&self.inner.follow_up));
        self.emit(&AgentEvent::QueueUpdate {
            steering,
            follow_up,
        });
    }

    /// Queues a message to steer the current run after its current tool calls.
    pub fn steer(&self, text: &str, images: Vec<ImageContent>) {
        let text = self.expand(text);
        lock(&self.inner.steering).push_back(Self::user_message(text, images));
        self.emit_queue_update();
    }

    /// Queues a message for when the current run would otherwise end.
    pub fn follow_up(&self, text: &str, images: Vec<ImageContent>) {
        let text = self.expand(text);
        lock(&self.inner.follow_up).push_back(Self::user_message(text, images));
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
        let (_, body) = crate::resources::parse_frontmatter(&content);
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

    fn active_tools(&self, active: &[String]) -> Vec<PromptTool> {
        active
            .iter()
            .filter_map(|name| {
                self.inner
                    .tools
                    .iter()
                    .find(|tool| &tool.tool.declaration().name == name)
                    .cloned()
            })
            .collect()
    }

    /// The system message patch that brings the transcript's prompt up to date.
    fn system_update(
        &self,
        messages: &[Message],
        active: &[String],
    ) -> Result<Option<Message>, String> {
        let sections = build_sections(&self.prompt_options(active))?;
        let current = ri_ai::transcript::current_system_message(messages)
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

    /// Sends a user prompt and runs until the agent settles. Templates and
    /// `/skill:` commands expand first.
    pub async fn prompt(&self, text: &str, images: Vec<ImageContent>) -> Result<(), String> {
        let expanded = self.expand(text);
        let (model, active, messages) = {
            let state = lock(&self.inner.state);
            if state.streaming {
                return Err("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".into());
            }
            let model = state.model.clone().ok_or(NO_MODEL_MESSAGE)?;
            (
                model,
                state.active_tools.clone(),
                state.session.build_context().messages,
            )
        };
        let has_auth = self
            .inner
            .registry
            .read()
            .map(|registry| registry.has_auth(&model.provider))
            .unwrap_or(false);
        if !has_auth {
            return Err(no_api_key_message(&model.provider));
        }
        let mut prompts = Vec::new();
        if let Some(update) = self.system_update(&messages, &active)? {
            prompts.push(update);
        }
        prompts.push(Self::user_message(expanded, images));
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
        *lock(&self.inner.cancel) = None;
        self.emit(&AgentEvent::AgentSettled);
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
        let (model, thinking_level, active, messages, session_id, session_file) = {
            let state = lock(&self.inner.state);
            (
                state.model.clone(),
                state.thinking_level,
                state.active_tools.clone(),
                state.session.build_context().messages,
                state.session.id().to_owned(),
                state.session.file().map(Path::to_path_buf),
            )
        };
        let Some(model) = model else {
            return;
        };
        if let Ok(mut runtime) = self.inner.runtime.write() {
            *runtime = Runtime {
                model: Some(model.clone()),
                thinking_level: Some(thinking_level),
                session_id: Some(session_id.clone()),
                session_file,
            };
        }
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
            tools: self
                .active_tools(&active)
                .into_iter()
                .map(|tool| tool.tool)
                .collect::<Vec<Arc<dyn Tool>>>(),
        };
        let hooks = Hooks {
            session: self.clone(),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
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
                self.emit_queue_update();
                Some(queued)
            }
            None => None,
        };
        match prompts {
            Some(prompts) => {
                ri_agent::run(prompts, &mut context, config, &hooks).await;
            }
            None => {
                let _ = ri_agent::run_continue(&mut context, config, &hooks).await;
            }
        }
    }

    /// The model whose limits apply to a response: the current one when it made it.
    fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        self.model()
            .filter(|model| model.provider == message.provider && model.id == message.model)
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
        tokio::select! {
            () = cancel.cancelled() => {
                self.finish_cancelled_retry();
                false
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
        }
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
                self.emit(&AgentEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(error_message),
                });
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

    async fn summarize(
        &self,
        model: &Model,
        preparation: &Preparation,
        custom_instructions: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, String> {
        let auth = self.registry().auth(model).await;
        let summarizer = Summarizer {
            model,
            apis: &self.inner.apis,
            auth: &auth,
            thinking_level: self.thinking_level(),
            session_id: None,
            retry: self.retry_policy(),
            cancel: cancel.clone(),
        };
        summarizer.compact(preparation, custom_instructions).await
    }

    /// Records a compaction result and fills in the estimate after it.
    fn record_compaction(&self, mut result: CompactionResult) -> CompactionResult {
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
        result
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
        let outcome = self.summarize(&model, &preparation, None, cancel).await;
        match outcome {
            Ok(result) if !cancel.is_cancelled() => {
                let result = self.record_compaction(result);
                self.emit(&AgentEvent::CompactionEnd {
                    reason,
                    result: Some(result),
                    aborted: false,
                    will_retry,
                    error_message: None,
                });
                will_retry || self.has_queued()
            }
            outcome => {
                let aborted = cancel.is_cancelled();
                let message = outcome
                    .err()
                    .unwrap_or_else(|| "Compaction cancelled".into());
                let error_message = (!aborted).then(|| match reason {
                    CompactionReason::Overflow => {
                        format!("Context overflow recovery failed: {message}")
                    }
                    _ => format!("Auto-compaction failed: {message}"),
                });
                self.emit(&AgentEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message,
                });
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
        let cancel = CancellationToken::new();
        self.emit(&AgentEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });
        let outcome = async {
            let model = self.model().ok_or(NO_MODEL_MESSAGE)?;
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
            let result = self
                .summarize(&model, &preparation, custom_instructions, &cancel)
                .await?;
            Ok(self.record_compaction(result))
        }
        .await;
        match outcome {
            Ok(result) => {
                self.emit(&AgentEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: Some(result.clone()),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                Ok(result)
            }
            Err(message) => {
                self.emit(&AgentEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(format!("Compaction failed: {message}")),
                });
                Err(message)
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
            let auth = self.registry().auth(model).await;
            let reserve = lock(&self.inner.settings)
                .settings()
                .branch_summary
                .as_ref()
                .and_then(|settings| settings.reserve_tokens)
                .unwrap_or(16384);
            let summarizer = Summarizer {
                model,
                apis: &self.inner.apis,
                auth: &auth,
                thinking_level: ThinkingLevel::Off,
                session_id: None,
                retry: self.retry_policy(),
                cancel,
            };
            let result = summarizer
                .branch_summary(
                    &entries,
                    options.custom_instructions.as_deref(),
                    options.replace_instructions,
                    reserve,
                )
                .await;
            *lock(&self.inner.cancel) = None;
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
                        Content::Blocks(blocks) => ri_types::message::blocks_text(blocks, ""),
                    },
                    _ => String::new(),
                };
                (entry.meta.parent_id.clone(), Some(text))
            }
            FileEntry::CustomMessage(entry) => {
                let text = match &entry.content {
                    Content::Text(text) => text.clone(),
                    Content::Blocks(blocks) => ri_types::message::blocks_text(blocks, ""),
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
        let Some(system) = ri_ai::transcript::current_system_message(&messages) else {
            return;
        };
        let names: Vec<String> = system
            .tools_added
            .iter()
            .flatten()
            .map(|tool| tool.name.clone())
            .filter(|name| {
                self.inner
                    .tools
                    .iter()
                    .any(|tool| tool.tool.declaration().name == *name)
            })
            .collect();
        lock(&self.inner.state).active_tools = names;
    }

    fn registry(&self) -> Arc<ModelRegistry> {
        self.inner
            .registry
            .read()
            .map(|registry| Arc::clone(&registry))
            .unwrap_or_default()
    }

    /// The session header line as JSON mode prints it.
    pub fn header_json(&self) -> Option<String> {
        self.with_session(|session| {
            session.header().and_then(|header| {
                ri_types::json::to_string(&ri_types::session::FileEntry::Session(header.clone()))
                    .ok()
            })
        })
    }

    /// Model registry access.
    pub fn with_registry<T>(&self, f: impl FnOnce(&ModelRegistry) -> T) -> Option<T> {
        self.inner.registry.read().ok().map(|registry| f(&registry))
    }

    /// Sets the active built-in tools.
    pub fn set_active_tools(&self, names: Vec<String>) {
        lock(&self.inner.state).active_tools = names;
    }

    /// The active tool names.
    pub fn active_tool_names(&self) -> Vec<String> {
        lock(&self.inner.state).active_tools.clone()
    }

    /// The working directory.
    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }
}

/// Text of an assistant message's text blocks, as print mode prints it.
pub fn assistant_text(message: &ri_types::message::AssistantMessage) -> String {
    ri_types::message::blocks_text(&message.content, "")
}

/// Whether an assistant message ended in failure.
pub fn failed(message: &ri_types::message::AssistantMessage) -> bool {
    matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
}

struct Hooks {
    session: AgentSession,
    steering_mode: Option<QueueMode>,
    follow_up_mode: Option<QueueMode>,
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
        let session = &self.session;
        match event {
            AgentEvent::AgentEnd { messages, .. } => {
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
        Box::pin(async {})
    }

    fn convert_to_llm(&self, messages: Vec<Message>) -> Vec<Message> {
        convert_to_llm(messages)
    }

    fn auth<'a>(&'a self, model: &'a Model) -> BoxFuture<'a, Auth> {
        // The registry is read under a short lock; credential commands run outside it.
        let registry = self.session.registry();
        Box::pin(async move { registry.auth(model).await })
    }

    fn steering_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        let messages = drain(&self.session.inner.steering, self.steering_mode);
        if !messages.is_empty() {
            self.session.emit_queue_update();
        }
        Box::pin(async move { messages })
    }

    fn follow_up_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        let messages = drain(&self.session.inner.follow_up, self.follow_up_mode);
        if !messages.is_empty() {
            self.session.emit_queue_update();
        }
        Box::pin(async move { messages })
    }
}
