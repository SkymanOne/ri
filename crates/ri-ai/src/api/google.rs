//! `google-generative-ai`: the Gemini API's `streamGenerateContent`.
//!
//! Port of `packages/ai/src/api/google-generative-ai.ts` and `google-shared.ts` in
//! pi `v1.0.0`, with the request and stream handling of `@google/genai` 2.21.

use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;
use ri_types::event::AssistantMessageEvent;
use ri_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolDeclaration, ToolResultMessage, Usage,
};
use ri_types::model::Model;
use serde_json::{Map, Value, json};

use super::sanitize_id_part;
use crate::cost::calculate_cost;
use crate::http::{self, Failure};
use crate::schema;
use crate::stream::{
    EventSender, EventStream, Provider, Request, StreamEvent, StreamOptions, ThinkingBudgets,
    new_output, now_ms, send_error,
};
use crate::thinking::{clamp_level, clamp_max_tokens_to_context};
use crate::transcript::{
    current_tools, initial_system_message, resolve_transcript, transform_messages,
};

/// The `google-generative-ai` wire API.
#[derive(Debug, Default)]
pub struct GoogleGenerativeAi;

impl Provider for GoogleGenerativeAi {
    fn api(&self) -> &str {
        "google-generative-ai"
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        tokio::spawn(run(request, sender));
        stream
    }
}

/// Message when a request is cancelled, as `fetch` reports it.
const ABORTED: &str = "This operation was aborted";

/// Thinking control for a request.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Thinking {
    Disabled,
    Level(&'static str),
    Budget(i64),
}

/// The Gemini major version of a model id, if it is a Gemini model.
fn gemini_major_version(id: &str) -> Option<u32> {
    let id = id.to_lowercase();
    let rest = id.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Models that need explicit ids on function calls and responses.
pub fn requires_tool_call_id(id: &str) -> bool {
    id.starts_with("claude-")
        || id.starts_with("gpt-oss-")
        || gemini_major_version(id).is_some_and(|major| major >= 3)
}

fn supports_multimodal_function_response(id: &str) -> bool {
    gemini_major_version(id).is_none_or(|major| major >= 3)
}

/// Gemini 3+ enforces required parameters in validated function calling.
fn supports_strict_tool_sampling(id: &str) -> bool {
    gemini_major_version(id).is_some_and(|major| major >= 3)
}

/// Whether the model takes Gemini's discrete `thinkingLevel` instead of a budget.
fn uses_thinking_level(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let gemini_3 = id.find("gemini-3").is_some_and(|start| {
        let rest = &id[start + "gemini-3".len()..];
        let rest = match rest.strip_prefix('.') {
            Some(minor) => {
                let digits = minor.chars().take_while(char::is_ascii_digit).count();
                if digits == 0 {
                    return false;
                }
                &minor[digits..]
            }
            None => rest,
        };
        rest.starts_with("-pro") || rest.starts_with("-flash")
    });
    gemini_3
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || id.contains("gemma-4")
        || id.contains("gemma4")
}

/// A supported level, or the model's mapping of it, as a standard Google level.
fn resolve_level(model: &Model, level: ThinkingLevel) -> Result<ThinkingLevel, String> {
    let mapped = model.thinking_level_value(level).flatten();
    let resolved = mapped.map_or_else(|| level.as_str().to_owned(), str::to_lowercase);
    match ThinkingLevel::parse(&resolved) {
        Some(
            level @ (ThinkingLevel::Minimal
            | ThinkingLevel::Low
            | ThinkingLevel::Medium
            | ThinkingLevel::High),
        ) => Ok(level),
        _ => Err(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
            model.provider,
            model.id,
            level.as_str(),
            match model.thinking_level_value(level) {
                Some(Some(value)) => value.to_owned(),
                Some(None) => "null".to_owned(),
                None => "undefined".to_owned(),
            }
        )),
    }
}

fn google_level(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "MINIMAL",
        ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium => "MEDIUM",
        _ => "HIGH",
    }
}

fn budget(model: &Model, level: ThinkingLevel, custom: &ThinkingBudgets) -> i64 {
    let custom = match level {
        ThinkingLevel::Minimal => custom.minimal,
        ThinkingLevel::Low => custom.low,
        ThinkingLevel::Medium => custom.medium,
        _ => custom.high,
    };
    if let Some(budget) = custom {
        return budget as i64;
    }
    let table: [i64; 4] = if model.id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if model.id.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576]
    } else if model.id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        ThinkingLevel::Minimal => table[0],
        ThinkingLevel::Low => table[1],
        ThinkingLevel::Medium => table[2],
        _ => table[3],
    }
}

