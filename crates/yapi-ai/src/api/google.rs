//! `google-generative-ai`: the Gemini API's `streamGenerateContent`.
//!
//! Port of `packages/ai/src/api/google-generative-ai.ts` and `google-shared.ts` in
//! pi `v1.0.0`, with the request and stream handling of `@google/genai` 2.21.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, ThinkingLevel, ToolCall,
    ToolDeclaration, ToolResultMessage, Usage, blocks_text,
};
use yapi_types::model::Model;

use crate::auth::google_adc;
use crate::cost::calculate_cost;
use crate::http::{self, Failure, Headers};
use crate::schema;
use crate::stream::{
    EventSender, Request, StreamEvent, StreamOptions, ThinkingBudgets, check_complete, new_output,
    now_ms, send_error,
};
use crate::thinking::{clamp_level, requested_max_tokens};
use crate::transcript::{
    current_tools, initial_system_message, resolve_transcript, transform_messages,
};

/// Which Google API a request goes to: `google-generative-ai` or, with a
/// Google Cloud API key or Application Default Credentials, `google-vertex`
/// (port of `google-vertex.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flavor {
    Gemini,
    Vertex,
}

/// Message when a request is cancelled, as `fetch` reports it.
const ABORTED: &str = http::ABORTED_READ;

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

