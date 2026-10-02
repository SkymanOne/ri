//! The agent loop: stream a response, run its tool calls, repeat.
//!
//! Port of `packages/agent/src/agent-loop.ts` in pi `v1.0.0`.

use std::sync::Arc;

use futures_util::future::join_all;
use ri_ai::stream::{EventStream, Request, StreamEvent, StreamOptions};
use ri_ai::transcript::{current_tools, tool_state_changes};
use ri_ai::validation::validate_tool_arguments;
use ri_types::event::{AgentEvent, ToolResult};
use ri_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, SystemMessage, TextContent,
    ThinkingLevel, ToolCall, ToolDeclaration, ToolResultMessage,
};
use ri_types::model::Model;
use serde_json::Value;

use crate::hooks::{AfterToolCall, AgentHooks, BeforeToolCall, Turn, TurnDecision};
use crate::tool::{ExecutionMode, Tool, UpdateSink};

/// Starts a provider request; dispatches on the model's wire API.
pub type StreamFn = Arc<dyn Fn(Request) -> EventStream + Send + Sync>;

/// The transcript and the tools the model may call.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// Messages so far; the loop appends to them.
    pub messages: Vec<Message>,
    /// Active tools; changes are declared to the model through system messages.
    pub tools: Vec<Arc<dyn Tool>>,
}

/// How the loop calls the model.
#[derive(Clone)]
pub struct LoopConfig {
    /// The model to call.
    pub model: Model,
    /// Thinking level; `Off` disables thinking.
    pub thinking_level: ThinkingLevel,
    /// Starts provider requests.
    pub stream: StreamFn,
    /// Options for every request; `cancel` aborts the run.
    pub options: StreamOptions,
    /// Forces sequential tool execution.
    pub tool_execution: ExecutionMode,
}

/// Runs the loop for new prompt messages and returns every message it added.
///
/// Events go to `hooks.on_event` in order: `agent_start`, then per turn
/// `turn_start`, the turn's messages and tool executions, `turn_end`, and finally
/// `agent_end`.
pub async fn run(
    prompts: Vec<Message>,
    context: &mut AgentContext,
    config: LoopConfig,
    hooks: &dyn AgentHooks,
) -> Vec<Message> {
    let initial = declare_tool_changes(&context.messages, &context.tools, prompts);
    let mut new_messages = initial.clone();
    context.messages.extend(initial.iter().cloned());

    hooks.on_event(&AgentEvent::AgentStart).await;
    hooks.on_event(&AgentEvent::TurnStart).await;
    for message in initial {
        emit_message(hooks, &message).await;
    }
    run_loop(context, &mut new_messages, config, hooks).await;
    new_messages
}

/// Continues from the current transcript, which must not end with an assistant
/// message.
pub async fn run_continue(
    context: &mut AgentContext,
    config: LoopConfig,
    hooks: &dyn AgentHooks,
) -> Result<Vec<Message>, String> {
    match context.messages.last() {
        None => return Err("Cannot continue: no messages in context".into()),
        Some(Message::Assistant(_)) => {
            return Err("Cannot continue from message role: assistant".into());
        }
        Some(_) => {}
    }
    let mut new_messages = Vec::new();
    hooks.on_event(&AgentEvent::AgentStart).await;
    hooks.on_event(&AgentEvent::TurnStart).await;
    run_loop(context, &mut new_messages, config, hooks).await;
    Ok(new_messages)
}