/// pi's `streamSimple` thinking selection.
fn thinking(model: &Model, options: &StreamOptions) -> Result<Thinking, String> {
    let Some(level) = options.reasoning else {
        return Ok(Thinking::Disabled);
    };
    let clamped = clamp_level(model, level);
    if clamped == ThinkingLevel::Off {
        return Ok(Thinking::Disabled);
    }
    let resolved = resolve_level(model, clamped)?;
    Ok(if uses_thinking_level(model) {
        Thinking::Level(google_level(resolved))
    } else {
        Thinking::Budget(budget(model, resolved, &options.thinking_budgets))
    })
}

fn disabled_thinking_config(model: &Model) -> Result<Value, String> {
    if !uses_thinking_level(model) {
        return Ok(json!({"thinkingBudget": 0}));
    }
    let fallback = clamp_level(model, ThinkingLevel::Off);
    if fallback == ThinkingLevel::Off {
        return Ok(json!({"thinkingBudget": 0}));
    }
    Ok(json!({"thinkingLevel": google_level(resolve_level(model, fallback)?)}))
}

/// Thought signatures are base64 (TYPE_BYTES); others are dropped.
fn valid_signature(signature: Option<&str>) -> Option<&str> {
    signature.filter(|signature| {
        !signature.is_empty() && signature.len() % 4 == 0 && {
            let body = signature.trim_end_matches('=');
            signature.len() - body.len() <= 2
                && !body.is_empty()
                && body
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        }
    })
}

fn inline_data(mime_type: &str, data: &str) -> Value {
    json!({"inlineData": {"data": data, "mimeType": mime_type}})
}

fn part(text: Option<&str>, thought: bool, signature: Option<&str>) -> Value {
    let mut part = Map::new();
    if let Some(text) = text {
        part.insert("text".into(), json!(text));
    }
    if thought {
        part.insert("thought".into(), json!(true));
    }
    if let Some(signature) = signature {
        part.insert("thoughtSignature".into(), json!(signature));
    }
    Value::Object(part)
}

fn convert_assistant(assistant: &AssistantMessage, model: &Model) -> Vec<Value> {
    let same = assistant.provider == model.provider && assistant.model == model.id;
    let signature = |value: Option<&str>| {
        if same {
            valid_signature(value).map(str::to_owned)
        } else {
            None
        }
    };
    let mut parts = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Text(text) => {
                let signature = signature(text.text_signature.as_deref());
                if text.text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                parts.push(part(Some(&text.text), false, signature.as_deref()));
            }
            ContentBlock::Thinking(thinking) => {
                if same {
                    let signature = signature(thinking.thinking_signature.as_deref());
                    if thinking.thinking.trim().is_empty() && signature.is_none() {
                        continue;
                    }
                    parts.push(part(Some(&thinking.thinking), true, signature.as_deref()));
                } else if !thinking.thinking.trim().is_empty() {
                    parts.push(part(Some(&thinking.thinking), false, None));
                }
            }
            ContentBlock::ToolCall(call) => {
                let mut function_call = Map::new();
                function_call.insert("args".into(), Value::Object(call.arguments.clone()));
                if requires_tool_call_id(&model.id) {
                    function_call.insert("id".into(), json!(call.id));
                }
                function_call.insert("name".into(), json!(call.name));
                let mut value = Map::new();
                value.insert("functionCall".into(), Value::Object(function_call));
                if let Some(signature) = signature(call.thought_signature.as_deref()) {
                    value.insert("thoughtSignature".into(), json!(signature));
                }
                parts.push(Value::Object(value));
            }
            ContentBlock::Image(_) => {}
        }
    }
    parts
}

