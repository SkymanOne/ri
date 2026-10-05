//! `anthropic-messages`: the Anthropic Messages API and compatible endpoints.
//!
//! Port of `packages/ai/src/api/anthropic-messages.ts` in pi `v1.0.0`. Not yet
//! ported: input-transformation diagnostics.

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use yapi_types::event::AssistantMessageEvent;
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, ThinkingContent, ThinkingLevel,
    ToolCall, ToolDeclaration, ToolResultMessage,
};
use yapi_types::model::{AnthropicMessagesCompat, Model};

use crate::auth::federation;
use crate::cost::calculate_cost;
use crate::http::{self, Failure, SseReader};
use crate::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::schema;
use crate::stream::{
    CacheRetention, EventSender, EventStream, Provider, Request, StreamEvent, StreamOptions,
    new_output, now_ms, send_error,
};
use crate::thinking::{adjust_max_tokens_for_thinking, clamp_max_tokens_to_context};
use crate::transcript::{
    current_tools, declared_tools, has_tool_redefinitions, initial_system_message,
    resolve_transcript, transform_messages,
};

const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const MID_CONVERSATION_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";

/// The `anthropic-messages` wire API.
#[derive(Debug, Default)]
pub struct AnthropicMessages;

impl Provider for AnthropicMessages {
    fn api(&self) -> &str {
        "anthropic-messages"
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        tokio::spawn(run(request, sender));
        stream
    }
}

/// Compat flags with pi's defaults applied.
struct Compat {
    supports_eager_tool_input_streaming: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    session_affinity_format: Option<String>,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_changes: bool,
    supports_mid_convo_effort: bool,
    force_adaptive_thinking: bool,
    has_fallbacks: bool,
}

impl Compat {
    fn new(model: &Model) -> Compat {
        let raw: AnthropicMessagesCompat = model.compat();
        let open_router =
            model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        Compat {
            supports_eager_tool_input_streaming: raw
                .supports_eager_tool_input_streaming
                .unwrap_or(true),
            supports_long_cache_retention: raw.supports_long_cache_retention.unwrap_or(true),
            send_session_affinity_headers: raw.send_session_affinity_headers.unwrap_or(open_router),
            session_affinity_format: raw
                .session_affinity_format
                .or_else(|| open_router.then(|| "openrouter".to_owned())),
            supports_cache_control_on_tools: raw.supports_cache_control_on_tools.unwrap_or(true),
            supports_temperature: raw.supports_temperature.unwrap_or(true),
            allow_empty_signature: raw.allow_empty_signature.unwrap_or(false),
            supports_strict_tools: raw.supports_strict_tools.unwrap_or(false),
            supports_mid_convo_system_messages: raw
                .supports_mid_convo_system_messages
                .unwrap_or(false),
            supports_mid_convo_tool_changes: raw.supports_mid_convo_tool_changes.unwrap_or(false),
            supports_mid_convo_effort: raw.supports_mid_convo_effort == Some(true),
            force_adaptive_thinking: raw.force_adaptive_thinking == Some(true),
            has_fallbacks: model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("allowedFallbackModels"))
                .and_then(Value::as_array)
                .is_some_and(|fallbacks| !fallbacks.is_empty()),
        }
    }
}

/// Thinking options derived from the requested level.
struct Thinking {
    enabled: Option<bool>,
    budget_tokens: Option<u64>,
    effort: Option<String>,
    max_tokens: u64,
}

fn thinking_options(
    model: &Model,
    compat: &Compat,
    messages: &[Message],
    options: &StreamOptions,
) -> Thinking {
    let base = clamp_max_tokens_to_context(
        model,
        messages,
        options.max_tokens.unwrap_or(model.max_tokens),
    );
    let level = options
        .reasoning
        .filter(|level| *level != ThinkingLevel::Off);
    let Some(level) = level else {
        return Thinking {
            enabled: Some(false),
            budget_tokens: None,
            effort: None,
            max_tokens: base,
        };
    };
    if compat.force_adaptive_thinking {
        return Thinking {
            enabled: Some(true),
            budget_tokens: None,
            effort: Some(effort_for_level(model, level)),
            max_tokens: base,
        };
    }
    let (adjusted, budget) = adjust_max_tokens_for_thinking(
        Some(base),
        model.max_tokens,
        level,
        &options.thinking_budgets,
    );
    let max_tokens = clamp_max_tokens_to_context(model, messages, adjusted);
    Thinking {
        enabled: Some(true),
        budget_tokens: Some(budget.min(max_tokens.saturating_sub(1024))),
        effort: None,
        max_tokens,
    }
}

