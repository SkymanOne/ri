//! `mistral-conversations`: Mistral's native chat completions endpoint.
//!
//! Port of `packages/ai/src/api/mistral-conversations.ts` in pi `v1.0.0`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, ThinkingContent, ThinkingLevel,
    ToolCall, ToolDeclaration,
};
use yapi_types::model::Model;

use crate::cost::calculate_cost;
use crate::hash::short_hash;
use crate::http::{self, Headers};
use crate::json_parse::parse_streaming_json;
use crate::schema;
use crate::stream::{
    CacheRetention, EventSender, Request, StreamEvent, StreamOptions, check_complete, new_output,
    now_ms, send_error,
};
use crate::thinking::{effort, requested_max_tokens};
use crate::transcript::{current_tools, resolve_transcript, transform_messages};

const TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_ERROR_BODY_CHARS: usize = 4000;
/// pi's default `timeoutMs`; it bounds the whole request, stream included.
const TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT_MESSAGE: &str = "The operation was aborted due to timeout";

/// A Mistral tool-call id for `id`: nine alphanumerics, from the id itself when
/// it already is one, else from its hash.
fn derive_tool_call_id(id: &str, attempt: usize) -> String {
    let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if attempt == 0 && normalized.chars().count() == TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        base.to_owned()
    } else {
        format!("{base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(TOOL_CALL_ID_LENGTH)
        .collect()
}

/// pi's `createMistralToolCallIdNormalizer`: stable, collision-free ids.
#[derive(Default)]
struct IdNormalizer {
    ids: HashMap<String, String>,
    owners: HashMap<String, String>,
}

impl IdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.ids.get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_tool_call_id(id, attempt);
            if self.owners.get(&candidate).is_none_or(|owner| owner == id) {
                self.ids.insert(id.to_owned(), candidate.clone());
                self.owners.insert(candidate.clone(), id.to_owned());
                return candidate;
            }
            attempt += 1;
        }
    }
}

fn image_url(mime_type: &str, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

/// pi's `buildToolResultText`.
fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let trimmed = text.trim();
    let prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{prefix}{trimmed}{suffix}");
    }
    if has_images {
        let body = if supports_images {
            "(see attached image)"
        } else {
            "(image omitted: model does not support images)"
        };
        return format!("{prefix}{body}");
    }
    format!("{prefix}(no tool output)")
}

/// Messages in Mistral's wire shape. Keys follow pi's: renamed fields move to
/// the end of their object.
fn chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => {
                let text = if index == 0 {
                    system.text()
                } else {
                    system.render_update()
                };
                if !text.is_empty() {
                    out.push(json!({"role": "system", "content": text}));
                }
            }
            Message::User(user) => match &user.content {
                Content::Text(text) => out.push(json!({"role": "user", "content": text})),
                Content::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Image(_)));
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => {
                                Some(json!({"type": "text", "text": text.text}))
                            }
                            ContentBlock::Image(image) if supports_images => Some(json!({
                                "type": "image_url",
                                "image_url": image_url(&image.mime_type, &image.data),
                            })),
                            _ => None,
                        })
                        .collect();
                    if !content.is_empty() {
                        out.push(json!({"role": "user", "content": content}));
                    } else if had_images && !supports_images {
                        out.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut content = Vec::new();
                let mut calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                            content.push(json!({"type": "text", "text": text.text}));
                        }
                        ContentBlock::Thinking(thinking) if !thinking.thinking.trim().is_empty() => {
                            content.push(json!({
                                "type": "thinking",
                                "thinking": [{"type": "text", "text": thinking.thinking}],
                            }));
                        }
                        ContentBlock::ToolCall(call) => calls.push(json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": yapi_types::json::to_string(&call.arguments).unwrap_or_default(),
                            },
                            "index": 0,
                        })),
                        _ => {}
                    }
                }
                if content.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut item = Map::new();
                item.insert("role".into(), json!("assistant"));
                item.insert("prefix".into(), json!(false));
                if !content.is_empty() {
                    item.insert("content".into(), Value::Array(content));
                }
                if !calls.is_empty() {
                    item.insert("tool_calls".into(), Value::Array(calls));
                }
                out.push(Value::Object(item));
            }
            Message::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image(_)));
                let mut content = vec![json!({
                    "type": "text",
                    "text": tool_result_text(&text, has_images, supports_images, result.is_error),
                })];
                if supports_images {
                    for block in &result.content {
                        if let ContentBlock::Image(image) = block {
                            content.push(json!({
                                "type": "image_url",
                                "image_url": image_url(&image.mime_type, &image.data),
                            }));
                        }
                    }
                }
                out.push(json!({
                    "role": "tool",
                    "name": result.tool_name,
                    "content": content,
                    "tool_call_id": result.tool_call_id,
                }));
            }
            _ => {}
        }
    }
    out
}