fn convert_tool_result(result: &ToolResultMessage, model: &Model, contents: &mut Vec<Value>) {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<Value> = if model.accepts_images() {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image(image) => Some(inline_data(&image.mime_type, &image.data)),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let multimodal = supports_multimodal_function_response(&model.id);
    let value = if !text.is_empty() {
        text
    } else if !images.is_empty() {
        "(see attached image)".to_owned()
    } else {
        String::new()
    };
    let mut response = Map::new();
    response.insert("name".into(), json!(result.tool_name));
    response.insert(
        "response".into(),
        if result.is_error {
            json!({"error": value})
        } else {
            json!({"output": value})
        },
    );
    if !images.is_empty() && multimodal {
        response.insert("parts".into(), Value::Array(images.clone()));
    }
    if requires_tool_call_id(&model.id) {
        response.insert("id".into(), json!(result.tool_call_id));
    }
    let function_response = json!({"functionResponse": response});
    let merged = contents.last_mut().is_some_and(|last| {
        let is_responses = last["role"] == "user"
            && last["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|p| p.get("functionResponse").is_some()));
        if is_responses && let Some(parts) = last["parts"].as_array_mut() {
            parts.push(function_response.clone());
        }
        is_responses
    });
    if !merged {
        contents.push(json!({"parts": [function_response], "role": "user"}));
    }
    if !images.is_empty() && !multimodal {
        let mut parts = vec![json!({"text": "Tool result image:"})];
        parts.extend(images);
        contents.push(json!({"parts": parts, "role": "user"}));
    }
}

/// Converts the conversation, without the leading prompt, to Gemini contents.
fn convert_messages(model: &Model, messages: &[Message]) -> Vec<Value> {
    let collapsed = resolve_transcript(messages, false);
    let conversation = match collapsed.first() {
        Some(Message::System(_)) => &collapsed[1..],
        _ => &collapsed[..],
    };
    let needs_ids = requires_tool_call_id(&model.id);
    let normalize = |id: &str, _: &AssistantMessage| {
        if needs_ids {
            sanitize_id_part(id).chars().take(64).collect()
        } else {
            id.to_owned()
        }
    };
    let transformed = transform_messages(conversation, model, Some(&normalize), now_ms());
    let mut contents: Vec<Value> = Vec::new();
    for message in &transformed {
        match message {
            Message::User(user) => {
                let parts: Vec<Value> = match &user.content {
                    Content::Text(text) => vec![json!({"text": text})],
                    Content::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(json!({"text": text.text})),
                            ContentBlock::Image(image) => {
                                Some(inline_data(&image.mime_type, &image.data))
                            }
                            _ => None,
                        })
                        .collect(),
                };
                if !parts.is_empty() {
                    contents.push(json!({"parts": parts, "role": "user"}));
                }
            }
            Message::Assistant(assistant) => {
                let parts = convert_assistant(assistant, model);
                if !parts.is_empty() {
                    contents.push(json!({"parts": parts, "role": "model"}));
                }
            }
            Message::ToolResult(result) => convert_tool_result(result, model, &mut contents),
            _ => {}
        }
    }
    contents
}

fn convert_tools(tools: &[ToolDeclaration], strict_mode: bool) -> Result<Value, String> {
    let declarations = tools
        .iter()
        .map(|tool| {
            let strict = schema::strict_sampling(tool, strict_mode, None)?;
            Ok(json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": schema::tool_parameters(tool, strict),
            }))
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(json!([{"functionDeclarations": declarations}]))
}

fn build_body(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    max_tokens: u64,
    thinking: &Thinking,
) -> Result<Value, String> {
    let contents = convert_messages(model, messages);
    let collapsed = resolve_transcript(messages, false);
    let tools = current_tools(&collapsed);
    let strict_mode = supports_strict_tool_sampling(&model.id);
    let mut body = Map::new();
    body.insert("contents".into(), Value::Array(contents));
    let system = initial_system_message(&collapsed)
        .map(|system| system.text())
        .unwrap_or_default();
    if !system.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}], "role": "user"}),
        );
    }
    if !tools.is_empty() {
        body.insert("tools".into(), convert_tools(&tools, strict_mode)?);
        let validated = tools
            .iter()
            .map(|tool| schema::strict_sampling(tool, strict_mode, None))
            .collect::<Result<Vec<bool>, String>>()?
            .into_iter()
            .any(|strict| strict);
        if validated {
            body.insert(
                "toolConfig".into(),
                json!({"functionCallingConfig": {"mode": "VALIDATED"}}),
            );
        }
    }
    let mut generation = Map::new();
    if let Some(temperature) = options.temperature {
        generation.insert("temperature".into(), json!(temperature));
    }
    generation.insert("maxOutputTokens".into(), json!(max_tokens));
    if model.reasoning {
        let config = match thinking {
            Thinking::Level(level) => {
                Some(json!({"includeThoughts": true, "thinkingLevel": level}))
            }
            Thinking::Budget(budget) => {
                Some(json!({"includeThoughts": true, "thinkingBudget": budget}))
            }
            Thinking::Disabled => Some(disabled_thinking_config(model)?),
        };
        if let Some(config) = config {
            generation.insert("thinkingConfig".into(), config);
        }
    }
    body.insert("generationConfig".into(), Value::Object(generation));
    Ok(Value::Object(body))
}