fn effort_for_level(model: &Model, level: ThinkingLevel) -> String {
    if let Some(Some(mapped)) = model.thinking_level_value(level) {
        return mapped.to_owned();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .to_owned()
}

fn cache_control(compat: &Compat, retention: CacheRetention) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Long if compat.supports_long_cache_retention => {
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        }
        _ => Some(json!({"type": "ephemeral"})),
    }
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |tool| (*tool).to_owned())
}

fn from_claude_code_name(name: &str, tools: &[ToolDeclaration]) -> String {
    tools
        .iter()
        .find(|tool| tool.name.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |tool| tool.name.clone())
}

async fn run(request: Request, sender: EventSender) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let compat = Compat::new(&model);
    let normalized = resolve_transcript(&messages, compat.supports_mid_convo_system_messages);
    let tools = current_tools(&normalized);
    let thinking = thinking_options(&model, &compat, &messages, &options);
    let mut output = new_output(&model, now_ms());
    if compat.supports_mid_convo_effort {
        output.provider_thinking_level =
            Some(thinking.effort.clone().unwrap_or_else(|| "high".into()));
    }

    let api_key = options.api_key.clone().filter(|key| !key.is_empty());
    let mut option_headers = options.headers.clone();
    let mut model_headers: IndexMap<String, Option<String>> = model
        .headers
        .iter()
        .flatten()
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .collect();
    if model.provider == "github-copilot" {
        for (key, value) in super::copilot_headers(&messages) {
            model_headers.insert(key.to_owned(), Some(value));
        }
    }
    let header_auth = options.has_header("authorization")
        || options.has_header("x-api-key")
        || options.has_header("cf-aig-authorization");
    // Workload identity federation stands in for a key on Anthropic itself.
    let federation = (model.provider == "anthropic" && api_key.is_none() && !header_auth)
        .then(|| federation::Config::from_env(&model.base_url, options.env.as_ref()))
        .flatten();
    if api_key.is_none() && !header_auth && federation.is_none() {
        send_error(
            &sender,
            output,
            &options.cancel,
            format!("No API key for provider: {}", model.provider),
        );
        return;
    }

    let oauth =
        model.provider != "github-copilot" && api_key.as_deref().is_some_and(is_oauth_token);
    let retention = options.resolved_cache_retention();
    let params = match build_params(
        &model,
        &compat,
        &normalized,
        &tools,
        oauth,
        retention,
        &thinking,
        &options,
    ) {
        Ok(params) => params,
        Err(message) => {
            send_error(&sender, output, &options.cancel, message);
            return;
        }
    };
    let betas = beta_features(
        &model,
        &compat,
        &normalized,
        oauth,
        &thinking,
        &model_headers,
        &option_headers,
    );

    // Headers: SDK defaults and auth, then pi's defaults, the model's and the
    // caller's; later entries win and `None` removes.
    let mut headers: IndexMap<String, Option<String>> = IndexMap::new();
    let set =
        |headers: &mut IndexMap<String, Option<String>>, name: &str, value: Option<String>| {
            headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
            headers.insert(name.to_owned(), value);
        };
    set(&mut headers, "anthropic-version", Some("2023-06-01".into()));
    set(
        &mut headers,
        "content-type",
        Some("application/json".into()),
    );
    if let Some(key) = &api_key {
        if model.provider == "github-copilot" || oauth {
            set(&mut headers, "authorization", Some(format!("Bearer {key}")));
        } else {
            set(&mut headers, "x-api-key", Some(key.clone()));
        }
    }
    set(&mut headers, "accept", Some("application/json".into()));
    set(
        &mut headers,
        "anthropic-dangerous-direct-browser-access",
        Some("true".into()),
    );
    if oauth {
        set(
            &mut headers,
            "user-agent",
            Some(format!("claude-cli/{CLAUDE_CODE_VERSION}")),
        );
        set(&mut headers, "x-app", Some("cli".into()));
    } else if let Some(session_id) = options
        .session_id
        .as_ref()
        .filter(|_| retention != CacheRetention::None && compat.send_session_affinity_headers)
    {
        let name = if compat.session_affinity_format.as_deref() == Some("openrouter") {
            "x-session-id"
        } else {
            "x-session-affinity"
        };
        set(&mut headers, name, Some(session_id.clone()));
    }
    for (key, value) in model_headers.drain(..).chain(option_headers.drain(..)) {
        set(&mut headers, &key, value);
    }
    if !betas.is_empty() {
        set(&mut headers, "anthropic-beta", Some(betas.join(",")));
    }

    let url = format!(
        "{}/v1/messages?beta=true",
        model.base_url.trim_end_matches('/')
    );
    let body = match yapi_types::json::to_string(&params) {
        Ok(body) => body,
        Err(err) => {
            send_error(&sender, output, &options.cancel, err.to_string());
            return;
        }
    };
    if let Some(config) = &federation {
        match federation::token(config, false, &options.cancel).await {
            Ok(token) => federated_headers(&mut headers, &token),
            Err(message) => {
                send_error(&sender, output, &options.cancel, message);
                return;
            }
        }
    }
    let build = |headers: &IndexMap<String, Option<String>>| {
        let mut request = http::client().post(&url).body(body.clone());
        for (name, value) in headers.iter() {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        request
    };
    let mut result = http::send(|| build(&headers), &options).await;
    // A 401 on a federated token: exchange again and retry once, as the SDK.
    if let Some(config) = &federation
        && matches!(result, Err(Failure::Status { status: 401, .. }))
    {
        federation::invalidate(config);
        match federation::token(config, true, &options.cancel).await {
            Ok(token) => {
                federated_headers(&mut headers, &token);
                result = http::send(|| build(&headers), &options).await;
            }
            Err(message) => {
                send_error(&sender, output, &options.cancel, message);
                return;
            }
        }
    }
    let response = match result {
        Ok(response) => response,
        Err(failure) => {
            let message = match failure {
                Failure::Status { status, body } => match serde_json::from_str::<Value>(&body) {
                    Ok(json) => http::sdk_status_message(status, Some(&json), None),
                    Err(_) => http::sdk_status_message(status, None, Some(&body)),
                },
                other => other.plain_message().unwrap_or_default(),
            };
            send_error(&sender, output, &options.cancel, message);
            return;
        }
    };

    sender.send(StreamEvent::Start(output.clone()));
    let mut state = StreamState {
        output,
        indices: Vec::new(),
        partial_json: Vec::new(),
        usage_model: model.clone(),
        tools,
        oauth,
    };
    match state
        .consume(SseReader::fetch(response), &model, &sender, &options)
        .await
    {
        Ok(()) => {
            let output = state.output;
            sender.send(StreamEvent::Done(output));
        }
        Err(message) => send_error(&sender, state.output, &options.cancel, message),
    }
}

