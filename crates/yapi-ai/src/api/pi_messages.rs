//! `pi-messages`: pi's own message protocol, spoken by the Radius gateway.
//!
//! One POST of `{model, context, options}` to `<baseUrl>/messages`, answered
//! by an SSE stream of serialized assistant-message events ending in `done`
//! or `error`. Port of `packages/ai/src/api/pi-messages.ts` in pi `v1.0.0`.

use serde_json::{Map, Value, json};
use yapi_types::event::AssistantMessageEvent;
use yapi_types::message::{
    AssistantMessage, ContentBlock, StopReason, ThinkingContent, ToolCall, Usage,
};
use yapi_types::model::Model;

use crate::credentials::provider_env_value;
use crate::http;
use crate::json_parse::parse_streaming_json;
use crate::stream::{
    CacheRetention, EventSender, Request, StreamEvent, StreamOptions, new_output, now_ms,
    send_error,
};

/// The request's cache retention: the option, else the legacy
/// `PI_CACHE_RETENTION=long`; the backend's default otherwise.
fn cache_retention(options: &StreamOptions) -> Option<&'static str> {
    match options.cache_retention {
        Some(CacheRetention::None) => Some("none"),
        Some(CacheRetention::Short) => Some("short"),
        Some(CacheRetention::Long) => Some("long"),
        None => (provider_env_value("PI_CACHE_RETENTION", options.env.as_ref()).as_deref()
            == Some("long"))
        .then_some("long"),
    }
}