fn function_tools(tools: &[ToolDeclaration]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = schema::strict_sampling(tool, true, None)?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": schema::tool_parameters(tool, strict),
                    "strict": strict,
                },
            }))
        })
        .collect()
}

/// pi's `streamSimple` reasoning: models with a thinking level map send
/// `reasoning_effort`; other reasoning models send `prompt_mode`.
fn reasoning(
    model: &Model,
    level: Option<ThinkingLevel>,
) -> (Option<String>, Option<&'static str>) {
    if !model.reasoning {
        return (None, None);
    }
    if model.thinking_level_map.is_some() {
        let effort = match level {
            Some(level) => match model.thinking_level_value(level) {
                Some(Some(value)) => Some(value.to_owned()),
                _ => Some("high".to_owned()),
            },
            None => model
                .thinking_level_value(ThinkingLevel::Off)
                .flatten()
                .map(str::to_owned),
        };
        return (effort, None);
    }
    (None, level.map(|_| "reasoning"))
}

fn uses_prompt_cache(options: &StreamOptions) -> Option<&str> {
    options
        .session_id
        .as_deref()
        .filter(|_| options.resolved_cache_retention() != CacheRetention::None)
        .filter(|session| !session.is_empty())
}

fn build_payload(
    model: &Model,
    normalized: &[Message],
    transformed: &[Message],
    options: &StreamOptions,
    max_tokens: u64,
    level: Option<ThinkingLevel>,
) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("stream".into(), json!(true));
    payload.insert(
        "messages".into(),
        Value::Array(chat_messages(transformed, model.accepts_images())),
    );
    let tools = current_tools(normalized);
    if !tools.is_empty() {
        payload.insert("tools".into(), Value::Array(function_tools(&tools)?));
    }
    if let Some(temperature) = options.temperature {
        payload.insert("temperature".into(), json!(temperature));
    }
    payload.insert("max_tokens".into(), json!(max_tokens));
    let (effort, mode) = reasoning(model, level);
    if let Some(effort) = effort {
        payload.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(mode) = mode {
        payload.insert("prompt_mode".into(), json!(mode));
    }
    if let Some(session) = uses_prompt_cache(options) {
        payload.insert("prompt_cache_key".into(), json!(session));
    }
    Ok(Value::Object(payload))
}

/// The events of a Mistral stream: blank-line separated blocks of `data:` lines.
#[derive(Default)]
struct EventParser {
    buffer: String,
}

/// One parsed event.
enum Parsed {
    Event(Value),
    Done,
}

fn boundary(buffer: &str) -> Option<(usize, usize)> {
    const SEPARATORS: [&str; 8] = [
        "\r\n\r\n", "\r\n\r", "\r\n\n", "\r\r\n", "\n\r\n", "\r\r", "\n\r", "\n\n",
    ];
    // The earliest match wins; at one position, the regex alternation order does.
    let bytes = buffer.as_bytes();
    (0..bytes.len()).find_map(|index| {
        SEPARATORS
            .iter()
            .find(|separator| buffer[index..].starts_with(*separator))
            .map(|separator| (index, separator.len()))
    })
}