async fn emit_message(hooks: &dyn AgentHooks, message: &Message) {
    hooks
        .on_event(&AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await;
    hooks
        .on_event(&AgentEvent::MessageEnd {
            message: message.clone(),
        })
        .await;
}

async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<Message>,
    mut config: LoopConfig,
    hooks: &dyn AgentHooks,
) {
    let mut first_turn = true;
    let mut explicit_continuation = false;
    let mut pending = hooks.steering_messages().await;

    loop {
        let mut more_tool_calls = true;
        while more_tool_calls || !pending.is_empty() {
            if !first_turn {
                if pending.is_empty() {
                    pending = hooks.steering_messages().await;
                }
                hooks.on_event(&AgentEvent::TurnStart).await;
            }
            first_turn = false;

            if let Some(tools) = hooks.current_tools() {
                context.tools = tools;
            }
            for message in declare_tool_changes(
                &context.messages,
                &context.tools,
                std::mem::take(&mut pending),
            ) {
                emit_message(hooks, &message).await;
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            if let Some(update) = hooks.prepare_request(&context.messages).await {
                if let Some(messages) = update.messages {
                    context.messages = messages;
                }
                if let Some(model) = update.model {
                    config.model = model;
                }
                if let Some(level) = update.thinking_level {
                    config.thinking_level = level;
                }
            }

            let message = stream_response(context, &config, hooks).await;
            new_messages.push(Message::Assistant(Box::new(message.clone())));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                hooks
                    .finish_turn(Turn {
                        message: &message,
                        tool_results: &[],
                        messages: &context.messages,
                    })
                    .await;
                hooks
                    .on_event(&AgentEvent::TurnEnd {
                        message: Message::Assistant(Box::new(message)),
                        tool_results: Vec::new(),
                    })
                    .await;
                end(hooks, new_messages).await;
                return;
            }

            let calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect();
            let mut tool_results = Vec::new();
            more_tool_calls = false;
            if !calls.is_empty() {
                let (results, terminate) = if message.stop_reason == StopReason::Length {
                    (fail_truncated_calls(&calls, hooks).await, false)
                } else {
                    execute_tool_calls(context, &message, &calls, &config, hooks).await
                };
                more_tool_calls = !terminate;
                for result in &results {
                    context.messages.push(Message::ToolResult(result.clone()));
                    new_messages.push(Message::ToolResult(result.clone()));
                }
                tool_results = results;
            }

            let decision = hooks
                .finish_turn(Turn {
                    message: &message,
                    tool_results: &tool_results,
                    messages: &context.messages,
                })
                .await;
            hooks
                .on_event(&AgentEvent::TurnEnd {
                    message: Message::Assistant(Box::new(message)),
                    tool_results,
                })
                .await;
            if decision == TurnDecision::End {
                end(hooks, new_messages).await;
                return;
            }
            explicit_continuation = decision == TurnDecision::Continue;
            pending = hooks.steering_messages().await;
            if more_tool_calls || !pending.is_empty() {
                explicit_continuation = false;
            }
        }

        let follow_up = hooks.follow_up_messages().await;
        if !follow_up.is_empty() {
            explicit_continuation = false;
            pending = follow_up;
            continue;
        }
        if explicit_continuation {
            explicit_continuation = false;
            continue;
        }
        break;
    }
    end(hooks, new_messages).await;
}

async fn end(hooks: &dyn AgentHooks, new_messages: &[Message]) {
    hooks
        .on_event(&AgentEvent::AgentEnd {
            messages: new_messages.to_vec(),
            will_retry: false,
        })
        .await;
}

/// Adds the tool changes since the transcript's last declaration to the last
/// pending system message, or inserts a system message for them before the first
/// non-system message.
fn declare_tool_changes(
    messages: &[Message],
    tools: &[Arc<dyn Tool>],
    pending: Vec<Message>,
) -> Vec<Message> {
    let system_index = pending
        .iter()
        .rposition(|message| matches!(message, Message::System(_)));
    let mut baseline = pending.clone();
    let pending_system = system_index.and_then(|index| match &pending[index] {
        Message::System(system) => Some(system.clone()),
        _ => None,
    });
    if let (Some(index), Some(system)) = (system_index, &pending_system) {
        baseline[index] = Message::System(with_tool_changes(system, Vec::new(), Vec::new()));
    }
    let declared: Vec<Message> = messages.iter().chain(&baseline).cloned().collect();
    let current: Vec<ToolDeclaration> = tools
        .iter()
        .map(|tool| tool.declaration().clone())
        .collect();
    let (added, removed) = tool_state_changes(&current_tools(&declared), &current);
    let unchanged = added.is_empty() && removed.is_empty();

    if let (Some(index), Some(system)) = (system_index, pending_system) {
        let had_changes = system.tools_added.as_ref().is_some_and(|t| !t.is_empty())
            || system.tools_removed.as_ref().is_some_and(|t| !t.is_empty());
        if unchanged && !had_changes {
            return pending;
        }
        baseline[index] = Message::System(with_tool_changes(&system, added, removed));
        return baseline;
    }
    if unchanged {
        return pending;
    }
    let update = SystemMessage {
        content: Content::Text(String::new()),
        sections: None,
        timestamp: ri_ai::stream::now_ms(),
        tools_added: None,
        tools_removed: None,
    };
    let update = Message::System(with_tool_changes(&update, added, removed));
    let index = pending
        .iter()
        .position(|message| !matches!(message, Message::System(_)))
        .unwrap_or(pending.len());
    let mut result = pending;
    result.insert(index, update);
    result
}