fn payload(model: &Model, request: &Request) -> Value {
    let options = &request.options;
    let mut fields = Map::new();
    if let Some(temperature) = options.temperature {
        fields.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        fields.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(reasoning) = options.reasoning {
        fields.insert("reasoning".into(), json!(reasoning.as_str()));
    }
    if let Some(retention) = cache_retention(options) {
        fields.insert("cacheRetention".into(), json!(retention));
    }
    if let Some(session) = &options.session_id {
        fields.insert("sessionId".into(), json!(session));
    }
    json!({
        "model": model.id,
        "context": { "messages": request.messages },
        "options": fields,
    })
}

/// The SDK-style message for a failed response, and the diagnostic pi
/// records with it.
fn response_failure(
    model: &Model,
    url: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> (String, Value) {
    let parsed = serde_json::from_str::<Value>(body)
        .ok()
        .filter(|value| value["error"].is_object());
    let error = parsed.as_ref().map(|value| &value["error"]);
    let message = error.and_then(|error| error["message"].as_str());
    let code = error.and_then(|error| error["code"].as_str());
    let text = format!(
        "{} {}: {}{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or_default(),
        message.unwrap_or(body),
        code.map(|code| format!(" ({code})")).unwrap_or_default()
    );
    let mut details = Map::new();
    details.insert("version".into(), json!(1));
    details.insert("provider".into(), json!(model.provider));
    details.insert("model".into(), json!(model.id));
    details.insert("url".into(), json!(url));
    details.insert("status".into(), json!(status.as_u16()));
    details.insert(
        "statusText".into(),
        json!(status.canonical_reason().unwrap_or_default()),
    );
    match error {
        Some(error) => {
            details.insert("error".into(), error.clone());
        }
        None => {
            let truncated: String = body.chars().take(8192).collect();
            let body = if body.chars().count() > 8192 {
                format!("{truncated}…")
            } else {
                truncated
            };
            details.insert("body".into(), json!(body));
        }
    }
    details.insert("timestampMs".into(), json!(now_ms()));
    let mut error_info = Map::new();
    error_info.insert("name".into(), json!("PiMessagesResponseError"));
    error_info.insert("message".into(), json!(text));
    if let Some(code) = code {
        error_info.insert("code".into(), json!(code));
    }
    let diagnostic = json!({
        "type": "pi_messages_response_failure",
        "timestamp": now_ms(),
        "error": error_info,
        "details": details,
    });
    (text, diagnostic)
}

/// Rebuilds the message from the backend's events, as pi's event converter.
struct Converter {
    partial: AssistantMessage,
    tool_json: Vec<(usize, String)>,
}

fn rewrite_diagnostic(rewrite: &Value) -> Option<Value> {
    rewrite.is_object().then(|| {
        json!({
            "type": "pi_messages_rewrite",
            "timestamp": now_ms(),
            "details": rewrite,
        })
    })
}

impl Converter {
    fn block(&mut self, index: usize, block: ContentBlock) {
        let content = &mut self.partial.content;
        while content.len() <= index {
            content.push(ContentBlock::text(""));
        }
        content[index] = block;
    }

    fn finish(&mut self, event: &Value) {
        if let Ok(usage) = serde_json::from_value::<Usage>(event["usage"].clone()) {
            self.partial.usage = usage;
        }
        self.partial.response_id = event["responseId"].as_str().map(str::to_owned);
        if let Some(level) = event["providerThinkingLevel"].as_str() {
            self.partial.provider_thinking_level = Some(level.to_owned());
        }
        if let Some(diagnostic) = rewrite_diagnostic(&event["rewrite"]) {
            self.partial
                .diagnostics
                .get_or_insert_with(Vec::new)
                .push(diagnostic);
        }
    }

    /// Applies one event; returns the stream event to send, if any.
    fn apply(&mut self, event: &Value) -> Option<StreamEvent> {
        let index = event["contentIndex"].as_u64().unwrap_or_default() as usize;
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_owned();
        let update = match event["type"].as_str().unwrap_or_default() {
            "start" => return Some(StreamEvent::Start(self.partial.clone())),
            "done" => {
                self.finish(event);
                self.partial.stop_reason = match event["reason"].as_str() {
                    Some("length") => StopReason::Length,
                    Some("toolUse") => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
                return Some(StreamEvent::Done(self.partial.clone()));
            }
            "error" => {
                self.finish(event);
                self.partial.stop_reason = match event["reason"].as_str() {
                    Some("aborted") => StopReason::Aborted,
                    _ => StopReason::Error,
                };
                self.partial.error_message = event["errorMessage"].as_str().map(str::to_owned);
                return Some(StreamEvent::Error(self.partial.clone()));
            }
            "text_start" => {
                self.block(index, ContentBlock::text(""));
                AssistantMessageEvent::TextStart {
                    content_index: index,
                }
            }
            "text_delta" => {
                let delta = text("delta");
                if let Some(ContentBlock::Text(block)) = self.partial.content.get_mut(index) {
                    block.text.push_str(&delta);
                }
                AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                }
            }
            "text_end" => {
                let content = text("content");
                if let Some(ContentBlock::Text(block)) = self.partial.content.get_mut(index) {
                    block.text.clone_from(&content);
                    block.text_signature = event["contentSignature"].as_str().map(str::to_owned);
                }
                AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                }
            }
            "thinking_start" => {
                self.block(
                    index,
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                );
                AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                }
            }
            "thinking_delta" => {
                let delta = text("delta");
                if let Some(ContentBlock::Thinking(block)) = self.partial.content.get_mut(index) {
                    block.thinking.push_str(&delta);
                }
                AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                }
            }
            "thinking_end" => {
                let content = text("content");
                if let Some(ContentBlock::Thinking(block)) = self.partial.content.get_mut(index) {
                    block.thinking.clone_from(&content);
                    block.thinking_signature =
                        event["contentSignature"].as_str().map(str::to_owned);
                    block.redacted = event["redacted"].as_bool();
                }
                AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                }
            }
            "toolcall_start" => {
                let id = text("id");
                let name = text("toolName");
                self.block(
                    index,
                    ContentBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Map::new(),
                        thought_signature: None,
                        namespace: None,
                    }),
                );
                self.tool_json.retain(|(at, _)| *at != index);
                self.tool_json.push((index, String::new()));
                AssistantMessageEvent::ToolcallStart {
                    content_index: index,
                    id,
                    tool_name: name,
                }
            }
            "toolcall_delta" => {
                let delta = text("delta");
                let json = match self.tool_json.iter_mut().find(|(at, _)| *at == index) {
                    Some((_, json)) => {
                        json.push_str(&delta);
                        json.clone()
                    }
                    None => {
                        self.tool_json.push((index, delta.clone()));
                        delta.clone()
                    }
                };
                if let Some(ContentBlock::ToolCall(call)) = self.partial.content.get_mut(index) {
                    call.arguments = parse_streaming_json(&json);
                }
                AssistantMessageEvent::ToolcallDelta {
                    content_index: index,
                    delta,
                }
            }
            "toolcall_end" => {
                self.tool_json.retain(|(at, _)| *at != index);
                let Some(ContentBlock::ToolCall(call)) = self.partial.content.get_mut(index) else {
                    return None;
                };
                let finished = &event["toolCall"];
                if let Some(id) = finished["id"].as_str() {
                    call.id = id.to_owned();
                }
                if let Some(name) = finished["name"].as_str() {
                    call.name = name.to_owned();
                }
                if let Some(arguments) = finished["arguments"].as_object() {
                    call.arguments = arguments.clone();
                }
                if let Some(signature) = finished["thoughtSignature"].as_str() {
                    call.thought_signature = Some(signature.to_owned());
                }
                AssistantMessageEvent::ToolcallEnd {
                    content_index: index,
                    tool_call: call.clone(),
                }
            }
            _ => return None,
        };
        Some(StreamEvent::Update {
            event: update,
            usage: self.partial.usage.clone(),
        })
    }
}