/// Bearer auth with a federated token, adding the OAuth beta to any others.
fn federated_headers(headers: &mut IndexMap<String, Option<String>>, token: &str) {
    headers.retain(|key, _| !key.eq_ignore_ascii_case("authorization"));
    headers.insert("authorization".into(), Some(format!("Bearer {token}")));
    let beta = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
        .and_then(|(_, value)| value.clone());
    let has_oauth = beta.as_deref().is_some_and(|beta| {
        beta.split(',')
            .any(|value| value.trim() == federation::OAUTH_API_BETA)
    });
    if !has_oauth {
        let value = match beta.filter(|beta| !beta.is_empty()) {
            Some(beta) => format!("{beta}, {}", federation::OAUTH_API_BETA),
            None => federation::OAUTH_API_BETA.to_owned(),
        };
        headers.retain(|key, _| !key.eq_ignore_ascii_case("anthropic-beta"));
        headers.insert("anthropic-beta".into(), Some(value));
    }
}

struct StreamState {
    output: AssistantMessage,
    /// Anthropic block index of each content block while it streams.
    indices: Vec<Option<u64>>,
    /// Raw argument JSON of each tool-call block while it streams.
    partial_json: Vec<String>,
    usage_model: Model,
    tools: Vec<ToolDeclaration>,
    oauth: bool,
}

impl StreamState {
    fn position(&self, index: u64) -> Option<usize> {
        self.indices
            .iter()
            .position(|candidate| *candidate == Some(index))
    }

    fn push_block(&mut self, block: ContentBlock, index: u64) -> usize {
        self.output.content.push(block);
        self.indices.push(Some(index));
        self.partial_json.push(String::new());
        self.output.content.len() - 1
    }