fn model_path(id: &str) -> String {
    if id.starts_with("models/") || id.starts_with("tunedModels/") {
        id.to_owned()
    } else {
        format!("models/{id}")
    }
}

/// The SDK's error for a failed status: the JSON body, re-serialized.
fn status_message(status: u16, body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(json) => ri_types::json::to_string(&json).unwrap_or_default(),
        Err(_) => {
            let reason = reqwest::StatusCode::from_u16(status)
                .ok()
                .and_then(|code| code.canonical_reason())
                .unwrap_or("");
            let error = json!({"error": {"message": body, "code": status, "status": reason}});
            ri_types::json::to_string(&error).unwrap_or_default()
        }
    }
}

fn failure_message(failure: Failure) -> String {
    match failure {
        Failure::Status { status, body } => status_message(status, &body),
        Failure::Connection(_) => "fetch failed".to_owned(),
        Failure::Aborted => ABORTED.to_owned(),
        Failure::RetryDelay(message) => message,
    }
}

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The current text or thinking block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
}

struct State {
    output: AssistantMessage,
    open: Option<Open>,
}

fn map_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "STOP" => Ok(StopReason::Stop),
        "MAX_TOKENS" => Ok(StopReason::Length),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "NO_IMAGE" => Ok(StopReason::Error),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

impl State {
    fn last(&self) -> usize {
        self.output.content.len().saturating_sub(1)
    }

    fn close(&mut self, sender: &EventSender) {
        let Some(open) = self.open.take() else {
            return;
        };
        let index = self.last();
        let event = match (&self.output.content[index], open) {
            (ContentBlock::Text(text), Open::Text) => AssistantMessageEvent::TextEnd {
                content_index: index,
                content: text.text.clone(),
            },
            (ContentBlock::Thinking(thinking), Open::Thinking) => {
                AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.thinking.clone(),
                }
            }
            _ => return,
        };
        sender.update(&self.output, event);
    }

    fn text_part(&mut self, part: &Value, text: &str, sender: &EventSender) {
        let kind = if part["thought"] == true {
            Open::Thinking
        } else {
            Open::Text
        };
        if self.open != Some(kind) {
            self.close(sender);
            let index = self.output.content.len();
            let (block, event) = match kind {
                Open::Thinking => (
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                    AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                    },
                ),
                Open::Text => (
                    ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                    AssistantMessageEvent::TextStart {
                        content_index: index,
                    },
                ),
            };
            self.output.content.push(block);
            self.open = Some(kind);
            sender.update(&self.output, event);
        }
        let incoming = part["thoughtSignature"]
            .as_str()
            .filter(|signature| !signature.is_empty())
            .map(str::to_owned);
        let index = self.last();
        let event = match &mut self.output.content[index] {
            ContentBlock::Thinking(block) => {
                block.thinking.push_str(text);
                if incoming.is_some() {
                    block.thinking_signature = incoming;
                }
                AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: text.to_owned(),
                }
            }
            ContentBlock::Text(block) => {
                block.text.push_str(text);
                if incoming.is_some() {
                    block.text_signature = incoming;
                }
                AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text.to_owned(),
                }
            }
            _ => return,
        };
        sender.update(&self.output, event);
    }

    fn function_call(&mut self, part: &Value, sender: &EventSender) {
        self.close(sender);
        let call = &part["functionCall"];
        let name = call["name"].as_str().unwrap_or_default().to_owned();
        let provided = call["id"].as_str().filter(|id| !id.is_empty());
        let duplicate = provided.is_some_and(|id| {
            self.output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(c) if c.id == id))
        });
        let id = match provided {
            Some(id) if !duplicate => id.to_owned(),
            _ => format!(
                "{name}_{}_{}",
                now_ms(),
                TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
            ),
        };
        let arguments = call["args"].as_object().cloned().unwrap_or_default();
        let tool_call = ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments,
            thought_signature: part["thoughtSignature"]
                .as_str()
                .filter(|signature| !signature.is_empty())
                .map(str::to_owned),
            namespace: None,
        };
        let delta = ri_types::json::to_string(&tool_call.arguments).unwrap_or_default();
        self.output
            .content
            .push(ContentBlock::ToolCall(tool_call.clone()));
        let index = self.last();
        sender.update(
            &self.output,
            AssistantMessageEvent::ToolcallStart {
                content_index: index,
                id,
                tool_name: name,
            },
        );
        sender.update(
            &self.output,
            AssistantMessageEvent::ToolcallDelta {
                content_index: index,
                delta,
            },
        );
        sender.update(
            &self.output,
            AssistantMessageEvent::ToolcallEnd {
                content_index: index,
                tool_call,
            },
        );
    }

    fn handle(&mut self, chunk: &Value, model: &Model, sender: &EventSender) -> Result<(), String> {
        if self.output.response_id.is_none()
            && let Some(id) = chunk["responseId"].as_str().filter(|id| !id.is_empty())
        {
            self.output.response_id = Some(id.to_owned());
        }
        let candidate = &chunk["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                self.text_part(part, text, sender);
            }
            if part["functionCall"].is_object() {
                self.function_call(part, sender);
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.output.raw_stop_reason = Some(reason.to_owned());
            self.output.stop_reason = map_stop_reason(reason)?;
            if self.output.stop_reason == StopReason::Stop
                && self
                    .output
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall(_)))
            {
                self.output.stop_reason = StopReason::ToolUse;
            }
        }
        let metadata = &chunk["usageMetadata"];
        if metadata.is_object() {
            let count = |key: &str| metadata[key].as_u64().unwrap_or(0);
            let cached = count("cachedContentTokenCount");
            let thoughts = count("thoughtsTokenCount");
            let mut usage = Usage {
                input: count("promptTokenCount").saturating_sub(cached),
                output: count("candidatesTokenCount") + thoughts,
                cache_read: cached,
                cache_write: 0,
                reasoning: Some(thoughts),
                total_tokens: Some(count("totalTokenCount")),
                ..Usage::default()
            };
            calculate_cost(model, &mut usage);
            self.output.usage = usage;
        }
        Ok(())
    }
}