fn parse_event(raw: &str) -> Result<Option<Parsed>, String> {
    let data = raw
        .split(['\r', '\n'])
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    let data = data.trim();
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(Some(Parsed::Done));
    }
    let parsed: Value = serde_json::from_str(data).map_err(|err| err.to_string())?;
    if !parsed["choices"].is_array() {
        return Err("Invalid Mistral streaming event".into());
    }
    Ok(Some(Parsed::Event(parsed)))
}

impl EventParser {
    /// Events completed by `text`; `Done` ends the stream.
    fn push(&mut self, text: &str) -> Result<Vec<Parsed>, String> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        while let Some((index, length)) = boundary(&self.buffer) {
            let raw = self.buffer[..index].to_owned();
            self.buffer.drain(..index + length);
            if let Some(event) = parse_event(&raw)? {
                let done = matches!(event, Parsed::Done);
                events.push(event);
                if done {
                    break;
                }
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Option<Parsed>, String> {
        if self.buffer.trim().is_empty() {
            return Ok(None);
        }
        parse_event(&std::mem::take(&mut self.buffer))
    }
}

/// pi's `mapChatStopReason`.
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

/// The open text or thinking block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Current {
    Text,
    Thinking,
}

struct State {
    output: AssistantMessage,
    current: Option<Current>,
    /// Tool-call blocks by stream index or id, in order of appearance.
    tools: IndexMap<String, usize>,
    partial_args: HashMap<usize, String>,
}

impl State {
    fn last(&self) -> usize {
        self.output.content.len().saturating_sub(1)
    }

    fn finish_current(&mut self, sender: &EventSender) {
        if self.current.take().is_some() {
            sender.end(&self.output, self.last());
        }
    }

    /// Appends a text or thinking delta, opening a block of that kind first.
    fn append(&mut self, kind: Current, delta: &str, sender: &EventSender) {
        // GLM models send empty deltas around thinking and tool calls.
        if delta.is_empty() {
            return;
        }
        if self.current != Some(kind) {
            self.finish_current(sender);
            self.current = Some(kind);
            let block = match kind {
                Current::Text => ContentBlock::text(""),
                Current::Thinking => ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }),
            };
            sender.start(&mut self.output, block);
        }
        let index = self.last();
        sender.delta(&mut self.output, index, delta);
    }

    fn tool_call(&mut self, call: &Value, sender: &EventSender) {
        self.finish_current(sender);
        let stream_index = call["index"].as_u64();
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty() && *id != "null")
            .map_or_else(
                || derive_tool_call_id(&format!("toolcall:{}", stream_index.unwrap_or(0)), 0),
                str::to_owned,
            );
        let key = stream_index.map_or_else(|| format!("id:{id}"), |index| format!("index:{index}"));
        let position = match self.tools.get(&key) {
            Some(position) => *position,
            None => {
                let name = call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let block = ContentBlock::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: Map::new(),
                    thought_signature: None,
                    namespace: None,
                });
                let position = sender.start(&mut self.output, block);
                self.tools.insert(key, position);
                self.partial_args.insert(position, String::new());
                position
            }
        };
        let arguments = &call["function"]["arguments"];
        let delta = match arguments {
            Value::String(text) => text.clone(),
            Value::Null => "{}".to_owned(),
            other => yapi_types::json::to_string(other).unwrap_or_default(),
        };
        let partial = self.partial_args.entry(position).or_default();
        partial.push_str(&delta);
        let parsed = parse_streaming_json(partial);
        if let Some(ContentBlock::ToolCall(block)) = self.output.content.get_mut(position) {
            block.arguments = parsed;
        }
        sender.delta(&mut self.output, position, &delta);
    }

    fn handle(&mut self, chunk: &Value, model: &Model, sender: &EventSender) {
        if self.output.response_id.as_deref().is_none_or(str::is_empty)
            && let Some(id) = chunk["id"].as_str()
        {
            self.output.response_id = Some(id.to_owned());
        }
        let usage = &chunk["usage"];
        if usage.is_object() {
            let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = [
                &usage["promptTokensDetails"]["cachedTokens"],
                &usage["prompt_tokens_details"]["cached_tokens"],
                &usage["promptTokenDetails"]["cachedTokens"],
                &usage["prompt_token_details"]["cached_tokens"],
                &usage["numCachedTokens"],
                &usage["num_cached_tokens"],
            ]
            .into_iter()
            .find(|value| !value.is_null())
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map_or(0, |value| (value.max(0.0) as u64).min(prompt));
            let usage_out = &mut self.output.usage;
            usage_out.input = prompt - cached;
            usage_out.output = usage["completion_tokens"].as_u64().unwrap_or(0);
            usage_out.cache_read = cached;
            usage_out.cache_write = 0;
            usage_out.total_tokens = Some(
                usage["total_tokens"]
                    .as_u64()
                    .filter(|total| *total > 0)
                    .unwrap_or(usage_out.input + usage_out.output + usage_out.cache_read),
            );
            calculate_cost(model, &mut self.output.usage);
        }
        let Some(choice) = chunk["choices"].get(0) else {
            return;
        };
        if let Some(reason) = choice["finish_reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_owned());
            let (stop, message) = map_stop_reason(reason);
            self.output.stop_reason = stop;
            if message.is_some() {
                self.output.error_message = message;
            }
        }
        let delta = &choice["delta"];
        match &delta["content"] {
            Value::String(text) => self.append(Current::Text, text, sender),
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::String(text) => self.append(Current::Text, text, sender),
                        _ if item["type"] == "thinking" => {
                            let text: String = item["thinking"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|part| part["text"].as_str())
                                .collect();
                            self.append(Current::Thinking, &text, sender);
                        }
                        _ if item["type"] == "text" => {
                            let text = item["text"].as_str().unwrap_or_default();
                            self.append(Current::Text, text, sender);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            self.tool_call(call, sender);
        }
    }

    fn finish(&mut self, sender: &EventSender) {
        self.finish_current(sender);
        let positions: Vec<usize> = self.tools.values().copied().collect();
        for position in positions {
            let partial = self.partial_args.remove(&position).unwrap_or_default();
            let arguments = parse_streaming_json(&partial);
            if let Some(ContentBlock::ToolCall(block)) = self.output.content.get_mut(position) {
                block.arguments = arguments;
                sender.end(&self.output, position);
            }
        }
    }
}

