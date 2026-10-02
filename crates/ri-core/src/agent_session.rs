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
use ri_ai::registry::{Auth, ModelRegistry};
use ri_ai::stream::{StreamOptions, ThinkingBudgets};
use ri_types::event::AgentEvent;
use ri_types::message::{
    Content, ContentBlock, ImageContent, Message, StopReason, SystemMessage, TextContent,
    ThinkingLevel, UserMessage,
};
use ri_types::model::Model;
use ri_types::settings::QueueMode;
use tokio_util::sync::CancellationToken;

use crate::messages::convert_to_llm;
use crate::resources::{ContextFile, PromptTemplate, Skill, expand_prompt_template};
use crate::session::SessionManager;
use crate::settings::SettingsManager;
use crate::system_prompt::{PromptOptions, build_sections, diff_sections};
use crate::time::now_ms;
use crate::tools::{PromptTool, Runtime, ToolEnv, builtin};

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
            agent_dir: _,
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
        };
        let available: Vec<PromptTool> = ["read", "bash", "edit", "write"]
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

    async fn run(&self, prompts: Vec<Message>) {
        let cancel = CancellationToken::new();
        *lock(&self.inner.cancel) = Some(cancel.clone());
        let (model, thinking_level, active, messages, session_id, session_file) = {
            let mut state = lock(&self.inner.state);
            state.streaming = true;
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
            lock(&self.inner.state).streaming = false;
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
        let budgets = settings.thinking_budgets.as_ref();
        let options = StreamOptions {
            session_id: Some(session_id),
            thinking_budgets: ThinkingBudgets {
                minimal: budgets.and_then(|b| b.minimal),
                low: budgets.and_then(|b| b.low),
                medium: budgets.and_then(|b| b.medium),
                high: budgets.and_then(|b| b.high),
            },
            max_retry_delay_ms: settings
                .retry
                .as_ref()
                .and_then(|retry| retry.provider.as_ref())
                .and_then(|provider| provider.max_retry_delay_ms),
            max_retries: settings
                .retry
                .as_ref()
                .and_then(|retry| retry.provider.as_ref())
                .and_then(|provider| provider.max_retries)
                .unwrap_or(0),
            cancel: cancel.clone(),
            ..StreamOptions::default()
        };
        let apis = self.inner.apis.clone();
        let config = LoopConfig {
            model,
            thinking_level,
            stream: Arc::new(move |request| apis.stream(request)),
            options,
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
        ri_agent::run(prompts, &mut context, config, &hooks).await;
        lock(&self.inner.state).streaming = false;
        *lock(&self.inner.cancel) = None;
        self.emit(&AgentEvent::AgentSettled);
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
        self.session.emit(event);
        if let AgentEvent::MessageEnd { message } = event {
            self.session.with_session(|session| match message {
                Message::Custom(custom) => {
                    let _ = session.append_custom_message(
                        &custom.custom_type,
                        custom.content.clone(),
                        custom.display,
                        custom.details.clone(),
                    );
                }
                Message::System(_)
                | Message::User(_)
                | Message::Assistant(_)
                | Message::ToolResult(_) => {
                    let _ = session.append_message(message.clone());
                }
                _ => {}
            });
        }
        Box::pin(async {})
    }

    fn convert_to_llm(&self, messages: Vec<Message>) -> Vec<Message> {
        convert_to_llm(messages)
    }

    fn auth<'a>(&'a self, model: &'a Model) -> BoxFuture<'a, Auth> {
        Box::pin(async move {
            // The registry is read under a short lock; credential commands run
            // outside it.
            let registry = self
                .session
                .inner
                .registry
                .read()
                .map(|registry| Arc::clone(&registry))
                .ok();
            match registry {
                Some(registry) => registry.auth(model).await,
                None => Auth::default(),
            }
        })
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