fn with_tool_changes(
    system: &SystemMessage,
    added: Vec<ToolDeclaration>,
    removed: Vec<ri_types::message::ToolReference>,
) -> SystemMessage {
    SystemMessage {
        tools_added: (!added.is_empty()).then_some(added),
        tools_removed: (!removed.is_empty()).then_some(removed),
        ..system.clone()
    }
}

async fn stream_response(
    context: &mut AgentContext,
    config: &LoopConfig,
    hooks: &dyn AgentHooks,
) -> AssistantMessage {
    let messages = hooks.transform_context(context.messages.clone()).await;
    let messages = hooks.convert_to_llm(messages);
    let auth = hooks.auth(&config.model).await;
    let mut headers = config.options.headers.clone();
    headers.extend(auth.headers);
    let reasoning = (config.thinking_level != ThinkingLevel::Off).then_some(config.thinking_level);
    let request = Request {
        model: config.model.clone(),
        messages,
        options: StreamOptions {
            api_key: auth.api_key.or_else(|| config.options.api_key.clone()),
            headers,
            reasoning,
            ..config.options.clone()
        },
    };
    let mut stream = (config.stream)(request);
    let mut started = false;
    let finish = |mut message: AssistantMessage| {
        message.thinking_level = Some(config.thinking_level);
        message
    };
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Start(partial) => {
                started = true;
                hooks
                    .on_event(&AgentEvent::MessageStart {
                        message: Message::Assistant(Box::new(partial)),
                    })
                    .await;
            }
            StreamEvent::Update { event, usage } => {
                if started {
                    hooks
                        .on_event(&AgentEvent::MessageUpdate {
                            usage,
                            assistant_message_event: event,
                        })
                        .await;
                }
            }
            StreamEvent::Done(message) | StreamEvent::Error(message) => {
                let message = finish(message);
                return complete(context, hooks, message, started).await;
            }
        }
    }
    // A provider that ends without a final event: report it as an error.
    let mut message = ri_ai::stream::new_output(&config.model, ri_ai::stream::now_ms());
    message.stop_reason = StopReason::Error;
    message.error_message = Some("Provider stream ended without a result".into());
    complete(context, hooks, finish(message), started).await
}

async fn complete(
    context: &mut AgentContext,
    hooks: &dyn AgentHooks,
    message: AssistantMessage,
    started: bool,
) -> AssistantMessage {
    let wrapped = Message::Assistant(Box::new(message.clone()));
    context.messages.push(wrapped.clone());
    if !started {
        hooks
            .on_event(&AgentEvent::MessageStart {
                message: wrapped.clone(),
            })
            .await;
    }
    hooks
        .on_event(&AgentEvent::MessageEnd { message: wrapped })
        .await;
    message
}

fn error_result(message: &str) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text: message.to_owned(),
            text_signature: None,
        })],
        details: Some(Value::Object(Default::default())),
        ..ToolResult::default()
    }
}

struct Outcome {
    call: ToolCall,
    result: ToolResult,
    is_error: bool,
}

