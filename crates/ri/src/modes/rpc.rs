//! RPC mode: JSON commands on stdin; responses and events on stdout. Port of
//! `modes/rpc/rpc-mode.ts` in pi `v1.0.0`.
//!
//! Lines are framed by LF only. Every line starts a command task at once, as
//! pi handles each line without waiting for the previous one, so `get_state`
//! or `steer` answer while a prompt runs. Events of a replaced session are
//! dropped.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use ri_core::agent_session::AgentSession;
use ri_core::session::SessionManager;
use ri_types::message::Message;
use ri_types::rpc::{
    self, CommandSource, ContextUsage, ForkMessage, RpcCommand, SessionState, SessionStats,
    SlashCommand, TokenTotals, response_line,
};
use serde::Serialize;
use serde_json::{Value, json};
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
}

impl Rpc {
    fn session(&self) -> AgentSession {
        self.session.borrow().clone()
    }

    /// Streams `session`'s events and makes it current.
    fn bind(&self, session: AgentSession) {
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
        *self.session.borrow_mut() = session;
    }

    /// pi's runtime teardown: the current run settles and is persisted before
    /// its session is replaced.
    async fn settle(&self) {
        let session = self.session();
        session.abort();
        session.abort_bash();
        session.wait_for_idle().await;
    }

    fn replace(&self, manager: SessionManager) -> Result<(), String> {
        let session = (self.factory)(manager).map_err(|error| error.to_string())?;
        self.bind(session);
        Ok(())
    }

    fn reply(&self, id: Option<&Value>, command: &str, reply: Reply) {
        let outcome = match &reply {
            Ok(data) => Ok(data.as_deref()),
            Err(error) => Err(error.as_str()),
        };
        self.out.line(response_line(id, command, outcome));
    }
}

/// The `type` of a command line, for its response.
fn command_name(value: &Value) -> String {
    match value.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(other) => other.to_string(),
        None => "undefined".to_owned(),
    }
}

async fn handle_line(rpc: Rc<Rpc>, line: String) {
    let parsed: Value = match serde_json::from_str(&line) {
        Ok(parsed) => parsed,
        Err(error) => {
            rpc.reply(
                None,
                "parse",
                Err(format!("Failed to parse command: {error}")),
            );
            return;
        }
    };
    if parsed.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        // No extension asks for input yet, so no request is pending.
        return;
    }
    let id = parsed.get("id").cloned();
    let name = command_name(&parsed);
    let command = match serde_json::from_value::<RpcCommand>(parsed) {
        Ok(command) => command,
        Err(error) => {
            rpc.reply(id.as_ref(), &name, Err(error.to_string()));
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
                rpc.reply(id.as_ref(), "prompt", reply);
            })
            .await;
        if let Err(error) = result
            && !started.get()
        {
            rpc.reply(id.as_ref(), "prompt", Err(error));
        }
        return;
    }
    let reply = match command {
        RpcCommand::Unknown => Err(format!("Unknown command: {name}")),
        command => handle(&rpc, id.as_ref(), command).await,
    };
    rpc.reply(id.as_ref(), &name, reply);
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
            rpc.replace(manager)?;
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
            Some(model) => data(&json!({
                "model": value(&model)?,
                "thinkingLevel": value(&session.thinking_level())?,
                "isScoped": false,
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
        RpcCommand::ExportHtml { .. } => Err("HTML export is not available in ri yet".into()),
        RpcCommand::SwitchSession { session_path } => {
            let fallback = session.cwd().to_path_buf();
            let manager =
                runtime::open_session(std::path::Path::new(&session_path), None, &fallback)
                    .map_err(|error| error.to_string())?;
            rpc.settle().await;
            rpc.replace(manager)?;
            data(&json!({"cancelled": false}))
        }
        RpcCommand::Fork { entry_id } => {
            let fork = runtime::plan_fork(&session, &entry_id, false)?;
            rpc.settle().await;
            rpc.replace(fork.build(&session)?)?;
            data(&json!({"text": fork.text, "cancelled": false}))
        }
        RpcCommand::Clone => {
            let leaf = session
                .with_session(|manager| manager.leaf_id().map(str::to_owned))
                .ok_or("Cannot clone session: no current entry selected")?;
            let fork = runtime::plan_fork(&session, &leaf, true)?;
            rpc.settle().await;
            rpc.replace(fork.build(&session)?)?;
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
            let mut commands: Vec<SlashCommand> = resources
                .templates
                .iter()
                .map(|template| SlashCommand {
                    name: template.name.clone(),
                    description: Some(template.description.clone()),
                    source: CommandSource::Prompt,
                    source_info: template.source.clone(),
                })
                .collect();
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
    });
    rpc.bind(session);
    let local = tokio::task::LocalSet::new();
    let code = local
        .run_until(async {
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
    out.flush();
    drop(writer);
    code
}

/// Resolves to pi's exit code for SIGTERM (143) or SIGHUP (129).
async fn termination() -> u8 {
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
