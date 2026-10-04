//! RPC mode: JSON commands on stdin; responses and events on stdout. Port of
//! `modes/rpc/rpc-mode.ts` in pi `v1.0.0`.
//!
//! Lines are framed by LF only. Every line starts a command task at once, as
//! pi handles each line without waiting for the previous one, so `get_state`
//! or `steer` answer while a prompt runs. Events of a replaced session are
//! dropped.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use futures_util::future::BoxFuture;
use ri_core::extensions::{DialogOptions, ExtensionUi, Mode, NotifyKind, Placement, Widget};
use ri_core::time::uuid_v4;
use tokio::sync::oneshot;

use ri_core::agent_session::AgentSession;
use ri_core::session::SessionManager;
use ri_types::message::Message;
use ri_types::rpc::{
    self, CommandSource, ContextUsage, ExtensionUiResponse, ForkMessage, RpcCommand, SessionState,
    SessionStats, SlashCommand, TokenTotals, response_line,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncBufReadExt;

use crate::runtime::{self, SessionFactory};

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
    ri_types::json::to_string(value)
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
    session: RefCell<AgentSession>,
    /// The current session's number; listeners of older ones stay silent.
    epoch: Arc<AtomicU64>,
    factory: SessionFactory,
    out: Output,
    ui: Arc<RpcUi>,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<ExtensionUiResponse>>>>;

/// Extension dialogs as `extension_ui_request` lines, answered by
/// `extension_ui_response` lines; pi's RPC `ExtensionUIContext`.
struct RpcUi {
    out: Output,
    pending: Pending,
    /// The theme for extensions, as [`ExtensionUi::theme`] describes it.
    theme: Mutex<Value>,
}

impl RpcUi {
    /// Writes request `id`.
    fn send(&self, id: &str, mut request: Map<String, Value>) {
        let mut line = Map::new();
        line.insert("type".into(), json!("extension_ui_request"));
        line.insert("id".into(), json!(id));
        line.append(&mut request);
        if let Ok(text) = ri_types::json::to_string(&line) {
            self.out.line(text);
        }
    }

    /// Writes a request that needs no response.
    fn tell(&self, request: Value) {
        self.send(&uuid_v4(), object(request));
    }

    /// Writes a request and waits for its response; `None` when the request
    /// is cancelled, times out or the session ends. A timeout is part of the
    /// request.
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
                response = rx => response.ok(),
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

    fn resolve(&self, response: ExtensionUiResponse) {
        if let Some(tx) = lock(&self.pending).remove(&response.id) {
            let _ = tx.send(response);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    fn extension_error(&self, path: &str, event: &str, error: &str) {
        let line = json!({"type": "extension_error", "extensionPath": path, "event": event, "error": error});
        if let Ok(text) = ri_types::json::to_string(&line) {
            self.out.line(text);
        }
    }

    fn notify(&self, message: &str, kind: NotifyKind) {
        self.tell(json!({"method": "notify", "message": message, "notifyType": kind.as_str()}));
    }

    fn select(
        &self,
        title: &str,
        options: Vec<String>,
        dialog: DialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let answer = self.ask(
            object(json!({"method": "select", "title": title, "options": options})),
            dialog,
        );
        Box::pin(async move {
            answer
                .await
                .filter(|response| response.cancelled != Some(true))
                .and_then(|response| response.value)
        })
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
        let answer = self.ask(request, dialog);
        Box::pin(async move {
            answer
                .await
                .filter(|response| response.cancelled != Some(true))
                .and_then(|response| response.value)
        })
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
                .filter(|response| response.cancelled != Some(true))
                .and_then(|response| response.confirmed)
                .unwrap_or(false)
        })
    }

    fn editor(&self, title: &str, prefill: Option<&str>) -> BoxFuture<'static, Option<String>> {
        let mut request = object(json!({"method": "editor", "title": title}));
        if let Some(prefill) = prefill {
            request.insert("prefill".into(), json!(prefill));
        }
        let answer = self.ask(request, DialogOptions::default());
        Box::pin(async move {
            answer
                .await
                .filter(|response| response.cancelled != Some(true))
                .and_then(|response| response.value)
        })
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
}

impl Rpc {
    fn session(&self) -> AgentSession {
        self.session.borrow().clone()
    }

    /// Streams `session`'s events, makes it current and starts its
    /// extensions.
    async fn bind(&self, session: AgentSession) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let current = Arc::clone(&self.epoch);
        let out = self.out.clone();
        session.subscribe(Box::new(move |event| {
            if current.load(Ordering::SeqCst) == epoch
                && let Ok(line) = ri_types::json::to_string(event)
            {
                out.line(line);
            }
        }));
        *self.session.borrow_mut() = session.clone();
        session
            .bind_extensions(Arc::clone(&self.ui) as Arc<dyn ExtensionUi>, Mode::Rpc)
            .await;
    }

    /// pi's runtime teardown: the current run settles and is persisted, and
    /// extensions stop, before the session is replaced.
    async fn settle(&self) {
        let session = self.session();
        session.abort();
        session.abort_bash();
        session.wait_for_idle().await;
        session.shutdown().await;
    }

    async fn replace(&self, manager: SessionManager) -> Result<(), String> {
        let session = (self.factory)(manager).map_err(|error| error.to_string())?;
        self.bind(session).await;
        Ok(())
    }

    fn reply(&self, id: Option<&Value>, command: Option<&str>, reply: Reply) {
        let outcome = match &reply {
            Ok(data) => Ok(data.as_deref()),
            Err(error) => Err(error.as_str()),
        };
        self.out.line(response_line(id, command, outcome));
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
            .prompt_with(&message, images, streaming_behavior, |disposition| {
                started.set(true);
                let reply = value(&disposition).and_then(|it| data(&json!({ "disposition": it })));
                rpc.reply(id.as_ref(), Some("prompt"), reply);
            })
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
        command => handle(&rpc, id.as_ref(), command).await,
    };
    rpc.reply(id.as_ref(), name.as_deref(), reply);
}

