//! RPC mode: JSON commands on stdin; responses and events on stdout. Port of
//! `modes/rpc/rpc-mode.ts` in pi `v1.0.0`.
//!
//! Lines are framed by LF only. Every line starts a command task at once, as
//! pi handles each line without waiting for the previous one, so `get_state`
//! or `steer` answer while a prompt runs. Events of a replaced session are
//! dropped.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use futures_util::future::BoxFuture;
use tokio::sync::oneshot;
use yapi_core::extensions::{DialogOptions, ExtensionUi, Mode, NotifyKind, Placement, Widget};
use yapi_core::time::uuid_v4;
use yapi_types::sync::lock;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncBufReadExt;
use yapi_core::agent_session::{AgentSession, InputSource, UserBash};
use yapi_types::message::Message;
use yapi_types::rpc::{
    self, CommandSource, ContextUsage, ExtensionUiResponse, ForkMessage, RpcCommand, SessionState,
    SessionStats, SlashCommand, StreamingBehavior, TokenTotals, response_line,
};

use crate::runtime::{self, Runtime, SessionFactory};

enum Out {
    Line(String),
    Flush(mpsc::Sender<()>),
}

/// Stdout, written by its own thread so a slow reader never blocks the
/// runtime.
#[derive(Clone)]
struct Output(mpsc::Sender<Out>);