async fn fail_truncated_calls(
    calls: &[ToolCall],
    hooks: &dyn AgentHooks,
) -> Vec<ToolResultMessage> {
    let mut messages = Vec::new();
    for call in calls {
        emit_start(hooks, call).await;
        let outcome = Outcome {
            call: call.clone(),
            result: error_result(&format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )),
            is_error: true,
        };
        emit_end(hooks, &outcome).await;
        let message = result_message(&outcome);
        emit_message(hooks, &Message::ToolResult(message.clone())).await;
        messages.push(message);
    }
    messages
}

async fn emit_start(hooks: &dyn AgentHooks, call: &ToolCall) {
    hooks
        .on_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: Value::Object(call.arguments.clone()),
            parent_tool_call_id: None,
        })
        .await;
}

async fn emit_end(hooks: &dyn AgentHooks, outcome: &Outcome) {
    hooks
        .on_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: outcome.call.id.clone(),
            tool_name: outcome.call.name.clone(),
            result: outcome.result.clone(),
            is_error: outcome.is_error,
            parent_tool_call_id: None,
        })
        .await;
}

fn result_message(outcome: &Outcome) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: outcome.call.id.clone(),
        tool_name: outcome.call.name.clone(),
        content: outcome.result.content.clone(),
        details: outcome.result.details.clone(),
        usage: outcome.result.usage.clone(),
        is_error: outcome.is_error,
        timestamp: ri_ai::stream::now_ms(),
        nested_calls: None,
    }
}

/// A call ready to run.
struct Prepared {
    call: ToolCall,
    tool: Arc<dyn Tool>,
    args: Value,
}

/// A prepared call, or the outcome when the call cannot run.
type Preparation = Result<Prepared, Box<Outcome>>;

async fn prepare(
    context: &AgentContext,
    assistant: &AssistantMessage,
    call: &ToolCall,
    config: &LoopConfig,
    hooks: &dyn AgentHooks,
) -> Preparation {
    let immediate = |message: &str, terminate: bool| {
        let mut result = error_result(message);
        if terminate {
            result.terminate = Some(true);
        }
        Err(Box::new(Outcome {
            call: call.clone(),
            result,
            is_error: true,
        }))
    };
    let Some(tool) = context
        .tools
        .iter()
        .find(|tool| tool.declaration().name == call.name)
    else {
        return immediate(&format!("Tool {} not found", call.name), false);
    };
    let prepared_call = ToolCall {
        arguments: tool.prepare_arguments(call.arguments.clone()),
        ..call.clone()
    };
    let args = match validate_tool_arguments(tool.declaration(), &prepared_call) {
        Ok(args) => args,
        Err(message) => return immediate(&message, false),
    };
    let block = hooks
        .before_tool_call(BeforeToolCall {
            assistant_message: assistant,
            tool_call: call,
            args: &args,
            messages: &context.messages,
        })
        .await;
    if config.options.cancel.is_cancelled() {
        return immediate("Operation aborted", false);
    }
    if let Some(block) = block {
        return immediate(
            block
                .reason
                .as_deref()
                .unwrap_or("Tool execution was blocked"),
            block.terminate,
        );
    }
    Ok(Prepared {
        call: call.clone(),
        tool: tool.clone(),
        args,
    })
}

async fn execute_prepared(
    call: ToolCall,
    tool: Arc<dyn Tool>,
    args: Value,
    assistant: &AssistantMessage,
    config: &LoopConfig,
    hooks: &dyn AgentHooks,
    updates: UpdateSink,
) -> Outcome {
    let executed = tool
        .execute(
            call.id.clone(),
            args.clone(),
            config.options.cancel.child_token(),
            updates,
        )
        .await;
    let (mut result, mut is_error) = match executed {
        Ok(result) => {
            let is_error = result.is_error == Some(true);
            (result, is_error)
        }
        Err(message) => (error_result(&message), true),
    };
    if let Some(patch) = hooks
        .after_tool_call(AfterToolCall {
            assistant_message: assistant,
            tool_call: &call,
            args: &args,
            result: &result,
            is_error,
        })
        .await
    {
        if let Some(content) = patch.content {
            result.content = content;
            result.structured_content = None;
        }
        if let Some(details) = patch.details {
            result.details = Some(details);
        }
        if let Some(terminate) = patch.terminate {
            result.terminate = Some(terminate);
        }
        if let Some(flag) = patch.is_error {
            is_error = flag;
        }
    }
    Outcome {
        call,
        result,
        is_error,
    }
}