/// Splits the SDK's stream into `data:` payloads: events end at `\n\n`, `\r\r` or
/// `\r\n\r\n`, whichever comes first.
fn next_event(buffer: &mut String) -> Option<String> {
    let (index, length) = ["\n\n", "\r\r", "\r\n\r\n"]
        .iter()
        .filter_map(|delimiter| buffer.find(delimiter).map(|index| (index, delimiter.len())))
        .min_by_key(|(index, _)| *index)?;
    let event = buffer[..index].to_owned();
    buffer.drain(..index + length);
    Some(event)
}

/// The SDK's check of a network chunk that is itself an error object.
fn chunk_error(text: &str) -> Option<String> {
    let json: Value = serde_json::from_str(text).ok()?;
    let error = json.get("error")?;
    let code = error["code"].as_f64()?;
    (400.0..600.0).contains(&code).then(|| {
        format!(
            "got status: {}. {}",
            match &error["status"] {
                Value::String(status) => status.clone(),
                Value::Null => "undefined".into(),
                other => other.to_string(),
            },
            ri_types::json::to_string(&json).unwrap_or_default()
        )
    })
}

async fn consume(
    state: &mut State,
    mut response: reqwest::Response,
    model: &Model,
    sender: &EventSender,
    options: &StreamOptions,
) -> Result<(), String> {
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = String::new();
    loop {
        let chunk = match http::read_chunk(&mut response, &options.cancel).await {
            Ok(chunk) => chunk,
            Err(_) if options.cancel.is_cancelled() => return Err(ABORTED.into()),
            Err(message) => return Err(message),
        };
        let Some(bytes) = chunk else {
            if !buffer.trim().is_empty() {
                return Err("Incomplete JSON segment at the end".into());
            }
            break;
        };
        pending.extend_from_slice(&bytes);
        let valid = match std::str::from_utf8(&pending) {
            Ok(text) => text.len(),
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            Err(_) => pending.len(),
        };
        let rest = pending.split_off(valid);
        let text = String::from_utf8_lossy(&std::mem::replace(&mut pending, rest)).into_owned();
        if let Some(error) = chunk_error(&text) {
            return Err(error);
        }
        buffer.push_str(&text);
        while let Some(event) = next_event(&mut buffer) {
            let Some(data) = event.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            let json: Value = serde_json::from_str(data)
                .map_err(|err| format!("exception parsing stream chunk {data}. {err}"))?;
            state.handle(&json, model, sender)?;
        }
    }
    Ok(())
}