fn budget(model: &Model, level: ThinkingLevel, custom: &ThinkingBudgets, flavor: Flavor) -> i64 {
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
    } else if flavor == Flavor::Gemini && model.id.contains("2.5-flash-lite") {
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
fn thinking(model: &Model, options: &StreamOptions, flavor: Flavor) -> Result<Thinking, String> {
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
        Thinking::Budget(budget(model, resolved, &options.thinking_budgets, flavor))
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
    let text = blocks_text(&result.content, "\n");
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
            super::normalize_tool_call_id(id)
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
        Ok(json) => yapi_types::json::stringify(&json),
        Err(_) => {
            let reason = reqwest::StatusCode::from_u16(status)
                .ok()
                .and_then(|code| code.canonical_reason())
                .unwrap_or("");
            let error = json!({"error": {"message": body, "code": status, "status": reason}});
            yapi_types::json::stringify(&error)
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
        if self.open.take().is_some() {
            sender.end(&self.output, self.last());
        }
    }

    fn text_part(&mut self, part: &Value, text: &str, sender: &EventSender) {
        let kind = if part["thought"] == true {
            Open::Thinking
        } else {
            Open::Text
        };
        if self.open != Some(kind) {
            self.close(sender);
            let block = match kind {
                Open::Thinking => ContentBlock::thinking("", None),
                Open::Text => ContentBlock::text(""),
            };
            sender.start(&mut self.output, block);
            self.open = Some(kind);
        }
        let incoming = part["thoughtSignature"]
            .as_str()
            .filter(|signature| !signature.is_empty())
            .map(str::to_owned);
        let index = self.last();
        let signature = match &mut self.output.content[index] {
            ContentBlock::Thinking(block) => &mut block.thinking_signature,
            ContentBlock::Text(block) => &mut block.text_signature,
            _ => return,
        };
        if incoming.is_some() {
            *signature = incoming;
        }
        sender.delta(&mut self.output, index, text);
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
        let delta = yapi_types::json::stringify(&arguments);
        let tool_call = ContentBlock::ToolCall(ToolCall {
            id,
            name,
            arguments,
            thought_signature: part["thoughtSignature"]
                .as_str()
                .filter(|signature| !signature.is_empty())
                .map(str::to_owned),
            namespace: None,
        });
        let index = sender.start(&mut self.output, tool_call);
        sender.delta(&mut self.output, index, &delta);
        sender.end(&self.output, index);
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
            self.output.error_message = (self.output.stop_reason == StopReason::Error)
                .then(|| format!("Provider stopped with: {reason}"));
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
            yapi_types::json::stringify(&json)
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
        let chunk = match http::read_chunk(&mut response, &options.cancel, ABORTED).await {
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
        let text = yapi_types::js::decode_utf8_stream(&mut pending, &bytes);
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

pub(super) async fn run(request: Request, sender: EventSender, flavor: Flavor) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let output = new_output(&model, now_ms());
    let response = match connect(&model, &messages, &options, flavor).await {
        Ok(response) => response,
        Err(message) => return send_error(&sender, output, &options.cancel, message),
    };
    sender.send(StreamEvent::Start(output.clone()));
    let mut state = State { output, open: None };
    let pending = match flavor {
        Flavor::Gemini => "Google stream ended without a finish reason",
        Flavor::Vertex => "Google Vertex stream ended without a finish reason",
    };
    let result = consume(&mut state, response, &model, &sender, &options)
        .await
        .and_then(|()| {
            state.close(&sender);
            check_complete(&state.output, &options.cancel, pending)
        });
    match result {
        Ok(()) => sender.send(StreamEvent::Done(state.output)),
        Err(message) => send_error(&sender, state.output, &options.cancel, message),
    }
}

/// Sends the request; the response once its status is a success.
async fn connect(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    flavor: Flavor,
) -> Result<reqwest::Response, String> {
    let api_key = options.api_key.clone().filter(|key| !key.is_empty());
    if flavor == Flavor::Gemini && api_key.is_none() {
        return Err(format!("No API key for provider: {}", model.provider));
    }
    let target = match flavor {
        Flavor::Gemini => Target {
            url: format!(
                "{}/{}:streamGenerateContent?alt=sse",
                model.base_url.trim_end_matches('/'),
                model_path(&model.id)
            ),
            auth: Auth::Key(api_key.unwrap_or_default()),
        },
        Flavor::Vertex => vertex::target(model, options)?,
    };
    let max_tokens = requested_max_tokens(model, messages, options);
    let thinking = thinking(model, options, flavor)?;
    let body = build_body(model, messages, options, max_tokens, &thinking)?;
    let body = options.hooks.payload(body).await;
    let body = yapi_types::json::stringify(&body);
    let mut headers = Headers::default();
    headers.set("content-type", Some("application/json"));
    match target.auth {
        Auth::Key(key) => headers.set("x-goog-api-key", Some(key)),
        Auth::Credentials { credentials_file } => {
            let token = google_adc::token(
                credentials_file,
                google_adc::OAUTH2_TOKEN_URL,
                &options.cancel,
            )
            .await?;
            headers.set(
                "authorization",
                Some(format!("Bearer {}", token.access_token)),
            );
            headers.set("x-goog-user-project", token.quota_project);
        }
    }
    headers.extend_model(model.headers.as_ref());
    headers.extend(&options.headers);
    let build = || headers.apply(http::client().post(&target.url).body(body.clone()));
    http::send(build, options).await.map_err(failure_message)
}

/// Where a request goes and how it authenticates.
struct Target {
    url: String,
    auth: Auth,
}

enum Auth {
    /// `x-goog-api-key`.
    Key(String),
    /// A bearer token from Application Default Credentials.
    Credentials { credentials_file: Option<String> },
}

/// Vertex AI endpoints, as `@google/genai` 2.21 builds them.
mod vertex {
    use yapi_types::model::Model;

    use super::{Auth, Target};
    use crate::auth::google_adc;
    use crate::credentials::provider_env_value;
    use crate::stream::StreamOptions;

    /// Marks a request that should use Application Default Credentials.
    const CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
    const API_VERSION: &str = "v1";

    /// pi's `resolveApiKey`: a real key, not a marker or `<placeholder>`.
    fn api_key(options: &StreamOptions) -> Option<String> {
        let key = options.api_key.as_deref()?.trim();
        let placeholder = key.len() > 2
            && key.starts_with('<')
            && key.ends_with('>')
            && !key[1..key.len() - 1].contains('>');
        (!key.is_empty() && key != CREDENTIALS_MARKER && !placeholder).then(|| key.to_owned())
    }

    /// The model's base URL unless it is the catalog's `{location}` template.
    fn custom_base_url(model: &Model) -> Option<&str> {
        let trimmed = model.base_url.trim();
        (!trimmed.is_empty() && !trimmed.contains("{location}")).then_some(trimmed)
    }

    /// Whether a path segment of the URL is an API version such as `v1beta1`.
    fn includes_api_version(base_url: &str) -> bool {
        let is_version = |part: &str| {
            part.strip_prefix('v').is_some_and(|rest| {
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && {
                    let tail = &rest[digits..];
                    tail.is_empty()
                        || tail
                            .strip_prefix("beta")
                            .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
                }
            })
        };
        match url::Url::parse(base_url) {
            Ok(url) => url.path().split('/').any(is_version),
            Err(_) => base_url.split('/').any(is_version),
        }
    }

    /// The SDK's `tModel` for Vertex.
    fn model_path(id: &str) -> String {
        if id.starts_with("publishers/") || id.starts_with("projects/") || id.starts_with("models/")
        {
            id.to_owned()
        } else if let Some((publisher, model)) = id.split_once('/') {
            let model = model.split('/').next().unwrap_or(model);
            format!("publishers/{publisher}/models/{model}")
        } else {
            format!("publishers/google/models/{id}")
        }
    }

    fn join(base: &str, version: bool, path: &str) -> String {
        let base = base.strip_suffix('/').unwrap_or(base);
        let mut url = base.to_owned();
        if version {
            url.push('/');
            url.push_str(API_VERSION);
        }
        format!("{url}/{path}:streamGenerateContent?alt=sse")
    }

    /// The request URL and auth: an API key on the global endpoint, else
    /// Application Default Credentials on the project's regional endpoint.
    pub(super) fn target(model: &Model, options: &StreamOptions) -> Result<Target, String> {
        let env = |name: &str| provider_env_value(name, options.env.as_ref());
        let custom = custom_base_url(model);
        let path = model_path(&model.id);
        if let Some(key) = api_key(options) {
            let url = match custom {
                Some(base) => join(base, !includes_api_version(base), &path),
                None => join("https://aiplatform.googleapis.com", true, &path),
            };
            return Ok(Target {
                url,
                auth: Auth::Key(key),
            });
        }
        let project = env("GOOGLE_CLOUD_PROJECT")
            .or_else(|| env("GCLOUD_PROJECT"))
            .ok_or_else(|| {
                "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.".to_owned()
            })?;
        let location = env("GOOGLE_CLOUD_LOCATION").ok_or_else(|| {
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
                .to_owned()
        })?;
        let url = match custom {
            Some(base) => join(base, !includes_api_version(base), &path),
            None => {
                let base = match location.as_str() {
                    "global" => "https://aiplatform.googleapis.com".to_owned(),
                    "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
                    _ => format!("https://{location}-aiplatform.googleapis.com"),
                };
                let path = if path.starts_with("projects/") {
                    path
                } else {
                    format!("projects/{project}/locations/{location}/{path}")
                };
                join(&base, true, &path)
            }
        };
        Ok(Target {
            url,
            auth: Auth::Credentials {
                credentials_file: google_adc::credentials_file(env),
            },
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::credentials::ProviderEnv;

        fn options(key: Option<&str>, vars: &[(&str, &str)]) -> StreamOptions {
            let env: ProviderEnv = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            StreamOptions {
                api_key: key.map(str::to_owned),
                env: Some(env),
                ..StreamOptions::default()
            }
        }

        fn model(id: &str, base_url: &str) -> Model {
            let mut model = crate::catalog::builtin_models("google-vertex")
                .into_iter()
                .next()
                .unwrap();
            model.id = id.into();
            model.base_url = base_url.into();
            model
        }

        #[test]
        fn builds_the_sdk_urls() {
            let template = "https://{location}-aiplatform.googleapis.com";
            let url = |key: Option<&str>, location: &str, base: &str| {
                target(
                    &model("gemini-2.5-flash", base),
                    &options(
                        key,
                        &[
                            ("GOOGLE_CLOUD_PROJECT", "proj-1"),
                            ("GOOGLE_CLOUD_LOCATION", location),
                        ],
                    ),
                )
                .map(|target| target.url)
            };
            assert_eq!(
                url(Some("AIzaKEY"), "us-central1", template).unwrap(),
                "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
            );
            for (location, host) in [
                (
                    "us-central1",
                    "https://us-central1-aiplatform.googleapis.com",
                ),
                ("global", "https://aiplatform.googleapis.com"),
                ("eu", "https://aiplatform.eu.rep.googleapis.com"),
            ] {
                assert_eq!(
                    url(Some("gcp-vertex-credentials"), location, template).unwrap(),
                    format!(
                        "{host}/v1/projects/proj-1/locations/{location}/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
                    )
                );
            }
            assert_eq!(
                url(None, "us-central1", "http://proxy.local/vertex/v1beta1/").unwrap(),
                "http://proxy.local/vertex/v1beta1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
            );
            assert_eq!(
                url(Some("<key>"), "us-central1", "http://proxy.local").unwrap(),
                "http://proxy.local/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
            );
            assert_eq!(model_path("meta/llama-4"), "publishers/meta/models/llama-4");
            let missing = target(&model("gemini-2.5-flash", template), &options(None, &[]));
            assert_eq!(
                missing.err().as_deref(),
                Some(
                    "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
                )
            );
        }
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