/// Runs a prepared call, emitting its progress updates as they arrive. Updates
/// reported after the tool finished are dropped, as in pi.
async fn run_with_updates(
    call: ToolCall,
    tool: Arc<dyn Tool>,
    args: Value,
    assistant: &AssistantMessage,
    config: &LoopConfig,
    hooks: &dyn AgentHooks,
) -> Outcome {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let (id, name) = (call.id.clone(), call.name.clone());
    let event_args = Value::Object(call.arguments.clone());
    let sink: UpdateSink = Arc::new(move |partial: ToolResult| {
        let _ = sender.send(AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            args: event_args.clone(),
            partial_result: partial,
            parent_tool_call_id: None,
        });
    });
    let execution = execute_prepared(call, tool, args, assistant, config, hooks, sink);
    tokio::pin!(execution);
    loop {
        tokio::select! {
            biased;
            Some(event) = receiver.recv() => hooks.on_event(&event).await,
            outcome = &mut execution => {
                while let Ok(event) = receiver.try_recv() {
                    hooks.on_event(&event).await;
                }
                return outcome;
            }
        }
    }
}

async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    calls: &[ToolCall],
    config: &LoopConfig,
    hooks: &dyn AgentHooks,
) -> (Vec<ToolResultMessage>, bool) {
    let sequential = config.tool_execution == ExecutionMode::Sequential
        || calls.iter().any(|call| {
            context.tools.iter().any(|tool| {
                tool.declaration().name == call.name
                    && tool.execution_mode() == ExecutionMode::Sequential
            })
        });
    let cancel = &config.options.cancel;
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut messages = Vec::new();

    if sequential {
        for call in calls {
            emit_start(hooks, call).await;
            let outcome = match prepare(context, assistant, call, config, hooks).await {
                Err(outcome) => *outcome,
                Ok(Prepared { call, tool, args }) => {
                    run_with_updates(call, tool, args, assistant, config, hooks).await
                }
            };
            emit_end(hooks, &outcome).await;
            let message = result_message(&outcome);
            emit_message(hooks, &Message::ToolResult(message.clone())).await;
            messages.push(message);
            outcomes.push(outcome);
            if cancel.is_cancelled() {
                break;
            }
        }
    } else {
        let mut entries = Vec::new();
        for call in calls {
            emit_start(hooks, call).await;
            let preparation = prepare(context, assistant, call, config, hooks).await;
            if let Err(outcome) = &preparation {
                emit_end(hooks, outcome).await;
            }
            entries.push(preparation);
            if cancel.is_cancelled() {
                break;
            }
        }
        let runs = entries.into_iter().map(|entry| async move {
            match entry {
                Err(outcome) => *outcome,
                Ok(Prepared { call, tool, args }) => {
                    if cancel.is_cancelled() {
                        let outcome = Outcome {
                            call,
                            result: error_result("Operation aborted"),
                            is_error: true,
                        };
                        emit_end(hooks, &outcome).await;
                        return outcome;
                    }
                    let outcome =
                        run_with_updates(call, tool, args, assistant, config, hooks).await;
                    emit_end(hooks, &outcome).await;
                    outcome
                }
            }
        });
        outcomes = join_all(runs).await;
        for outcome in &outcomes {
            let message = result_message(outcome);
            emit_message(hooks, &Message::ToolResult(message.clone())).await;
            messages.push(message);
        }
    }
    let terminate = !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|outcome| outcome.result.terminate == Some(true));
    (messages, terminate)
}