async fn run(request: Request, sender: EventSender) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let output = new_output(&model, now_ms());
    let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
        send_error(
            &sender,
            output,
            &options.cancel,
            format!("No API key for provider: {}", model.provider),
        );
        return;
    };
    let max_tokens = clamp_max_tokens_to_context(
        &model,
        &messages,
        options.max_tokens.unwrap_or(model.max_tokens),
    );
    let body = thinking(&model, &options)
        .and_then(|thinking| build_body(&model, &messages, &options, max_tokens, &thinking))
        .and_then(|body| ri_types::json::to_string(&body).map_err(|err| err.to_string()));
    let body = match body {
        Ok(body) => body,
        Err(message) => {
            send_error(&sender, output, &options.cancel, message);
            return;
        }
    };
    let mut headers: IndexMap<String, Option<String>> = IndexMap::new();
    let mut set = |name: &str, value: Option<String>| {
        headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
        headers.insert(name.to_owned(), value);
    };
    set("content-type", Some("application/json".into()));
    set("x-goog-api-key", Some(api_key));
    for (key, value) in model.headers.iter().flatten() {
        set(key, Some(value.clone()));
    }
    for (key, value) in &options.headers {
        set(key, value.clone());
    }
    let url = format!(
        "{}/{}:streamGenerateContent?alt=sse",
        model.base_url.trim_end_matches('/'),
        model_path(&model.id)
    );
    let build = || {
        let mut request = http::client().post(&url).body(body.clone());
        for (name, value) in headers.iter() {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        request
    };
    let response = match http::send(build, &options).await {
        Ok(response) => response,
        Err(failure) => {
            send_error(&sender, output, &options.cancel, failure_message(failure));
            return;
        }
    };
    sender.send(StreamEvent::Start(output.clone()));
    let mut state = State { output, open: None };
    let result = consume(&mut state, response, &model, &sender, &options)
        .await
        .and_then(|()| {
            state.close(&sender);
            if options.cancel.is_cancelled() {
                return Err(http::ABORTED_DURING_STREAM.into());
            }
            match state.output.stop_reason {
                StopReason::Pending => Err("Google stream ended without a finish reason".into()),
                StopReason::Aborted | StopReason::Error => {
                    Err(match &state.output.raw_stop_reason {
                        Some(reason) => format!("Provider stopped with: {reason}"),
                        None => "An unknown error occurred".into(),
                    })
                }
                _ => Ok(()),
            }
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
    fn detects_model_families() {
        assert_eq!(gemini_major_version("gemini-3.1-pro-preview"), Some(3));
        assert_eq!(gemini_major_version("gemini-live-2.5-flash"), Some(2));
        assert!(requires_tool_call_id("gemini-3-flash-preview"));
        assert!(!requires_tool_call_id("gemini-2.5-flash"));
        assert!(requires_tool_call_id("claude-sonnet-4"));
        let model = |id: &str| -> Model {
            serde_json::from_value(json!({
                "id": id, "name": id, "api": "google-generative-ai", "provider": "google",
                "baseUrl": "", "reasoning": true, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 1, "maxTokens": 1,
            }))
            .unwrap()
        };
        assert!(uses_thinking_level(&model("gemini-3.8-flash")));
        assert!(uses_thinking_level(&model("gemini-3-pro-preview")));
        assert!(!uses_thinking_level(&model("gemini-2.5-pro")));
        assert!(uses_thinking_level(&model("gemma-4-31b-it")));
    }

    #[test]
    fn splits_events_and_signatures() {
        let mut buffer = "data: {}\r\n\r\ndata: {\"a\":1}\n\nrest".to_owned();
        assert_eq!(next_event(&mut buffer).as_deref(), Some("data: {}"));
        assert_eq!(next_event(&mut buffer).as_deref(), Some("data: {\"a\":1}"));
        assert_eq!(next_event(&mut buffer), None);
        assert_eq!(buffer, "rest");
        assert!(valid_signature(Some("c2ln")).is_some());
        assert!(valid_signature(Some("c2lnbg==")).is_some());
        assert!(valid_signature(Some("abc")).is_none());
        assert!(valid_signature(Some("ab$=")).is_none());
    }
}