    async fn consume(
        &mut self,
        mut reader: SseReader,
        model: &Model,
        sender: &EventSender,
        options: &StreamOptions,
    ) -> Result<(), String> {
        let mut saw_start = false;
        let mut saw_stop = false;
        while let Some(sse) = reader.next(&options.cancel).await? {
            let name = sse.event.as_deref().unwrap_or_default();
            if name == "error" {
                return Err(sse.data);
            }
            if !matches!(
                name,
                "message_start"
                    | "message_delta"
                    | "message_stop"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
            ) {
                continue;
            }
            let event = parse_json_with_repair(&sse.data).map_err(|err| {
                format!(
                    "Could not parse Anthropic SSE event {name}: {err}; data={}",
                    sse.data
                )
            })?;
            match event["type"].as_str() {
                Some("message_start") => saw_start = true,
                Some("message_stop") => saw_stop = true,
                _ => {}
            }
            self.handle(&event, model, sender)?;
        }
        if saw_start && !saw_stop {
            return Err("Anthropic stream ended before message_stop".into());
        }
        if options.cancel.is_cancelled() {
            return Err(http::ABORTED_DURING_STREAM.into());
        }
        match self.output.stop_reason {
            StopReason::Pending => Err("Anthropic stream ended without a stop reason".into()),
            StopReason::Aborted | StopReason::Error => Err(self
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".into())),
            _ => Ok(()),
        }
    }

    fn handle(&mut self, event: &Value, model: &Model, sender: &EventSender) -> Result<(), String> {
        let index = event["index"].as_u64();
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let message = &event["message"];
                if let Some(id) = message["id"].as_str() {
                    self.output.response_id = Some(id.to_owned());
                }
                let response_model = message["model"].as_str().unwrap_or(&model.id);
                if response_model != model.id {
                    self.output.response_model = Some(response_model.to_owned());
                    let fallback_cost = model
                        .compat
                        .as_ref()
                        .and_then(|compat| compat.get("allowedFallbackModels"))
                        .and_then(Value::as_array)
                        .and_then(|fallbacks| {
                            fallbacks.iter().find(|fallback| {
                                fallback["provider"] == model.provider.as_str()
                                    && fallback["model"] == response_model
                            })
                        })
                        .and_then(|fallback| serde_json::from_value(fallback["cost"].clone()).ok());
                    if let Some(cost) = fallback_cost {
                        self.usage_model = Model {
                            id: response_model.to_owned(),
                            cost,
                            ..model.clone()
                        };
                    }
                }
                let usage_json = &message["usage"];
                let usage = &mut self.output.usage;
                usage.input = usage_json["input_tokens"].as_u64().unwrap_or(0);
                usage.output = usage_json["output_tokens"].as_u64().unwrap_or(0);
                usage.cache_read = usage_json["cache_read_input_tokens"].as_u64().unwrap_or(0);
                usage.cache_write = usage_json["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.cache_write_1h = Some(
                    usage_json["cache_creation"]["ephemeral_1h_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                );
                usage.total_tokens =
                    Some(usage.input + usage.output + usage.cache_read + usage.cache_write);
                calculate_cost(&self.usage_model, usage);
            }
            "content_block_start" => {
                let block = &event["content_block"];
                let index = index.unwrap_or_default();
                match block["type"].as_str().unwrap_or_default() {
                    "fallback" => {
                        if !self.output.content.is_empty() {
                            return Err(
                                "Anthropic performed an unsupported mid-output model fallback"
                                    .into(),
                            );
                        }
                    }
                    "text" => {
                        let position = self.push_block(
                            ContentBlock::text(
                                block["text"].as_str().unwrap_or_default().to_owned(),
                            ),
                            index,
                        );
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::TextStart {
                                content_index: position,
                            },
                        );
                    }
                    "thinking" => {
                        let position = self.push_block(
                            ContentBlock::Thinking(ThinkingContent {
                                thinking: block["thinking"].as_str().unwrap_or_default().to_owned(),
                                thinking_signature: Some(
                                    block["signature"].as_str().unwrap_or_default().to_owned(),
                                ),
                                redacted: None,
                            }),
                            index,
                        );
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ThinkingStart {
                                content_index: position,
                            },
                        );
                    }
                    "redacted_thinking" => {
                        let position = self.push_block(
                            ContentBlock::Thinking(ThinkingContent {
                                thinking: "[Reasoning redacted]".into(),
                                thinking_signature: block["data"].as_str().map(str::to_owned),
                                redacted: Some(true),
                            }),
                            index,
                        );
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ThinkingStart {
                                content_index: position,
                            },
                        );
                    }
                    "tool_use" => {
                        let raw_name = block["name"].as_str().unwrap_or_default();
                        let name = if self.oauth {
                            from_claude_code_name(raw_name, &self.tools)
                        } else {
                            raw_name.to_owned()
                        };
                        let id = block["id"].as_str().unwrap_or_default().to_owned();
                        let position = self.push_block(
                            ContentBlock::ToolCall(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: block["input"].as_object().cloned().unwrap_or_default(),
                                thought_signature: None,
                                namespace: None,
                            }),
                            index,
                        );
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ToolcallStart {
                                content_index: position,
                                id,
                                tool_name: name,
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(position) = index.and_then(|index| self.position(index)) else {
                    return Ok(());
                };
                let delta = &event["delta"];
                match (
                    delta["type"].as_str().unwrap_or_default(),
                    &mut self.output.content[position],
                ) {
                    ("text_delta", ContentBlock::Text(text)) => {
                        let piece = delta["text"].as_str().unwrap_or_default();
                        text.text.push_str(piece);
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::TextDelta {
                                content_index: position,
                                delta: piece.to_owned(),
                            },
                        );
                    }
                    ("thinking_delta", ContentBlock::Thinking(thinking)) => {
                        let piece = delta["thinking"].as_str().unwrap_or_default();
                        thinking.thinking.push_str(piece);
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ThinkingDelta {
                                content_index: position,
                                delta: piece.to_owned(),
                            },
                        );
                    }
                    ("input_json_delta", ContentBlock::ToolCall(call)) => {
                        let piece = delta["partial_json"].as_str().unwrap_or_default();
                        self.partial_json[position].push_str(piece);
                        call.arguments = parse_streaming_json(&self.partial_json[position]);
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ToolcallDelta {
                                content_index: position,
                                delta: piece.to_owned(),
                            },
                        );
                    }
                    ("signature_delta", ContentBlock::Thinking(thinking)) => {
                        thinking
                            .thinking_signature
                            .get_or_insert_with(String::new)
                            .push_str(delta["signature"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(position) = index.and_then(|index| self.position(index)) else {
                    return Ok(());
                };
                self.indices[position] = None;
                let event = match &mut self.output.content[position] {
                    ContentBlock::Text(text) => AssistantMessageEvent::TextEnd {
                        content_index: position,
                        content: text.text.clone(),
                    },
                    ContentBlock::Thinking(thinking) => AssistantMessageEvent::ThinkingEnd {
                        content_index: position,
                        content: thinking.thinking.clone(),
                    },
                    ContentBlock::ToolCall(call) => {
                        call.arguments = parse_streaming_json(&self.partial_json[position]);
                        AssistantMessageEvent::ToolcallEnd {
                            content_index: position,
                            tool_call: call.clone(),
                        }
                    }
                    ContentBlock::Image(_) => return Ok(()),
                };
                sender.update(&self.output, event);
            }
            "message_delta" => {
                let delta = &event["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.output.raw_stop_reason = Some(reason.to_owned());
                    let (stop, message) = map_stop_reason(reason, &delta["stop_details"])?;
                    self.output.stop_reason = stop;
                    if let Some(message) = message {
                        self.output.error_message = Some(message);
                    }
                }
                let usage_json = &event["usage"];
                let usage = &mut self.output.usage;
                if let Some(value) = usage_json["input_tokens"].as_u64() {
                    usage.input = value;
                }
                if let Some(value) = usage_json["output_tokens"].as_u64() {
                    usage.output = value;
                }
                if let Some(value) = usage_json["cache_read_input_tokens"].as_u64() {
                    usage.cache_read = value;
                }
                if let Some(value) = usage_json["cache_creation_input_tokens"].as_u64() {
                    usage.cache_write = value;
                }
                if let Some(value) =
                    usage_json["cache_creation"]["ephemeral_1h_input_tokens"].as_u64()
                {
                    usage.cache_write_1h = Some(value);
                }
                if let Some(value) = usage_json["output_tokens_details"]["thinking_tokens"].as_u64()
                {
                    usage.reasoning = Some(value);
                }
                usage.total_tokens =
                    Some(usage.input + usage.output + usage.cache_read + usage.cache_write);
                calculate_cost(&self.usage_model, usage);
            }
            _ => {}
        }
        Ok(())
    }
}

fn map_stop_reason(reason: &str, details: &Value) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => (
            StopReason::Error,
            Some(
                details["explanation"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .unwrap_or("The model refused to complete the request")
                    .to_owned(),
            ),
        ),
        "sensitive" => (
            StopReason::Error,
            Some("Provider stopped with: sensitive".into()),
        ),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

fn beta_features(
    model: &Model,
    compat: &Compat,
    messages: &[Message],
    oauth: bool,
    thinking: &Thinking,
    model_headers: &IndexMap<String, Option<String>>,
    option_headers: &IndexMap<String, Option<String>>,
) -> Vec<String> {
    let mut configured: Option<Option<&str>> = None;
    for (name, value) in model_headers.iter().chain(option_headers.iter()) {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            configured = Some(value.as_deref());
        }
    }
    let mut features: Vec<String> = Vec::new();
    match configured {
        Some(None) => return features,
        Some(Some(value)) => {
            for feature in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                if !features.iter().any(|existing| existing == feature) {
                    features.push(feature.to_owned());
                }
            }
            return features;
        }
        None => {}
    }
    let mut add = |feature: &str| {
        if !features.iter().any(|existing| existing == feature) {
            features.push(feature.to_owned());
        }
    };
    if oauth {
        add("claude-code-20250219");
        add("oauth-2025-04-20");
    }
    if !current_tools(messages).is_empty() && !compat.supports_eager_tool_input_streaming {
        add(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if model.reasoning && thinking.enabled == Some(true) && !compat.force_adaptive_thinking {
        add(INTERLEAVED_THINKING_BETA);
    }
    if compat.has_fallbacks {
        add(SERVER_SIDE_FALLBACK_BETA);
    }
    if compat.supports_mid_convo_effort {
        add(MID_CONVERSATION_OUTPUT_CONFIG_BETA);
        add(THINKING_BINDING_CONTROLS_BETA);
    }
    if native_tool_changes(compat, messages) {
        add(MID_CONVERSATION_TOOL_CHANGES_BETA);
    }
    features
}

fn native_tool_changes(compat: &Compat, messages: &[Message]) -> bool {
    compat.supports_mid_convo_system_messages
        && compat.supports_mid_convo_tool_changes
        && initial_system_message(messages)
            .and_then(|system| system.tools_added.as_ref())
            .is_some_and(|tools| !tools.is_empty())
        && !has_tool_redefinitions(messages)
}

fn normalize_tool_call_id(id: &str) -> String {
    super::sanitize_id_part(id).chars().take(64).collect()
}

#[allow(clippy::too_many_arguments, reason = "mirrors pi's buildParams inputs")]
fn build_params(
    model: &Model,
    compat: &Compat,
    messages: &[Message],
    tools: &[ToolDeclaration],
    oauth: bool,
    retention: CacheRetention,
    thinking: &Thinking,
    options: &StreamOptions,
) -> Result<Value, String> {
    let cache_control = cache_control(compat, retention);
    let initial = initial_system_message(messages);
    let initial_text = initial.map(|system| system.text()).unwrap_or_default();
    let normalize = |id: &str, _: &AssistantMessage| normalize_tool_call_id(id);
    let transformed = transform_messages(messages, model, Some(&normalize), now_ms());
    let conversation = if initial.is_some() {
        &transformed[1..]
    } else {
        &transformed[..]
    };
    let native_changes = native_tool_changes(compat, messages);
    let managed_provider = compat
        .supports_mid_convo_effort
        .then_some(model.provider.as_str());
    let (converted, levels) = convert_messages(
        conversation,
        oauth,
        cache_control.as_ref(),
        compat.allow_empty_signature,
        managed_provider,
        native_changes,
    );
    let active_effort = thinking.effort.clone().unwrap_or_else(|| "high".into());

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert(
        "messages".into(),
        Value::Array(if compat.supports_mid_convo_effort {
            insert_thinking_level_messages(converted, &levels, &active_effort)
        } else {
            converted
        }),
    );
    params.insert("max_tokens".into(), json!(thinking.max_tokens));
    params.insert("stream".into(), json!(true));

    let with_cache = |mut block: Map<String, Value>| {
        if let Some(cache_control) = &cache_control {
            block.insert("cache_control".into(), cache_control.clone());
        }
        Value::Object(block)
    };
    let text_block = |text: &str| {
        let mut block = Map::new();
        block.insert("type".into(), json!("text"));
        block.insert("text".into(), json!(text));
        block
    };
    if oauth {
        let mut system = vec![with_cache(text_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
        ))];
        if !initial_text.is_empty() {
            system.push(with_cache(text_block(&initial_text)));
        }
        params.insert("system".into(), Value::Array(system));
    } else if !initial_text.is_empty() {
        params.insert(
            "system".into(),
            json!([with_cache(text_block(&initial_text))]),
        );
    }

    if let Some(temperature) = options.temperature
        && thinking.enabled != Some(true)
        && !compat.supports_mid_convo_effort
        && compat.supports_temperature
    {
        params.insert("temperature".into(), json!(temperature));
    }

    let tool_cache = if compat.supports_cache_control_on_tools {
        cache_control.as_ref()
    } else {
        None
    };
    if native_changes {
        let initial_tools = initial
            .and_then(|system| system.tools_added.clone())
            .unwrap_or_default();
        let later: Vec<ToolDeclaration> = declared_tools(messages)
            .into_iter()
            .filter(|tool| {
                !initial_tools
                    .iter()
                    .any(|initial| initial.name == tool.name)
            })
            .collect();
        let mut declared = convert_tools(&initial_tools, oauth, compat, tool_cache)?;
        declared.push(json!({
            "name": "__pi_deferred_placeholder__",
            "description": "Reserved placeholder. Never available. Never call this.",
            "input_schema": {"type": "object", "properties": {}, "required": []},
            "defer_loading": true,
        }));
        for mut tool in convert_tools(&later, oauth, compat, None)? {
            if let Some(object) = tool.as_object_mut() {
                object.insert("defer_loading".into(), json!(true));
            }
            declared.push(tool);
        }
        params.insert("tools".into(), Value::Array(declared));
    } else if !tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_tools(tools, oauth, compat, tool_cache)?),
        );
    }

    if compat.supports_mid_convo_effort {
        params.insert(
            "thinking".into(),
            json!({"type": "adaptive", "display": "summarized",
                   "block_binding": {"prefix_mismatch_behavior": "drop_block"}}),
        );
        params.insert("output_config".into(), json!({"effort": "high"}));
    } else if model.reasoning {
        if thinking.enabled == Some(true) {
            if compat.force_adaptive_thinking {
                params.insert(
                    "thinking".into(),
                    json!({"type": "adaptive", "display": "summarized"}),
                );
                if let Some(effort) = &thinking.effort {
                    params.insert("output_config".into(), json!({"effort": effort}));
                }
            } else {
                params.insert(
                    "thinking".into(),
                    json!({"type": "enabled", "budget_tokens": thinking.budget_tokens.filter(|b| *b > 0).unwrap_or(1024), "display": "summarized"}),
                );
            }
        } else if thinking.enabled == Some(false)
            && model.thinking_level_value(ThinkingLevel::Off) != Some(None)
        {
            params.insert("thinking".into(), json!({"type": "disabled"}));
        }
    }

    if let Some(fallbacks) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("allowedFallbackModels"))
        .and_then(Value::as_array)
        .filter(|fallbacks| !fallbacks.is_empty())
    {
        params.insert(
            "fallbacks".into(),
            Value::Array(
                fallbacks
                    .iter()
                    .map(|fallback| json!({"model": fallback["model"]}))
                    .collect(),
            ),
        );
    }
    Ok(Value::Object(params))
}

