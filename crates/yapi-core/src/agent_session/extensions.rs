//! Extension dispatch: pi events, input and command handlers, and the
//! agent loop hooks that deliver loop events to extensions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use yapi_agent::Tool;
use yapi_agent::hooks::AgentHooks;
use yapi_ai::registry::Auth;
use yapi_types::event::AgentEvent;
use yapi_types::message::{
    Content, ContentBlock, ImageContent, Message, StopReason, SystemMessage, ToolResultMessage,
};
use yapi_types::model::Model;
use yapi_types::rpc::SourceInfo;
use yapi_types::rpc::StreamingBehavior;
use yapi_types::settings::QueueMode;
use yapi_types::sync::{lock, write};

use crate::extensions::{Context, Extension, ExtensionUi, Mode, ToolRenderers};
use crate::messages::convert_to_llm;
use crate::time::now_ms;

use super::{AgentSession, InputSource, Replacement, SessionChange, drain};

fn behavior_name(behavior: StreamingBehavior) -> &'static str {
    match behavior {
        StreamingBehavior::Steer => "steer",
        StreamingBehavior::FollowUp => "followUp",
    }
}

pub(super) struct Hooks {
    pub(super) session: AgentSession,
    pub(super) steering_mode: Option<QueueMode>,
    pub(super) follow_up_mode: Option<QueueMode>,
    pub(super) cancel: CancellationToken,
    /// pi's turn index for extension events: turns since `agent_start`.
    pub(super) turn_index: std::sync::atomic::AtomicU64,
}

fn ui_prompt_event(kind_of_event: &str, kind: &str, title: Option<String>) -> Value {
    let event = serde_json::json!({"type": kind_of_event, "reason": "ui_prompt", "kind": kind, "title": title});
    defined(event, &["title"])
}

/// `event` without the fields in `keys` that are null, which pi leaves
/// undefined.
pub(super) fn defined(mut event: Value, keys: &[&str]) -> Value {
    if let Some(object) = event.as_object_mut() {
        object.retain(|key, value| !(value.is_null() && keys.contains(&key.as_str())));
    }
    event
}