async fn handle(rpc: &Rpc, id: Option<&Value>, command: RpcCommand) -> Reply {
    let session = rpc.session();
    match command {
        RpcCommand::Prompt { .. } | RpcCommand::Unknown => Ok(None),
        RpcCommand::Steer { message, images } => {
            session.steer(&message, images);
            data(&json!({"disposition": "queued"}))
        }
        RpcCommand::FollowUp { message, images } => {
            session.follow_up(&message, images);
            data(&json!({"disposition": "queued"}))
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
            rpc.settle().await;
            let manager = runtime::new_session(&session, parent_session)
                .map_err(|error| error.to_string())?;
            rpc.replace(manager).await?;
            data(&json!({"cancelled": false}))
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
            let tag = id.and_then(Value::as_str).map(str::to_owned);
            let result = session
                .execute_bash(&command, exclude_from_context, tag, |_| {})
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
                &ri_core::config::agent_dir(),
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
            let fallback = session.cwd().to_path_buf();
            let manager =
                runtime::open_session(std::path::Path::new(&session_path), None, &fallback)
                    .map_err(|error| error.to_string())?;
            rpc.settle().await;
            rpc.replace(manager).await?;
            data(&json!({"cancelled": false}))
        }
        RpcCommand::Fork { entry_id } => {
            let fork = runtime::plan_fork(&session, &entry_id, false)?;
            rpc.settle().await;
            rpc.replace(fork.build(&session)?).await?;
            data(&json!({"text": fork.text, "cancelled": false}))
        }
        RpcCommand::Clone => {
            let leaf = session
                .with_session(|manager| manager.leaf_id().map(str::to_owned))
                .ok_or("Cannot clone session: no current entry selected")?;
            let fork = runtime::plan_fork(&session, &leaf, true)?;
            rpc.settle().await;
            rpc.replace(fork.build(&session)?).await?;
            data(&json!({"cancelled": false}))
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
            let leaf =
                ri_types::json::to_string(&manager.leaf_id()).map_err(|error| error.to_string())?;
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
            for extension in session.extensions() {
                commands.extend(
                    extension
                        .commands()
                        .into_iter()
                        .map(|command| SlashCommand {
                            name: command.name,
                            description: Some(command.description),
                            source: CommandSource::Extension,
                            source_info: extension.source(),
                        }),
                );
            }
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

fn state(session: &AgentSession) -> SessionState {
    let (file, id, name) = session.with_session(|manager| {
        (
            manager.file().map(|file| file.display().to_string()),
            manager.id().to_owned(),
            manager.name(),
        )
    });
    SessionState {
        model: session
            .model()
            .and_then(|model| serde_json::to_value(model).ok()),
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
    let rpc = Rc::new(Rpc {
        session: RefCell::new(session.clone()),
        epoch: Arc::new(AtomicU64::new(0)),
        factory,
        out: out.clone(),
        ui: Arc::new(RpcUi {
            out: out.clone(),
            pending: Arc::default(),
            theme: Mutex::new(crate::interactive::extension_theme(
                session.settings().theme.as_deref(),
                &ri_core::config::agent_dir(),
            )),
        }),
    });
    let local = tokio::task::LocalSet::new();
    let code = local
        .run_until(async {
            rpc.bind(session).await;
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut buffer = Vec::new();
            let signal = termination();
            tokio::pin!(signal);
            loop {
                buffer.clear();
                let read = tokio::select! {
                    read = stdin.read_until(b'\n', &mut buffer) => read,
                    code = &mut signal => return code,
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
    let session = rpc.session();
    session.abort();
    session.abort_bash();
    // Commands still running would outlive ri in their own process groups.
    ri_core::tools::bash::kill_tracked_children();
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