fn strict_keyword(key: &str, value: &Value) -> bool {
    const UNSUPPORTED: &[&str] = &[
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "maxItems",
        "uniqueItems",
        "minContains",
        "maxContains",
        "minProperties",
        "maxProperties",
    ];
    const FORMATS: &[&str] = &[
        "date-time",
        "time",
        "date",
        "duration",
        "email",
        "hostname",
        "uri",
        "ipv4",
        "ipv6",
        "uuid",
    ];
    if UNSUPPORTED.contains(&key) {
        return true;
    }
    match key {
        "minItems" => value != &json!(0) && value != &json!(1),
        "format" => !value
            .as_str()
            .is_some_and(|format| FORMATS.contains(&format)),
        _ => false,
    }
}

fn convert_tools(
    tools: &[ToolDeclaration],
    oauth: bool,
    compat: &Compat,
    cache_control: Option<&Value>,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict =
                schema::strict_sampling(tool, compat.supports_strict_tools, Some(strict_keyword))?;
            let parameters = schema::tool_parameters(tool, strict);
            let properties = parameters
                .get("properties")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let required = parameters
                .get("required")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let input_schema = if strict {
                let mut schema = parameters.as_object().cloned().unwrap_or_default();
                schema.insert("type".into(), json!("object"));
                schema.insert("properties".into(), properties);
                schema.insert("required".into(), required);
                Value::Object(schema)
            } else {
                json!({"type": "object", "properties": properties, "required": required})
            };
            let mut declared = Map::new();
            declared.insert(
                "name".into(),
                json!(if oauth {
                    to_claude_code_name(&tool.name)
                } else {
                    tool.name.clone()
                }),
            );
            declared.insert("description".into(), json!(tool.description));
            if compat.supports_eager_tool_input_streaming {
                declared.insert("eager_input_streaming".into(), json!(true));
            }
            if strict {
                declared.insert("strict".into(), json!(true));
            }
            declared.insert("input_schema".into(), input_schema);
            if let Some(cache_control) = cache_control
                && index == tools.len() - 1
            {
                declared.insert("cache_control".into(), cache_control.clone());
            }
            Ok(Value::Object(declared))
        })
        .collect()
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