/// A loop event as pi's extension event (`_emitExtensionEvent` in pi's agent
/// session), when `wanted` takes its type; `None` for events extensions do
/// not see.
fn extension_event(
    event: &AgentEvent,
    turn_index: u64,
    wanted: impl Fn(&str) -> bool,
) -> Option<Value> {
    let (kind, keys): (&str, &[&str]) = match event {
        AgentEvent::AgentStart => ("agent_start", &[]),
        AgentEvent::AgentEnd { .. } => ("agent_end", &["messages"]),
        AgentEvent::TurnStart => ("turn_start", &[]),
        // `turn_end` reaches extensions as a boundary, before the event.
        AgentEvent::MessageStart { .. } => ("message_start", &["message"]),
        AgentEvent::MessageUpdate { .. } => {
            ("message_update", &["message", "assistantMessageEvent"])
        }
        // `message_end` reaches extensions as the message is finished.
        AgentEvent::ToolExecutionStart { .. } => (
            "tool_execution_start",
            &["toolCallId", "toolName", "args", "parentToolCallId"],
        ),
        AgentEvent::ToolExecutionUpdate { .. } => (
            "tool_execution_update",
            &[
                "toolCallId",
                "toolName",
                "args",
                "partialResult",
                "parentToolCallId",
            ],
        ),
        AgentEvent::ToolExecutionEnd { .. } => (
            "tool_execution_end",
            &[
                "toolCallId",
                "toolName",
                "result",
                "isError",
                "parentToolCallId",
            ],
        ),
        _ => return None,
    };
    if !wanted(kind) {
        return None;
    }
    let value = serde_json::to_value(event).ok()?;
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String(kind.to_owned()));
    if kind == "turn_start" {
        out.insert("turnIndex".into(), Value::from(turn_index));
        out.insert("timestamp".into(), Value::from(now_ms()));
    }
    for key in keys {
        if let Some(field) = value.get(*key) {
            out.insert((*key).to_owned(), field.clone());
        }
    }
    Some(Value::Object(out))
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
                extension_event(event, turn_index, |kind| session.has_handlers(kind))
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
            let session = &self.session;
            let mut messages = session
                .context_handlers(messages, self.cancel.clone())
                .await;
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
                    .tool_call(&ctx, &call.tool_call.name, &*call.args)
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
            // Handlers change the call's arguments by editing `event.input`,
            // which later handlers see, as in pi.
            for extension in self.session.handlers_of("tool_call") {
                let Some(result) = extension.handle(&ctx, &event).await else {
                    continue;
                };
                if let Some(input) = result.get("input").filter(|input| input.is_object()) {
                    event["input"] = input.clone();
                }
                if result["block"] == true {
                    return Some(yapi_agent::hooks::Block {
                        reason: result["reason"].as_str().map(str::to_owned),
                        terminate: result["terminate"] == true,
                    });
                }
            }
            *call.args = event["input"].take();
            None
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        call: yapi_agent::hooks::AfterToolCall<'a>,
    ) -> BoxFuture<'a, Option<yapi_agent::hooks::ResultPatch>> {
        Box::pin(async move {
            let mut patch = self.tool_result_handlers(&call).await;
            // After the handlers, so images they add are normalized too.
            let content = patch
                .as_ref()
                .and_then(|patch| patch.content.as_ref())
                .unwrap_or(&call.result.content);
            if content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
            {
                let content = content.clone();
                let (auto_resize, limits) = self.session.image_options();
                let normalized = tokio::task::spawn_blocking(move || {
                    crate::images::normalize_tool_result(&content, auto_resize, limits.as_ref())
                })
                .await
                .ok()
                .flatten();
                if let Some(content) = normalized {
                    patch.get_or_insert_default().content = Some(content);
                }
            }
            patch
        })
    }

    fn finish_turn<'a>(
        &'a self,
        message: &'a yapi_types::message::AssistantMessage,
        tool_results: &'a [ToolResultMessage],
    ) -> BoxFuture<'a, yapi_agent::hooks::TurnDecision> {
        Box::pin(async move {
            let turn_index = self.turn_index.load(Ordering::SeqCst);
            self.session
                .turn_end_boundary(message, tool_results, turn_index, self.cancel.clone())
                .await
        })
    }

    fn finish_message(&self, message: Message) -> BoxFuture<'_, Message> {
        Box::pin(async move {
            self.session
                .message_end_handlers(message, self.cancel.clone())
                .await
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

impl Hooks {
    /// pi's `tool_result` event: the changes its handlers make, if any.
    async fn tool_result_handlers(
        &self,
        call: &yapi_agent::hooks::AfterToolCall<'_>,
    ) -> Option<yapi_agent::hooks::ResultPatch> {
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
    }
}

impl AgentSession {
    /// The context extensions get, with `cancel` for the operation at hand.
    pub(super) fn extension_context(&self, cancel: CancellationToken) -> Context {
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

    /// The UI and mode extensions are bound to.
    pub fn extension_binding(&self) -> (Arc<dyn ExtensionUi>, Mode) {
        lock(&self.inner.binding).clone()
    }

    /// The extensions with handlers for pi events of type `kind`.
    pub(super) fn handlers_of(&self, kind: &str) -> Vec<&Arc<dyn Extension>> {
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
    /// order, as pi's runner `emit` does. Only a `session_before_*` event has
    /// a result: the last one a handler gave, or the first that cancels,
    /// which stops delivery.
    pub async fn emit_extension_event(
        &self,
        event: &Value,
        cancel: CancellationToken,
    ) -> Option<Value> {
        let kind = event["type"].as_str().unwrap_or_default();
        let handlers = self.handlers_of(kind);
        if handlers.is_empty() {
            return None;
        }
        let ctx = self.extension_context(cancel);
        let before = kind.starts_with("session_before_");
        let mut result = None;
        for extension in handlers {
            if let Some(next) = extension.handle(&ctx, event).await
                && before
            {
                let cancelled = next["cancel"] == true;
                result = Some(next);
                if cancelled {
                    break;
                }
            }
        }
        result
    }

    /// pi's `session_before_switch` or `session_before_fork`: whether an
    /// extension cancels `change`.
    pub async fn cancels(&self, change: &SessionChange) -> bool {
        let event = match change {
            SessionChange::New => {
                serde_json::json!({"type": change.event(), "reason": change.reason().as_str()})
            }
            SessionChange::Resume(target) => serde_json::json!({
                "type": change.event(),
                "reason": change.reason().as_str(),
                "targetSessionFile": target,
            }),
            SessionChange::Fork { entry_id, at } => serde_json::json!({
                "type": change.event(),
                "entryId": entry_id,
                "position": if *at { "at" } else { "before" },
            }),
        };
        let result = self
            .emit_extension_event(&event, CancellationToken::new())
            .await;
        result.is_some_and(|result| result["cancel"] == true)
    }

    /// pi's `message_end`: the message as handlers replaced it, each seeing
    /// the previous one's. A replacement of another role is reported and
    /// skipped, and a missing content of a user, assistant, tool result or
    /// custom message becomes empty.
    async fn message_end_handlers(&self, message: Message, cancel: CancellationToken) -> Message {
        let handlers = self.handlers_of("message_end");
        if handlers.is_empty() {
            return message;
        }
        let ctx = self.extension_context(cancel);
        let report = |extension: &Arc<dyn Extension>, error: &str| {
            ctx.ui
                .extension_error(&extension.source().path, "message_end", error, None);
        };
        let mut current = serde_json::json!(message);
        let mut replaced_by = None;
        for extension in handlers {
            let event = serde_json::json!({"type": "message_end", "message": current});
            let Some(next) = extension
                .handle(&ctx, &event)
                .await
                .and_then(|mut result| result.get_mut("message").map(Value::take))
                .filter(|next| !next.is_null())
            else {
                continue;
            };
            if next["role"] != current["role"] {
                report(
                    extension,
                    "message_end handlers must return a message with the same role",
                );
                continue;
            }
            current = next;
            replaced_by = Some(extension);
        }
        let Some(extension) = replaced_by else {
            return message;
        };
        if matches!(
            current["role"].as_str(),
            Some("user" | "assistant" | "toolResult" | "custom")
        ) && current["content"].is_null()
        {
            current["content"] = Value::Array(Vec::new());
        }
        serde_json::from_value(current).unwrap_or_else(|error| {
            report(extension, &error.to_string());
            message
        })
    }

    /// Passes `value` through the extensions handling `kind` as the event's
    /// `field`, each seeing the `field` the previous one returned.
    pub(super) async fn chain(
        &self,
        kind: &str,
        field: &str,
        value: Value,
        cancel: CancellationToken,
    ) -> Value {
        let handlers = self.handlers_of(kind);
        if handlers.is_empty() {
            return value;
        }
        let ctx = self.extension_context(cancel);
        let mut value = value;
        for extension in handlers {
            let event = serde_json::json!({"type": kind, field: value});
            if let Some(result) = extension.handle(&ctx, &event).await
                && let Some(next) = result.get(field)
            {
                value = next.clone();
            }
        }
        value
    }

    /// pi's `emitContext`: `context` handlers see the conversation without
    /// system messages, which come back as the current prompt when they
    /// change it; `context_with_system` handlers then see and return the
    /// whole transcript.
    async fn context_handlers(
        &self,
        messages: Vec<Message>,
        cancel: CancellationToken,
    ) -> Vec<Message> {
        let mut messages = messages;
        let ctx = self.extension_context(cancel);
        for extension in self.handlers_of("context") {
            let visible: Vec<Message> = messages
                .iter()
                .filter(|message| !matches!(message, Message::System(_)))
                .cloned()
                .collect();
            let event = serde_json::json!({"type": "context", "messages": visible});
            if let Some(result) = extension.handle(&ctx, &event).await
                && let Ok(returned) =
                    serde_json::from_value::<Vec<Message>>(result["messages"].clone())
                && returned != visible
            {
                let head = yapi_ai::transcript::current_system_message(&messages);
                messages = head
                    .map(Message::System)
                    .into_iter()
                    .chain(returned)
                    .collect();
            }
        }
        for extension in self.handlers_of("context_with_system") {
            let had_head = matches!(messages.first(), Some(Message::System(_)));
            let event = serde_json::json!({"type": "context_with_system", "messages": messages});
            if let Some(result) = extension.handle(&ctx, &event).await
                && let Ok(returned) = serde_json::from_value(result["messages"].clone())
            {
                messages = returned;
            }
            if had_head && !matches!(messages.first(), Some(Message::System(_))) {
                ctx.ui.extension_error(
                    &extension.source().path,
                    "context_with_system",
                    "Handler removed the leading system message; the request has no prompt or initial tool declarations. Keep it at index 0 or replace a dropped prefix with getCurrentSystemMessage().",
                    None,
                );
            }
        }
        messages
    }

    /// pi's `before_provider_headers`: the headers after handlers changed
    /// them in place; a null value removes a header.
    pub(super) async fn before_provider_headers(
        &self,
        headers: yapi_ai::stream::RequestHeaders,
        cancel: CancellationToken,
    ) -> yapi_ai::stream::RequestHeaders {
        let value = serde_json::to_value(&headers).unwrap_or_default();
        let value = self
            .chain("before_provider_headers", "headers", value, cancel)
            .await;
        match value.as_object() {
            Some(headers) => headers
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().map(str::to_owned)))
                .collect(),
            None => headers,
        }
    }

    /// A blocking extension dialog of `kind` (`select`, `confirm`, `input`,
    /// `editor` or `custom`) with `title` opens; extensions hear of the
    /// outermost as pi's `ui_prompt_start`.
    pub fn ui_prompt_opened(&self, kind: &str, title: Option<&str>) {
        let title = title.filter(|title| !title.is_empty()).map(str::to_owned);
        {
            let mut prompts = lock(&self.inner.ui_prompts);
            prompts.0 += 1;
            if prompts.0 > 1 {
                return;
            }
            prompts.1 = Some((kind.to_owned(), title.clone()));
        }
        self.announce(ui_prompt_event("ui_prompt_start", kind, title));
    }

    /// A blocking extension dialog closed; when it was the outermost,
    /// extensions hear of it as pi's `ui_prompt_end`.
    pub fn ui_prompt_closed(&self) {
        let (kind, title) = {
            let mut prompts = lock(&self.inner.ui_prompts);
            prompts.0 = prompts.0.saturating_sub(1);
            if prompts.0 > 0 {
                return;
            }
            match prompts.1.take() {
                Some(prompt) => prompt,
                None => return,
            }
        };
        self.announce(ui_prompt_event("ui_prompt_end", &kind, title));
    }

    /// Delivers `event` to extensions in the background, after the events
    /// announced before it, as pi's runner delivers events it does not wait
    /// for. Outside a tokio runtime it waits for the next
    /// [`AgentSession::flush_announcements`].
    pub(super) fn announce(&self, event: Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        if !self.has_handlers(kind) {
            return;
        }
        lock(&self.inner.announcements).push_back(event);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let session = self.clone();
            runtime.spawn(async move { session.flush_announcements().await });
        }
    }

    /// Waits until the events announced so far have reached extensions.
    pub async fn flush_announcements(&self) {
        let _turn = self.inner.announcing.lock().await;
        loop {
            let next = lock(&self.inner.announcements).pop_front();
            let Some(event) = next else {
                return;
            };
            self.emit_extension_event(&event, CancellationToken::new())
                .await;
        }
    }

    /// pi's `input` event: `None` when a handler handled the input,
    /// otherwise the possibly transformed text and images.
    pub(super) async fn input_handlers(
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
    pub(super) async fn before_agent_start_handlers(
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

    /// Sets how the mode carries out session changes extension commands
    /// ask for.
    pub fn set_actions(&self, actions: crate::extensions::SessionActions) {
        *lock(&self.inner.actions) = Some(actions);
    }

    /// Carries out a session change an extension command asks for: whether
    /// an extension cancelled it. Without a mode, nothing changes, as pi's
    /// default actions do.
    pub async fn session_action(
        &self,
        action: crate::extensions::SessionAction,
    ) -> Result<bool, String> {
        let actions = lock(&self.inner.actions).clone();
        match actions {
            Some(actions) => actions(action).await,
            None => Ok(false),
        }
    }

    /// Gives extensions their UI and mode and starts them: pi's
    /// `bindExtensions`, which emits `session_start` with the reason this
    /// session replaced another and that session's file, or `startup` when
    /// `replaced` is `None`.
    pub async fn bind_extensions(
        &self,
        ui: Arc<dyn ExtensionUi>,
        mode: Mode,
        replaced: Option<(Replacement, Option<String>)>,
    ) {
        *lock(&self.inner.binding) = (ui, mode);
        let ctx = self.extension_context(CancellationToken::new());
        for extension in &self.inner.extensions {
            extension.session_start(&ctx).await;
        }
        let mut event = serde_json::json!({"type": "session_start", "reason": "startup"});
        let mut reload = false;
        if let Some((reason, previous)) = replaced {
            event["reason"] = reason.as_str().into();
            if let Some(previous) = reason.reported(previous) {
                event["previousSessionFile"] = previous.into();
            }
            reload = reason == Replacement::Reload;
        }
        self.emit_extension_event(&event, CancellationToken::new())
            .await;
        self.discover_resources(if reload { "reload" } else { "startup" })
            .await;
    }

    /// pi's `resources_discover`, after `session_start`: adds the skills,
    /// prompt templates and themes extensions name, each with its extension
    /// as source.
    async fn discover_resources(&self, reason: &str) {
        let handlers = self.handlers_of("resources_discover");
        if handlers.is_empty() {
            return;
        }
        let event = serde_json::json!({"type": "resources_discover", "cwd": self.inner.cwd, "reason": reason});
        let ctx = self.extension_context(CancellationToken::new());
        let mut found: [Vec<SourceInfo>; 3] = Default::default();
        for extension in handlers {
            let Some(result) = extension.handle(&ctx, &event).await else {
                continue;
            };
            let origin = extension.source().path;
            let synthetic = origin.starts_with("builtin:") || origin.starts_with('<');
            let source = if synthetic {
                format!("extension:{}", origin.replace(['<', '>'], ""))
            } else {
                let name = std::path::Path::new(&origin)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let name = name
                    .strip_suffix(".ts")
                    .or_else(|| name.strip_suffix(".js"))
                    .unwrap_or(&name);
                format!("extension:{name}")
            };
            let base_dir = (!synthetic)
                .then(|| std::path::Path::new(&origin).parent())
                .flatten()
                .map(|dir| dir.display().to_string());
            for (kind, paths) in ["skillPaths", "promptPaths", "themePaths"]
                .iter()
                .zip(found.iter_mut())
            {
                for path in result[*kind].as_array().into_iter().flatten() {
                    let Some(path) = path.as_str() else { continue };
                    let path = crate::tools::path::resolve_to_cwd(path.trim(), &self.inner.cwd);
                    paths.push(SourceInfo {
                        path: path.display().to_string(),
                        source: source.clone(),
                        scope: "temporary".into(),
                        origin: "top-level".into(),
                        base_dir: base_dir.clone(),
                    });
                }
            }
        }
        // pi's `extendResources`: each kind extensions name paths for loads
        // again from its paths, merged with those it had.
        let [skills, prompts, themes] = found;
        let merged = |known: &mut Vec<SourceInfo>, found: Vec<SourceInfo>| {
            crate::resources::merge_sources(std::mem::take(known).into_iter().chain(found))
        };
        let mut resources = write(&self.inner.resources);
        let resources = &mut *resources;
        if !skills.is_empty() {
            resources.skill_sources = merged(&mut resources.skill_sources, skills);
            (resources.skills, resources.skill_diagnostics) =
                crate::resources::skills_from(&resources.skill_sources);
        }
        if !prompts.is_empty() {
            resources.template_sources = merged(&mut resources.template_sources, prompts);
            (resources.templates, resources.template_diagnostics) =
                crate::resources::templates_from(&resources.template_sources);
        }
        if !themes.is_empty() {
            resources.themes = merged(&mut resources.themes, themes);
        }
    }

    /// Stops extensions before the session ends: pi's `session_shutdown`
    /// with the reason `quit`.
    pub async fn shutdown(&self) {
        self.stop_extensions(serde_json::json!({"type": "session_shutdown", "reason": "quit"}))
            .await;
    }

    /// Stops extensions before a session replaces this one for `reason`;
    /// `target` is the replacement's session file, which pi does not report
    /// on reload.
    pub async fn shutdown_for(&self, reason: Replacement, target: Option<String>) {
        let mut event = serde_json::json!({"type": "session_shutdown", "reason": reason.as_str()});
        if let Some(target) = reason.reported(target) {
            event["targetSessionFile"] = target.into();
        }
        self.stop_extensions(event).await;
    }

    async fn stop_extensions(&self, event: Value) {
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
    pub(super) async fn run_extension_command(&self, text: &str) -> bool {
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
}