impl Output {
    fn start() -> (Output, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<Out>();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            while let Ok(first) = rx.recv() {
                // Write what is queued, then flush once.
                let mut next = Some(first);
                while let Some(out) = next.take() {
                    match out {
                        Out::Line(line) => {
                            let _ = stdout.write_all(line.as_bytes());
                            let _ = stdout.write_all(b"\n");
                            next = rx.try_recv().ok();
                        }
                        Out::Flush(done) => {
                            let _ = stdout.flush();
                            let _ = done.send(());
                        }
                    }
                }
                let _ = stdout.flush();
            }
        });
        (Output(tx), writer)
    }

    fn line(&self, line: String) {
        let _ = self.0.send(Out::Line(line));
    }

    /// Waits until everything queued so far is written.
    fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self.0.send(Out::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

fn data<T: Serialize + ?Sized>(value: &T) -> Result<Option<String>, String> {
    yapi_types::json::to_string(value)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// `value` as JSON, to compose response data with `json!`.
fn value<T: Serialize + ?Sized>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

/// What a command answers: `data` as JSON, or an error message.
type Reply = Result<Option<String>, String>;

struct Rpc {
    runtime: Rc<Runtime>,
    out: Output,
    ui: Arc<RpcUi>,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<ExtensionUiResponse>>>>;

/// Extension dialogs as `extension_ui_request` lines, answered by
/// `extension_ui_response` lines; pi's RPC `ExtensionUIContext`.
struct RpcUi {
    out: Output,
    pending: Pending,
    /// pi's `shutdownRequested`: an extension asked to exit once the agent
    /// settles or the current command answers.
    shutdown: AtomicBool,
    /// Wakes the main loop to exit.
    exit: tokio::sync::Notify,
    /// The theme for extensions, as [`ExtensionUi::theme`] describes it.
    theme: Mutex<Value>,
}

impl RpcUi {
    /// pi's `checkShutdownRequested`: exits when an extension asked to.
    fn check_shutdown(&self) {
        if self.shutdown.load(Ordering::SeqCst) {
            self.exit.notify_one();
        }
    }

    /// Writes request `id`.
    fn send(&self, id: &str, mut request: Map<String, Value>) {
        let mut line = Map::new();
        line.insert("type".into(), json!("extension_ui_request"));
        line.insert("id".into(), json!(id));
        line.append(&mut request);
        self.out.line(yapi_types::json::stringify(&line));
    }

    /// Writes a request that needs no response.
    fn tell(&self, request: Value) {
        self.send(&uuid_v4(), object(request));
    }

    /// Writes a request and waits for its response; `None` when the request
    /// or the response is cancelled, the request times out or the session
    /// ends. A timeout is part of the request.
    fn ask(
        &self,
        mut request: Map<String, Value>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<ExtensionUiResponse>> {
        let id = uuid_v4();
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), tx);
        if let Some(timeout) = dialog.timeout {
            request.insert("timeout".into(), json!(timeout.as_millis()));
        }
        self.send(&id, request);
        let pending = Arc::clone(&self.pending);
        Box::pin(async move {
            let cancel = dialog.cancel.unwrap_or_default();
            let expired = async {
                match dialog.timeout {
                    Some(timeout) => tokio::time::sleep(timeout).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                response = rx => response.ok().filter(|response| response.cancelled != Some(true)),
                () = cancel.cancelled() => {
                    lock(&pending).remove(&id);
                    None
                }
                () = expired => {
                    lock(&pending).remove(&id);
                    None
                }
            }
        })
    }

    /// [`RpcUi::ask`] for the response's `value`.
    fn ask_value(
        &self,
        request: Map<String, Value>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let answer = self.ask(request, dialog);
        Box::pin(async move { answer.await.and_then(|response| response.value) })
    }

    fn resolve(&self, response: ExtensionUiResponse) {
        if let Some(tx) = lock(&self.pending).remove(&response.id) {
            let _ = tx.send(response);
        }
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

impl ExtensionUi for RpcUi {
    fn has_ui(&self) -> bool {
        true
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn extension_error(&self, path: &str, event: &str, error: &str, _stack: Option<&str>) {
        let line = json!({"type": "extension_error", "extensionPath": path, "event": event, "error": error});
        self.out.line(yapi_types::json::stringify(&line));
    }

    fn notify(&self, message: &str, kind: NotifyKind) {
        self.tell(json!({"method": "notify", "message": message, "notifyType": kind.as_str()}));
    }

    fn notify_untyped(&self, message: &str) {
        self.tell(json!({"method": "notify", "message": message}));
    }

    fn select(
        &self,
        title: &str,
        options: Vec<String>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        self.ask_value(
            object(json!({"method": "select", "title": title, "options": options})),
            dialog,
        )
    }

    fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let mut request = object(json!({"method": "input", "title": title}));
        if let Some(placeholder) = placeholder {
            request.insert("placeholder".into(), json!(placeholder));
        }
        self.ask_value(request, dialog)
    }

    fn confirm(
        &self,
        title: &str,
        message: &str,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, bool> {
        let answer = self.ask(
            object(json!({"method": "confirm", "title": title, "message": message})),
            dialog,
        );
        Box::pin(async move {
            answer
                .await
                .and_then(|response| response.confirmed)
                .unwrap_or(false)
        })
    }

    fn editor(&self, title: &str, prefill: Option<&str>) -> BoxFuture<'static, Option<String>> {
        let mut request = object(json!({"method": "editor", "title": title}));
        if let Some(prefill) = prefill {
            request.insert("prefill".into(), json!(prefill));
        }
        self.ask_value(request, DialogOptions::default())
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut request = object(json!({"method": "setStatus", "statusKey": key}));
        if let Some(text) = text {
            request.insert("statusText".into(), json!(text));
        }
        self.send(&uuid_v4(), request);
    }

    /// Only lines reach RPC clients; component widgets are not sent.
    fn set_widget(&self, key: &str, widget: Option<Widget>, placement: Option<Placement>) {
        let mut request = object(json!({"method": "setWidget", "widgetKey": key}));
        match widget {
            Some(Widget::Lines(lines)) => {
                request.insert("widgetLines".into(), json!(lines));
            }
            Some(Widget::Component(_)) => return,
            None => {}
        }
        match placement {
            Some(Placement::AboveEditor) => {
                request.insert("widgetPlacement".into(), json!("aboveEditor"));
            }
            Some(Placement::BelowEditor) => {
                request.insert("widgetPlacement".into(), json!("belowEditor"));
            }
            None => {}
        }
        self.send(&uuid_v4(), request);
    }

    fn set_title(&self, title: &str) {
        self.tell(json!({"method": "setTitle", "title": title}));
    }

    fn set_editor_text(&self, text: &str) {
        self.tell(json!({"method": "set_editor_text", "text": text}));
    }

    fn theme(&self) -> Value {
        lock(&self.theme).clone()
    }

    fn set_theme(&self, _name: &str) -> Result<(), String> {
        Err("Theme switching not supported in RPC mode".into())
    }
}

impl Rpc {
    fn session(&self) -> AgentSession {
        self.runtime.session()
    }

    /// pi's RPC session commands bind the replacement again after the
    /// runtime did, so extensions see `session_start` twice.
    async fn rebind(&self) {
        self.runtime.rebind().await;
    }

    fn reply(&self, id: Option<&Value>, command: Option<&str>, reply: Reply) {
        let outcome = match &reply {
            Ok(data) => Ok(data.as_deref()),
            Err(error) => Err(error.as_str()),
        };
        self.out.line(response_line(id, command, outcome));
        self.ui.check_shutdown();
    }
}

/// JavaScript's `String(value)` for a JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                item => js_string(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
        other => other.to_string(),
    }
}

/// The `type` of a command line, for its response; `None` without one.
fn command_name(value: &Value) -> Option<String> {
    match value.get("type") {
        Some(Value::String(name)) => Some(name.clone()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

async fn handle_line(rpc: Rc<Rpc>, line: String) {
    // pi runs extension code to completion before it reads the next line, so
    // a command that such code finished answers before this one.
    let session = rpc.session();
    for extension in session.extensions() {
        extension.settle().await;
    }
    let parsed: Value = match serde_json::from_str(&line) {
        Ok(parsed) => parsed,
        Err(error) => {
            rpc.reply(
                None,
                Some("parse"),
                Err(format!("Failed to parse command: {error}")),
            );
            return;
        }
    };
    if parsed.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        if let Ok(response) = serde_json::from_value::<ExtensionUiResponse>(parsed) {
            rpc.ui.resolve(response);
        }
        return;
    }
    let id = parsed.get("id").cloned();
    // A `type` that is not a string is echoed as it is, as in pi.
    if let Some(kind) = parsed.get("type").filter(|kind| !kind.is_string()) {
        let mut line = Map::new();
        if let Some(id) = &id {
            line.insert("id".into(), id.clone());
        }
        line.insert("type".into(), "response".into());
        line.insert("command".into(), kind.clone());
        line.insert("success".into(), false.into());
        line.insert(
            "error".into(),
            format!("Unknown command: {}", js_string(kind)).into(),
        );
        rpc.out.line(yapi_types::json::stringify(&line));
        return;
    }
    let name = command_name(&parsed);
    // A line without a `type`, an array or a string included, is an unknown
    // command to pi.
    if name.is_none() {
        rpc.reply(id.as_ref(), None, Err("Unknown command: undefined".into()));
        return;
    }
    let command = match serde_json::from_value::<RpcCommand>(parsed) {
        Ok(command) => command,
        Err(error) => {
            rpc.reply(id.as_ref(), name.as_deref(), Err(error.to_string()));
            return;
        }
    };
    if let RpcCommand::Prompt {
        message,
        images,
        streaming_behavior,
    } = command
    {
        // The response is the preflight outcome, before any event of the run.
        let started = Cell::new(false);
        let session = rpc.session();
        let result = session
            .prompt_with(
                &message,
                images,
                streaming_behavior,
                InputSource::Rpc,
                |disposition| {
                    started.set(true);
                    let reply =
                        value(&disposition).and_then(|it| data(&json!({ "disposition": it })));
                    rpc.reply(id.as_ref(), Some("prompt"), reply);
                },
            )
            .await;
        if let Err(error) = result
            && !started.get()
        {
            rpc.reply(id.as_ref(), Some("prompt"), Err(error));
        }
        return;
    }
    let reply = match command {
        RpcCommand::Unknown => Err(format!(
            "Unknown command: {}",
            name.as_deref().unwrap_or("undefined")
        )),
        command => {
            let session = rpc.session();
            let reply = handle(&rpc, id.as_ref(), command).await;
            // Extensions hear of model, thinking level and name changes
            // before the response, as pi's handlers run first.
            session.flush_announcements().await;
            reply
        }
    };
    rpc.reply(id.as_ref(), name.as_deref(), reply);
}

async fn handle(rpc: &Rpc, id: Option<&Value>, command: RpcCommand) -> Reply {
    let session = rpc.session();
    let steer = matches!(command, RpcCommand::Steer { .. });
    match command {
        RpcCommand::Prompt { .. } | RpcCommand::Unknown => Ok(None),
        RpcCommand::Steer { message, images } | RpcCommand::FollowUp { message, images } => {
            let behavior = if steer {
                StreamingBehavior::Steer
            } else {
                StreamingBehavior::FollowUp
            };
            let disposition = session
                .queue_input(&message, images, behavior, InputSource::Rpc)
                .await?;
            data(&json!({ "disposition": value(&disposition)? }))
        }
        RpcCommand::Abort => {
            session.abort();
            session.wait_for_idle().await;
            Ok(None)
        }
        RpcCommand::ClearQueue => {
            let (steering, follow_up) = session.clear_queues();
            data(&json!({"steering": steering, "followUp": follow_up}))
        }
        RpcCommand::NewSession { parent_session } => {
            let cancelled = rpc.runtime.new_session(parent_session).await?;
            if !cancelled {
                rpc.rebind().await;
            }
            data(&json!({ "cancelled": cancelled }))
        }
        RpcCommand::GetState => data(&state(&session)),
        RpcCommand::SetModel { provider, model_id } => {
            let model = session
                .available_models()
                .into_iter()
                .find(|model| model.provider == provider && model.id == model_id)
                .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
            session.set_model(model.clone())?;
            data(&model)
        }
        RpcCommand::CycleModel => match session.cycle_model(true) {
            Some((model, is_scoped)) => data(&json!({
                "model": value(&model)?,
                "thinkingLevel": value(&session.thinking_level())?,
                "isScoped": is_scoped,
            })),
            None => data(&Value::Null),
        },
        RpcCommand::GetAvailableModels => {
            data(&json!({ "models": value(&session.available_models())? }))
        }
        RpcCommand::SetThinkingLevel { level } => {
            session.set_thinking_level(level);
            Ok(None)
        }
        RpcCommand::CycleThinkingLevel => match session.cycle_thinking_level() {
            Some(level) => data(&json!({ "level": value(&level)? })),
            None => data(&Value::Null),
        },
        RpcCommand::GetAvailableThinkingLevels => {
            data(&json!({ "levels": value(&session.available_thinking_levels())? }))
        }
        RpcCommand::SetSteeringMode { mode } => {
            session.set_steering_mode(mode)?;
            Ok(None)
        }
        RpcCommand::SetFollowUpMode { mode } => {
            session.set_follow_up_mode(mode)?;
            Ok(None)
        }
        RpcCommand::Compact {
            custom_instructions,
        } => {
            let result = session.compact(custom_instructions.as_deref()).await?;
            data(&result)
        }
        RpcCommand::SetAutoCompaction { enabled } => {
            session.set_auto_compaction(enabled)?;
            Ok(None)
        }
        RpcCommand::SetAutoRetry { enabled } => {
            session.set_auto_retry(enabled)?;
            Ok(None)
        }
        RpcCommand::AbortRetry => {
            session.abort_retry();
            Ok(None)
        }
        RpcCommand::Bash {
            command,
            exclude_from_context,
        } => {
            let operations = match session
                .user_bash(&command, exclude_from_context.unwrap_or(false))
                .await?
            {
                UserBash::Done(result) => {
                    session.record_bash(&command, &result, exclude_from_context);
                    return data(&result);
                }
                UserBash::Operations(operations) => Some(operations),
                UserBash::Local => None,
            };
            let tag = id.and_then(Value::as_str).map(str::to_owned);
            let result = session
                .execute_bash(&command, exclude_from_context, tag, operations, |_| {})
                .await?;
            data(&result)
        }
        RpcCommand::AbortBash => {
            session.abort_bash();
            Ok(None)
        }
        RpcCommand::GetSessionStats => data(&stats(&session)),
        RpcCommand::ExportHtml { output_path } => {
            let (theme, appearance) = crate::interactive::export_theme(
                session.settings().theme.as_deref(),
                &yapi_core::config::agent_dir(),
            );
            let path = crate::export_html::export_session(
                &session,
                output_path.as_deref(),
                &crate::export_html::ExportTheme {
                    theme: &theme,
                    foreground: None,
                    background: None,
                    appearance,
                },
            )?;
            data(&serde_json::json!({"path": path.display().to_string()}))
        }
        RpcCommand::SwitchSession { session_path } => {
            let cancelled = rpc.runtime.switch_session(&session_path).await?;
            if !cancelled {
                rpc.rebind().await;
            }
            data(&json!({ "cancelled": cancelled }))
        }
        RpcCommand::Fork { entry_id } => match rpc.runtime.fork(&entry_id, false).await? {
            None => data(&json!({"cancelled": true})),
            Some(text) => {
                rpc.rebind().await;
                data(&json!({"text": text, "cancelled": false}))
            }
        },
        RpcCommand::Clone => {
            let leaf = session
                .with_session(|manager| manager.leaf_id().map(str::to_owned))
                .ok_or("Cannot clone session: no current entry selected")?;
            let cancelled = rpc.runtime.fork(&leaf, true).await?.is_none();
            if !cancelled {
                rpc.rebind().await;
            }
            data(&json!({ "cancelled": cancelled }))
        }
        RpcCommand::GetForkMessages => {
            let messages: Vec<ForkMessage> = session
                .user_messages_for_forking()
                .into_iter()
                .map(|(entry_id, text)| ForkMessage { entry_id, text })
                .collect();
            data(&json!({ "messages": value(&messages)? }))
        }
        RpcCommand::GetEntries { since } => session.with_session(|manager| {
            let entries: Vec<_> = manager.entries().collect();
            let start = match &since {
                Some(since) => {
                    entries
                        .iter()
                        .position(|entry| entry.meta().is_some_and(|meta| &meta.id == since))
                        .ok_or_else(|| format!("Entry not found: {since}"))?
                        + 1
                }
                None => 0,
            };
            data(&json!({"entries": value(&entries[start..])?, "leafId": manager.leaf_id()}))
        }),
        RpcCommand::GetTree => session.with_session(|manager| {
            let tree = manager.tree();
            let nodes: Vec<rpc::TreeNode<'_>> = tree
                .nodes
                .iter()
                .map(|node| rpc::TreeNode {
                    entry: &node.entry,
                    children: node.children.clone(),
                    label: node.label.as_deref(),
                    label_timestamp: node.label_timestamp.as_deref(),
                })
                .collect();
            let tree = rpc::tree_json(&nodes, &tree.roots).map_err(|error| error.to_string())?;
            let leaf = yapi_types::json::to_string(&manager.leaf_id())
                .map_err(|error| error.to_string())?;
            Ok(Some(format!("{{\"tree\":{tree},\"leafId\":{leaf}}}")))
        }),
        // pi's `text` is undefined, so absent, when there is none.
        RpcCommand::GetLastAssistantText => match session.last_assistant_text() {
            Some(text) => data(&json!({ "text": text })),
            None => data(&json!({})),
        },
        RpcCommand::SetSessionName { name } => {
            let name = name.trim();
            if name.is_empty() {
                return Err("Session name cannot be empty".into());
            }
            session.set_name(name);
            Ok(None)
        }
        RpcCommand::GetMessages => {
            let messages: Vec<Message> = session.messages();
            data(&json!({ "messages": value(&messages)? }))
        }
        RpcCommand::GetCommands => {
            let resources = session.resources();
            let mut commands: Vec<SlashCommand> = Vec::new();
            commands.extend(session.extension_commands().into_iter().map(|resolved| {
                SlashCommand {
                    name: resolved.invocation,
                    // pi omits a description the command did not give.
                    description: Some(resolved.command.description)
                        .filter(|description| !description.is_empty()),
                    source: CommandSource::Extension,
                    source_info: resolved.extension.source(),
                }
            }));
            commands.extend(resources.templates.iter().map(|template| SlashCommand {
                name: template.name.clone(),
                description: Some(template.description.clone()),
                source: CommandSource::Prompt,
                source_info: template.source.clone(),
            }));
            commands.extend(resources.skills.iter().map(|skill| SlashCommand {
                name: format!("skill:{}", skill.name),
                description: Some(skill.description.clone()),
                source: CommandSource::Skill,
                source_info: skill.source.clone(),
            }));
            data(&json!({ "commands": value(&commands)? }))
        }
    }
}

/// pi-agent-core's `DEFAULT_MODEL`, which a session without models reports.
fn placeholder_model() -> Value {
    json!({
        "id": "unknown",
        "name": "unknown",
        "api": "unknown",
        "provider": "unknown",
        "baseUrl": "",
        "reasoning": false,
        "input": [],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 0,
        "maxTokens": 0,
    })
}

fn state(session: &AgentSession) -> SessionState {
    let (file, id, name) = session.with_session(|manager| {
        (
            manager.file().map(|file| file.display().to_string()),
            manager.id().to_owned(),
            manager.name(),
        )
    });
    SessionState {
        // pi's agent starts from a placeholder model when none is available.
        model: Some(session.model().map_or_else(placeholder_model, |model| {
            serde_json::to_value(model).unwrap_or_default()
        })),
        thinking_level: session.thinking_level(),
        is_streaming: session.is_streaming(),
        is_compacting: session.is_compacting(),
        steering_mode: session.steering_mode(),
        follow_up_mode: session.follow_up_mode(),
        session_file: file,
        session_id: id,
        session_name: name,
        auto_compaction_enabled: session.auto_compaction_enabled(),
        message_count: session.messages().len(),
        pending_message_count: session.pending_message_count(),
    }
}

fn stats(session: &AgentSession) -> SessionStats {
    let counts = session.session_stats();
    let totals = session.usage_totals();
    let (file, id) = session.with_session(|manager| {
        (
            manager.file().map(|file| file.display().to_string()),
            manager.id().to_owned(),
        )
    });
    SessionStats {
        session_file: file,
        session_id: id,
        user_messages: counts.user_messages,
        assistant_messages: counts.assistant_messages,
        tool_calls: counts.tool_calls,
        tool_results: counts.tool_results,
        total_messages: counts.total_messages,
        tokens: TokenTotals {
            input: totals.input,
            output: totals.output,
            cache_read: totals.cache_read,
            cache_write: totals.cache_write,
            total: totals.input + totals.output + totals.cache_read + totals.cache_write,
        },
        cost: totals.cost,
        context_usage: session.context_usage().map(|usage| ContextUsage {
            tokens: usage.tokens,
            context_window: usage.context_window,
            percent: usage.percent(),
        }),
    }
}

/// Runs RPC mode until stdin closes or a termination signal arrives; returns
/// the exit code.
pub async fn run(session: AgentSession, factory: SessionFactory) -> u8 {
    let (out, writer) = Output::start();
    let ui = Arc::new(RpcUi {
        out: out.clone(),
        pending: Arc::default(),
        shutdown: AtomicBool::new(false),
        exit: tokio::sync::Notify::new(),
        theme: Mutex::new(crate::interactive::extension_theme(
            session.settings().theme.as_deref(),
            &yapi_core::config::agent_dir(),
        )),
    });
    // The current session's number; listeners of older ones stay silent.
    let epoch = Arc::new(AtomicU64::new(0));
    let bind: runtime::Bind = {
        let (out, ui) = (out.clone(), Arc::clone(&ui));
        // As in pi, a session's events stream once its extensions have
        // started.
        Box::new(move |session, replaced| {
            let (out, ui, epoch) = (out.clone(), Arc::clone(&ui), Arc::clone(&epoch));
            Box::pin(async move {
                session
                    .bind_extensions(Arc::clone(&ui) as Arc<dyn ExtensionUi>, Mode::Rpc, replaced)
                    .await;
                runtime::forward_newest(&session, &epoch, move |event| {
                    if let Ok(line) = yapi_types::json::to_string(event) {
                        out.line(line);
                    }
                    if matches!(event, yapi_types::event::AgentEvent::AgentSettled) {
                        ui.check_shutdown();
                    }
                });
            })
        })
    };
    let local = tokio::task::LocalSet::new();
    let mut current = None;
    let code = local
        .run_until(async {
            let rpc = Rc::new(Rpc {
                runtime: Runtime::start(session, factory, bind).await,
                out: out.clone(),
                ui,
            });
            current = Some(Rc::clone(&rpc));
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut buffer = Vec::new();
            let signal = termination();
            tokio::pin!(signal);
            loop {
                buffer.clear();
                let read = tokio::select! {
                    read = stdin.read_until(b'\n', &mut buffer) => read,
                    code = &mut signal => return code,
                    () = rpc.ui.exit.notified() => return 0,
                };
                match read {
                    Ok(0) | Err(_) => return 0,
                    Ok(_) => {
                        let mut line = String::from_utf8_lossy(&buffer).into_owned();
                        if line.ends_with('\n') {
                            line.pop();
                        }
                        if line.ends_with('\r') {
                            line.pop();
                        }
                        tokio::task::spawn_local(handle_line(Rc::clone(&rpc), line));
                        // Start the command before reading the next line.
                        tokio::task::yield_now().await;
                    }
                }
            }
        })
        .await;
    let Some(rpc) = current else {
        return code;
    };
    let session = rpc.session();
    session.abort();
    session.abort_bash();
    // Commands still running would outlive yapi in their own process groups.
    yapi_core::tools::bash::kill_tracked_children();
    session.shutdown().await;
    out.flush();
    drop(writer);
    code
}

/// Resolves to pi's exit code for SIGTERM (143) or SIGHUP (129).
pub(crate) async fn termination() -> u8 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => 143,
            _ = hup.recv() => 129,
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending().await
    }
}