/// Cancels `token` when the request has run for [`TIMEOUT`], recording that it did.
fn start_timeout(token: &CancellationToken) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let (token, flag) = (token.clone(), Arc::clone(&fired));
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(TIMEOUT) => {
                flag.store(true, Ordering::SeqCst);
                token.cancel();
            }
            () = token.cancelled() => {}
        }
    });
    fired
}

pub(super) async fn run(request: Request, sender: EventSender) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let output = new_output(&model, now_ms());
    let cancel = options.cancel.child_token();
    let timed_out = start_timeout(&cancel);
    let aborted = || {
        if timed_out.load(Ordering::SeqCst) {
            TIMEOUT_MESSAGE.to_owned()
        } else {
            http::ABORTED_READ.to_owned()
        }
    };
    let response = async {
        let compat: yapi_types::model::OpenAiResponsesCompat = model.compat();
        let normalized = resolve_transcript(
            &messages,
            compat.supports_mid_convo_system_messages.unwrap_or(false),
        );
        let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return Err(format!("No API key for provider: {}", model.provider));
        };
        let transformed = {
            let ids = RefCell::new(IdNormalizer::default());
            let normalize = |id: &str, _: &AssistantMessage| ids.borrow_mut().normalize(id);
            transform_messages(&normalized, &model, Some(&normalize), now_ms())
        };
        let max_tokens = requested_max_tokens(&model, &messages, &options);
        let level = effort(&model, &options);
        let payload = build_payload(
            &model,
            &normalized,
            &transformed,
            &options,
            max_tokens,
            level,
        )?;
        let payload = yapi_types::json::to_string(&payload).map_err(|err| err.to_string())?;

        let mut headers = Headers::default();
        headers.set("accept", Some("text/event-stream"));
        headers.set("authorization", Some(format!("Bearer {api_key}")));
        headers.set("content-type", Some("application/json"));
        headers.extend_model(model.headers.as_ref());
        headers.extend(&options.headers);
        let explicit_affinity = model
            .headers
            .iter()
            .flatten()
            .map(|(name, _)| name)
            .chain(options.headers.keys())
            .any(|name| name.eq_ignore_ascii_case("x-affinity"));
        if let Some(session) = uses_prompt_cache(&options)
            && !explicit_affinity
        {
            headers.set("x-affinity", Some(session));
        }

        let url = format!(
            "{}/v1/chat/completions",
            model.base_url.trim_end_matches('/')
        );
        let request = headers.apply(http::client().post(&url).body(payload));
        let response = tokio::select! {
            () = cancel.cancelled() => return Err(aborted()),
            response = request.send() => response.map_err(|_| "fetch failed".to_owned())?,
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = tokio::select! {
            () = cancel.cancelled() => return Err(aborted()),
            body = response.text() => body.unwrap_or_default(),
        };
        let body = body.trim();
        let text = if body.is_empty() {
            status.canonical_reason().map_or_else(
                || format!("Request failed with status {}", status.as_u16()),
                str::to_owned,
            )
        } else {
            http::truncate_chars(body, MAX_ERROR_BODY_CHARS)
        };
        Err(format!("Mistral API error ({}): {text}", status.as_u16()))
    }
    .await;
    let mut response = match response {
        Ok(response) => response,
        Err(message) => {
            cancel.cancel();
            return send_error(&sender, output, &options.cancel, message);
        }
    };

    sender.send(StreamEvent::Start(output.clone()));
    let mut state = State {
        output,
        current: None,
        tools: IndexMap::new(),
        partial_args: HashMap::new(),
    };
    let mut parser = EventParser::default();
    let mut pending = Vec::new();
    let result: Result<(), String> = 'stream: loop {
        let chunk = match http::read_chunk(&mut response, &cancel, http::ABORTED_READ).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => match parser.finish() {
                Ok(Some(Parsed::Event(event))) => {
                    state.handle(&event, &model, &sender);
                    break 'stream Ok(());
                }
                Ok(_) => break 'stream Ok(()),
                Err(message) => break 'stream Err(message),
            },
            Err(_) if cancel.is_cancelled() => break 'stream Err(aborted()),
            Err(message) => break 'stream Err(message),
        };
        let text = yapi_types::js::decode_utf8_stream(&mut pending, &chunk);
        match parser.push(&text) {
            Ok(events) => {
                for event in events {
                    match event {
                        Parsed::Event(event) => state.handle(&event, &model, &sender),
                        Parsed::Done => break 'stream Ok(()),
                    }
                }
            }
            Err(message) => break 'stream Err(message),
        }
    };
    cancel.cancel();
    let result = result.map(|()| state.finish(&sender)).and_then(|()| {
        check_complete(
            &state.output,
            &options.cancel,
            "Mistral stream ended without a finish reason",
        )
    });
    match result {
        Ok(()) => sender.send(StreamEvent::Done(state.output)),
        Err(message) => send_error(&sender, state.output, &options.cancel, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_tool_call_ids_like_pi() {
        assert_eq!(derive_tool_call_id("abcDEF123", 0), "abcDEF123");
        let mut ids = IdNormalizer::default();
        let first = ids.normalize("call_1|fc_long_item");
        assert_eq!(first.len(), 9);
        assert_eq!(ids.normalize("call_1|fc_long_item"), first);
        assert_ne!(ids.normalize("call_2|fc_other"), first);
    }

    #[test]
    fn splits_events_on_any_blank_line() {
        let mut parser = EventParser::default();
        let events = parser
            .push("data: {\"choices\":[]}\r\n\r\ndata: {\"choices\":[1]}\n\ndata: [DONE]\n\n")
            .unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[2], Parsed::Done));
        assert!(parser.push("data: {\"x\":1}\n\n").is_err());
    }
}