/// The `data:` payload of one SSE event; `None` for `[DONE]` and events
/// without data.
fn event_data(raw: &str) -> Option<&str> {
    let data = raw
        .lines()
        .find_map(|line| line.strip_prefix("data:"))?
        .trim();
    (!data.is_empty() && data != "[DONE]").then_some(data)
}

pub(super) async fn run(request: Request, sender: EventSender) {
    let model = request.model.clone();
    let options = request.options.clone();
    let fail = |message: String| {
        send_error(
            &sender,
            new_output(&model, now_ms()),
            &options.cancel,
            message,
        );
    };
    let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
        return fail(format!(
            "No API key provided for provider \"{}\"",
            model.provider
        ));
    };
    let url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    let body = yapi_types::json::to_string(&payload(&model, &request)).unwrap_or_default();
    let mut builder = http::client()
        .post(&url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("accept", "text/event-stream")
        .header("content-type", "application/json");
    for (name, value) in &options.headers {
        if let Some(value) = value {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    let response = tokio::select! {
        () = options.cancel.cancelled() => return fail(http::ABORTED_READ.into()),
        response = builder.body(body).send() => response,
    };
    let Ok(mut response) = response else {
        return fail("fetch failed".into());
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let (message, diagnostic) = response_failure(&model, &url, status, &text);
        let mut output = new_output(&model, now_ms());
        if !options.cancel.is_cancelled() {
            output.diagnostics = Some(vec![diagnostic]);
        }
        return send_error(&sender, output, &options.cancel, message);
    }
    let mut converter = Converter {
        partial: new_output(&model, now_ms()),
        tool_json: Vec::new(),
    };
    let mut buffer = String::new();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let chunk = match http::read_chunk(&mut response, &options.cancel, http::ABORTED_READ).await
        {
            Ok(chunk) => chunk,
            Err(message) => return fail(message),
        };
        let done = chunk.is_none();
        if let Some(bytes) = chunk {
            buffer.push_str(&yapi_types::js::decode_utf8_stream(&mut pending, &bytes));
        }
        buffer = buffer.replace("\r\n", "\n");
        let mut events: Vec<String> = Vec::new();
        while let Some(split) = buffer.find("\n\n") {
            events.push(buffer[..split].to_owned());
            buffer.drain(..split + 2);
        }
        if done && !buffer.trim().is_empty() {
            events.push(std::mem::take(&mut buffer));
        }
        for raw in events {
            let Some(data) = event_data(&raw) else {
                continue;
            };
            let event: Value = match serde_json::from_str(data) {
                Ok(event) => event,
                Err(err) => return fail(err.to_string()),
            };
            if let Some(update) = converter.apply(&event) {
                let terminal = matches!(update, StreamEvent::Done(_) | StreamEvent::Error(_));
                sender.send(update);
                if terminal {
                    return;
                }
            }
        }
        if done {
            break;
        }
    }
    fail(format!(
        "{} stream ended without a terminal event",
        model.provider
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_gateway_errors() {
        let model = crate::catalog::builtin_models("radius").remove(0);
        let (message, diagnostic) = response_failure(
            &model,
            "https://radius.pi.dev/v1/messages",
            reqwest::StatusCode::PAYMENT_REQUIRED,
            r#"{"error":{"message":"Out of credits","code":"insufficient_credits"}}"#,
        );
        assert_eq!(
            message,
            "402 Payment Required: Out of credits (insufficient_credits)"
        );
        assert_eq!(
            diagnostic["details"]["error"]["code"],
            "insufficient_credits"
        );
        let (message, diagnostic) = response_failure(
            &model,
            "u",
            reqwest::StatusCode::BAD_GATEWAY,
            "upstream down",
        );
        assert_eq!(message, "502 Bad Gateway: upstream down");
        assert_eq!(diagnostic["details"]["body"], "upstream down");
        assert_eq!(event_data("event: x\ndata: [DONE]"), None);
        assert_eq!(
            event_data("data: {\"type\":\"start\"}"),
            Some("{\"type\":\"start\"}")
        );
    }
}