fn tool_result_content(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)));
    if !has_images {
        return json!(yapi_types::message::blocks_text(content, "\n"));
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(json!({"type": "text", "text": text.text})),
            ContentBlock::Image(image) => Some(image_block(&image.data, &image.mime_type)),
            _ => None,
        })
        .collect();
    if !blocks.iter().any(|block| block["type"] == "text") {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

fn convert_tool_result(result: &ToolResultMessage) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": result.tool_call_id,
        "content": tool_result_content(&result.content),
        "is_error": result.is_error,
    })
}

fn is_effort(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high" | "xhigh" | "max")
}

/// Converts the conversation (without the initial system message) to Anthropic
/// messages, and records the effort of earlier managed-effort assistant messages.
fn convert_messages(
    messages: &[Message],
    oauth: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    managed_provider: Option<&str>,
    native_tool_changes: bool,
) -> (Vec<Value>, Vec<(usize, String)>) {
    let mut params: Vec<Value> = Vec::new();
    let mut levels = Vec::new();
    let mut pending_system: Vec<Value> = Vec::new();
    let tool_name = |name: &str| {
        if oauth {
            to_claude_code_name(name)
        } else {
            name.to_owned()
        }
    };

    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::System(system) => {
                let text = system.render_update();
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if native_tool_changes {
                    for tool in system.tools_removed.iter().flatten() {
                        blocks.push(json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": tool_name(&tool.name)}}));
                    }
                    for tool in system.tools_added.iter().flatten() {
                        blocks.push(json!({"type": "tool_addition", "tool": {"type": "tool_reference", "name": tool_name(&tool.name)}}));
                    }
                }
                if !blocks.is_empty() {
                    pending_system.push(json!({"role": "system", "content": blocks}));
                }
            }
            Message::User(user) => match &user.content {
                Content::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({"role": "user", "content": text}));
                    }
                }
                Content::Blocks(blocks) => {
                    let blocks: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                                Some(json!({"type": "text", "text": text.text}))
                            }
                            ContentBlock::Image(image) => {
                                Some(image_block(&image.data, &image.mime_type))
                            }
                            _ => None,
                        })
                        .collect();
                    if !blocks.is_empty() {
                        params.push(json!({"role": "user", "content": blocks}));
                    }
                }
            },
            Message::Assistant(assistant) => {
                params.append(&mut pending_system);
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(text) => {
                            if !text.text.trim().is_empty() {
                                blocks.push(json!({"type": "text", "text": text.text}));
                            }
                        }
                        ContentBlock::Thinking(thinking) => {
                            if thinking.redacted == Some(true) {
                                blocks.push(json!({"type": "redacted_thinking", "data": thinking.thinking_signature}));
                                continue;
                            }
                            let signature = thinking
                                .thinking_signature
                                .as_deref()
                                .filter(|signature| !signature.trim().is_empty());
                            if thinking.thinking.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            blocks.push(match signature {
                                Some(signature) => json!({"type": "thinking", "thinking": thinking.thinking, "signature": signature}),
                                None if allow_empty_signature => json!({"type": "thinking", "thinking": thinking.thinking, "signature": ""}),
                                None => json!({"type": "text", "text": thinking.thinking}),
                            });
                        }
                        ContentBlock::ToolCall(call) => {
                            blocks.push(json!({"type": "tool_use", "id": call.id, "name": tool_name(&call.name), "input": call.arguments}));
                        }
                        ContentBlock::Image(_) => {}
                    }
                }
                if blocks.is_empty() {
                    index += 1;
                    continue;
                }
                let position = params.len();
                params.push(json!({"role": "assistant", "content": blocks}));
                if let (Some(provider), Some(level)) = (
                    managed_provider,
                    assistant.provider_thinking_level.as_deref(),
                ) && assistant.api == "anthropic-messages"
                    && assistant.provider == provider
                    && is_effort(level)
                {
                    levels.push((position, level.to_owned()));
                }
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    results.push(convert_tool_result(result));
                    index += 1;
                }
                params.push(json!({"role": "user", "content": results}));
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    params.append(&mut pending_system);

    if let (Some(cache_control), Some(last)) = (cache_control, params.last_mut())
        && matches!(last["role"].as_str(), Some("user" | "system"))
    {
        match &mut last["content"] {
            Value::Array(blocks) => {
                if let Some(block) = blocks.last_mut()
                    && matches!(
                        block["type"].as_str(),
                        Some("text" | "image" | "tool_result" | "tool_addition" | "tool_removal")
                    )
                    && let Some(object) = block.as_object_mut()
                {
                    object.insert("cache_control".into(), cache_control.clone());
                }
            }
            Value::String(text) => {
                let text = std::mem::take(text);
                last["content"] =
                    json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            }
            _ => {}
        }
    }
    (params, levels)
}

fn insert_thinking_level_messages(
    messages: Vec<Value>,
    levels: &[(usize, String)],
    active: &str,
) -> Vec<Value> {
    let mut result = Vec::new();
    for (index, message) in messages.into_iter().enumerate() {
        if let Some((_, effort)) = levels.iter().find(|(position, _)| *position == index) {
            result.push(
                json!({"role": "system", "content": [], "output_config": {"effort": effort}}),
            );
        }
        result.push(message);
    }
    result.push(json!({"role": "system", "content": [], "output_config": {"effort": active}}));
    result
}
