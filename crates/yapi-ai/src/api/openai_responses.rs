//! The OpenAI Responses API and its two variants: `openai-responses`,
//! `azure-openai-responses` and `openai-codex-responses` (ChatGPT's Codex
//! backend).
//!
//! Port of `packages/ai/src/api/openai-responses.ts`,
//! `azure-openai-responses.ts`, `openai-codex-responses.ts` and
//! `openai-responses-shared.ts` in pi `v1.0.0`. Not yet ported: grammar-constrained
//! custom tools (such tools are sent as function tools), service tier selection,
//! and Codex's WebSocket transport and zstd request compression (Codex requests
//! use pi's SSE fallback, uncompressed).

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use yapi_types::event::AssistantMessageEvent;
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolDeclaration, ToolResultMessage,
};
use yapi_types::model::{Model, OpenAiResponsesCompat};

use super::sanitize_id_part;
use crate::cost::calculate_cost;
use crate::hash::short_hash;
use crate::http::{self, Failure, SseReader};
use crate::json_parse::parse_streaming_json;
use crate::schema;
use crate::stream::{
    CacheRetention, EventSender, EventStream, Provider, Request, StreamEvent, StreamOptions,
    new_output, now_ms, send_error,
};
use crate::thinking::{clamp_level, clamp_max_tokens_to_context};
use crate::transcript::{resolve_transcript, resolve_transcript_tools, transform_messages};

/// Which Responses endpoint a request goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// `openai-responses`.
    OpenAi,
    /// `azure-openai-responses`.
    Azure,
    /// `openai-codex-responses`.
    Codex,
}

impl Flavor {
    /// Providers whose `call|item` tool-call ids are kept as Responses item ids.
    fn tool_call_providers(self) -> &'static [&'static str] {
        match self {
            Flavor::Azure => &[
                "openai",
                "openai-codex",
                "opencode",
                "azure-openai-responses",
            ],
            Flavor::OpenAi | Flavor::Codex => &["openai", "openai-codex", "opencode"],
        }
    }
}
/// The Responses API rejects `max_output_tokens` below this.
const MIN_OUTPUT_TOKENS: u64 = 16;
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// The `openai-responses` wire API.
#[derive(Debug, Default)]
pub struct OpenAiResponses;

impl Provider for OpenAiResponses {
    fn api(&self) -> &str {
        "openai-responses"
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        tokio::spawn(run(request, sender, Flavor::OpenAi));
        stream
    }
}

/// The `azure-openai-responses` wire API: Responses on an Azure resource.
#[derive(Debug, Default)]
pub struct AzureOpenAiResponses;

impl Provider for AzureOpenAiResponses {
    fn api(&self) -> &str {
        "azure-openai-responses"
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        tokio::spawn(run(request, sender, Flavor::Azure));
        stream
    }
}

/// The `openai-codex-responses` wire API: ChatGPT's Codex backend, signed in
/// with a ChatGPT account.
#[derive(Debug, Default)]
pub struct OpenAiCodexResponses;

impl Provider for OpenAiCodexResponses {
    fn api(&self) -> &str {
        "openai-codex-responses"
    }

    fn stream(&self, request: Request) -> EventStream {
        let (sender, stream) = EventStream::channel();
        tokio::spawn(run(request, sender, Flavor::Codex));
        stream
    }
}

/// Compat flags with defaults applied.
#[derive(Clone, Debug)]
struct Compat {
    flavor: Flavor,
    supports_developer_role: bool,
    supports_mid_convo_system_messages: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
    supports_max_output_tokens: bool,
}

impl Compat {
    fn new(model: &Model, flavor: Flavor) -> Compat {
        let raw: OpenAiResponsesCompat = model.compat();
        let detected = if model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
        {
            "openrouter"
        } else {
            "openai"
        };
        Compat {
            flavor,
            supports_developer_role: raw.supports_developer_role.unwrap_or(true),
            supports_mid_convo_system_messages: raw
                .supports_mid_convo_system_messages
                .unwrap_or(false),
            session_affinity_format: raw
                .session_affinity_format
                .unwrap_or_else(|| detected.to_owned()),
            supports_long_cache_retention: raw.supports_long_cache_retention.unwrap_or(true),
            supports_strict_mode: raw.supports_strict_mode.unwrap_or(flavor != Flavor::OpenAi),
            supports_additional_tools: raw.supports_additional_tools.unwrap_or(false),
            supports_tool_search: raw.supports_tool_search.unwrap_or(false),
            supports_explicit_prompt_cache_mode: raw
                .supports_explicit_prompt_cache_mode
                .unwrap_or(false),
            supports_max_output_tokens: raw.supports_max_output_tokens.unwrap_or(true),
        }
    }
}

/// A non-`sk-` credential sent straight to OpenAI is a Sign in with ChatGPT token,
/// which rejects some request fields.
fn is_chatgpt_sign_in(model: &Model, api_key: &str) -> bool {
    model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && !api_key.starts_with("sk-")
}

/// pi's text signature: `{"v":1,"id":…,"phase":…}`.
fn encode_text_signature(id: Option<&str>, phase: Option<&str>) -> String {
    let mut payload = Map::new();
    payload.insert("v".into(), json!(1));
    if let Some(id) = id {
        payload.insert("id".into(), json!(id));
    }
    if let Some(phase) = phase.filter(|phase| !phase.is_empty()) {
        payload.insert("phase".into(), json!(phase));
    }
    yapi_types::json::to_string(&payload).unwrap_or_default()
}

/// The message id and phase from a text signature: v1 JSON, or a legacy plain id.
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|signature| !signature.is_empty())?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed["v"] == 1
        && let Some(id) = parsed["id"].as_str()
    {
        let phase = parsed["phase"]
            .as_str()
            .filter(|phase| matches!(*phase, "commentary" | "final_answer"))
            .map(str::to_owned);
        return Some((id.to_owned(), phase));
    }
    Some((signature.to_owned(), None))
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// An id part as the Responses API accepts it: sanitized, at most 64 units, no
/// trailing underscores.
fn normalize_id_part(part: &str) -> String {
    let sanitized: String = sanitize_id_part(part).chars().take(64).collect();
    sanitized.trim_end_matches('_').to_owned()
}

fn foreign_item_id(item_id: &str) -> String {
    format!("fc_{}", short_hash(item_id))
        .chars()
        .take(64)
        .collect()
}

/// Rewrites a tool-call id from another model into `call|item` with an `fc_` item.
fn normalize_tool_call_id(
    id: &str,
    model: &Model,
    source: &AssistantMessage,
    flavor: Flavor,
) -> String {
    if !flavor
        .tool_call_providers()
        .contains(&model.provider.as_str())
        || !id.contains('|')
    {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call = normalize_id_part(parts.next().unwrap_or_default());
    let item = parts.next().unwrap_or_default();
    let foreign = source.provider != model.provider || source.api != model.api;
    let mut item = if foreign {
        foreign_item_id(item)
    } else {
        normalize_id_part(item)
    };
    if !item.starts_with("fc_") {
        item = normalize_id_part(&format!("fc_{item}"));
    }
    format!("{call}|{item}")
}

fn convert_tools(
    tools: &[ToolDeclaration],
    compat: &Compat,
    tool_search_result: bool,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = schema::strict_sampling(tool, compat.supports_strict_mode, None)?;
            let mut function = Map::new();
            function.insert("type".into(), json!("function"));
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), schema::tool_parameters(tool, strict));
            if tool_search_result {
                function.insert("defer_loading".into(), json!(true));
            }
            if compat.supports_strict_mode {
                // Codex leaves strictness to the server unless a tool asks for it.
                let value = if !strict && compat.flavor == Flavor::Codex {
                    Value::Null
                } else {
                    json!(strict)
                };
                function.insert("strict".into(), value);
            }
            Ok(Value::Object(function))
        })
        .collect()
}

fn image_input(mime_type: &str, data: &str) -> Value {
    json!({
        "type": "input_image",
        "detail": "auto",
        "image_url": format!("data:{mime_type};base64,{data}"),
    })
}

fn tool_result_output(model: &Model, result: &ToolResultMessage) -> Value {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<Value> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image_input(&image.mime_type, &image.data)),
            _ => None,
        })
        .collect();
    if images.is_empty() || !model.accepts_images() {
        return Value::String(if !text.is_empty() {
            text
        } else if !images.is_empty() {
            "(see attached image)".into()
        } else {
            "(no tool output)".into()
        });
    }
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({"type": "input_text", "text": text}));
    }
    output.extend(images);
    Value::Array(output)
}

fn convert_assistant(
    assistant: &AssistantMessage,
    model: &Model,
    index: usize,
) -> Result<Vec<Value>, String> {
    let same_api = assistant.provider == model.provider && assistant.api == model.api;
    let same_model = same_api && assistant.model == model.id;
    let different_model = same_api && assistant.model != model.id;
    let mut output = Vec::new();
    let mut text_index = 0;
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking(thinking) => {
                if let Some(signature) = thinking
                    .thinking_signature
                    .as_deref()
                    .filter(|signature| !signature.is_empty())
                {
                    let item: Value = serde_json::from_str(signature)
                        .map_err(|err| format!("Invalid reasoning signature: {err}"))?;
                    output.push(item);
                }
            }
            ContentBlock::Text(text) => {
                let parsed = parse_text_signature(text.text_signature.as_deref());
                let fallback = if text_index == 0 {
                    format!("msg_pi_{index}")
                } else {
                    format!("msg_pi_{index}_{text_index}")
                };
                text_index += 1;
                let id = match parsed.as_ref().map(|(id, _)| id.as_str()) {
                    None | Some("") => fallback,
                    Some(id) if utf16_len(id) > 64 => format!("msg_{}", short_hash(id)),
                    Some(id) => id.to_owned(),
                };
                let mut item = Map::new();
                item.insert("type".into(), json!("message"));
                item.insert("role".into(), json!("assistant"));
                item.insert(
                    "content".into(),
                    json!([{"type": "output_text", "text": text.text, "annotations": []}]),
                );
                item.insert("status".into(), json!("completed"));
                item.insert("id".into(), json!(id));
                if let Some(phase) = parsed.and_then(|(_, phase)| phase) {
                    item.insert("phase".into(), json!(phase));
                }
                output.push(Value::Object(item));
            }
            ContentBlock::ToolCall(call) => {
                let mut parts = call.id.split('|');
                let call_id = parts.next().unwrap_or_default();
                let item_id = parts
                    .next()
                    .filter(|item| !different_model && item.starts_with("fc_"));
                let mut item = Map::new();
                item.insert("type".into(), json!("function_call"));
                if let Some(item_id) = item_id {
                    item.insert("id".into(), json!(item_id));
                }
                item.insert("call_id".into(), json!(call_id));
                item.insert("name".into(), json!(call.name));
                item.insert(
                    "arguments".into(),
                    json!(yapi_types::json::to_string(&call.arguments).unwrap_or_default()),
                );
                if same_model && let Some(namespace) = &call.namespace {
                    item.insert("namespace".into(), json!(namespace));
                }
                output.push(Value::Object(item));
            }
            ContentBlock::Image(_) => {}
        }
    }
    Ok(output)
}

/// Converts the transcript to Responses input items.
fn convert_messages(
    model: &Model,
    messages: &[Message],
    compat: &Compat,
) -> Result<Vec<Value>, String> {
    let messages = resolve_transcript(messages, compat.supports_mid_convo_system_messages);
    let normalize = |id: &str, source: &AssistantMessage| {
        normalize_tool_call_id(id, model, source, compat.flavor)
    };
    let transformed = transform_messages(&messages, model, Some(&normalize), now_ms());
    let (_, anchors) = resolve_transcript_tools(
        &messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let mut input: Vec<Value> = Vec::new();
    let mut index = 0;
    for (position, message) in transformed.iter().enumerate() {
        let leading = position == 0 && matches!(message, Message::System(_));
        match message {
            Message::System(system) => {
                let added = system.tools_added.as_deref().unwrap_or_default();
                if !leading && anchors && !added.is_empty() {
                    if compat.supports_additional_tools {
                        input.push(json!({
                            "type": "additional_tools",
                            "role": "developer",
                            "tools": convert_tools(added, compat, false)?,
                        }));
                    } else if compat.supports_tool_search {
                        let names: Vec<&str> =
                            added.iter().map(|tool| tool.name.as_str()).collect();
                        let call_id = format!(
                            "pi_tool_load_{}",
                            short_hash(&format!("system:{index}:{}", names.join(",")))
                        );
                        input.push(json!({
                            "type": "tool_search_call",
                            "call_id": call_id,
                            "execution": "client",
                            "status": "completed",
                            "arguments": {"query": names.join(" "), "limit": names.len()},
                        }));
                        input.push(json!({
                            "type": "tool_search_output",
                            "call_id": call_id,
                            "execution": "client",
                            "status": "completed",
                            "tools": convert_tools(added, compat, true)?,
                        }));
                    }
                }
                // Codex sends the initial prompt as `instructions`.
                let text = if leading && compat.flavor == Flavor::Codex {
                    String::new()
                } else if leading {
                    system.text()
                } else {
                    system.render_update()
                };
                if !text.is_empty() {
                    input.push(json!({"role": instruction_role, "content": text}));
                }
            }
            Message::User(user) => match &user.content {
                Content::Text(text) => {
                    input.push(json!({
                        "role": "user",
                        "content": [{"type": "input_text", "text": text}],
                    }));
                }
                Content::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => {
                                Some(json!({"type": "input_text", "text": text.text}))
                            }
                            ContentBlock::Image(image) => {
                                Some(image_input(&image.mime_type, &image.data))
                            }
                            _ => None,
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    input.push(json!({"role": "user", "content": content}));
                }
            },
            Message::Assistant(assistant) => {
                let output = convert_assistant(assistant, model, index)?;
                if output.is_empty() {
                    continue;
                }
                input.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_output(model, result),
                }));
            }
            _ => {}
        }
        if !leading {
            index += 1;
        }
    }
    Ok(input)
}

fn build_params(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
    max_tokens: u64,
    effort: Option<ThinkingLevel>,
) -> Result<Value, String> {
    let (request_tools, _) = resolve_transcript_tools(
        messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let input = convert_messages(model, messages, compat)?;
    let retention = options.resolved_cache_retention();
    let omit = options
        .api_key
        .as_deref()
        .is_some_and(|key| is_chatgpt_sign_in(model, key));
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("input".into(), Value::Array(input));
    params.insert("stream".into(), json!(true));
    if retention != CacheRetention::None
        && let Some(session) = &options.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            json!(session.chars().take(64).collect::<String>()),
        );
    }
    if !omit {
        if retention == CacheRetention::Long
            && compat.supports_long_cache_retention
            && !compat.supports_explicit_prompt_cache_mode
        {
            params.insert("prompt_cache_retention".into(), json!("24h"));
        }
        if compat.supports_explicit_prompt_cache_mode {
            match retention {
                CacheRetention::None => {
                    params.insert("prompt_cache_options".into(), json!({"mode": "explicit"}));
                }
                CacheRetention::Long if compat.supports_long_cache_retention => {
                    params.insert("prompt_cache_options".into(), json!({"ttl": "30m"}));
                }
                _ => {}
            }
        }
    }
    params.insert("store".into(), json!(false));
    if max_tokens > 0 && compat.supports_max_output_tokens && !omit {
        params.insert(
            "max_output_tokens".into(),
            json!(max_tokens.max(MIN_OUTPUT_TOKENS)),
        );
    }
    if let Some(temperature) = options.temperature.filter(|_| !omit) {
        params.insert("temperature".into(), json!(temperature));
    }
    if !request_tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_tools(&request_tools, compat, false)?),
        );
    }
    if model.reasoning {
        if let Some(level) = effort {
            let effort = match model.thinking_level_value(level) {
                Some(Some(value)) => value.to_owned(),
                _ => level.as_str().to_owned(),
            };
            params.insert(
                "reasoning".into(),
                json!({"effort": effort, "summary": "auto"}),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if model.provider != "github-copilot" {
            match model.thinking_level_value(ThinkingLevel::Off) {
                Some(None) => {}
                Some(Some(value)) => {
                    params.insert("reasoning".into(), json!({"effort": value}));
                }
                None => {
                    params.insert("reasoning".into(), json!({"effort": "none"}));
                }
            }
        }
        if model.provider == "xai" {
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    for (key, value) in model.sampling_params.iter().flatten() {
        params.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(params))
}

fn service_tier_multiplier(model: &Model, tier: Option<&str>, flavor: Flavor) -> f64 {
    match (flavor, tier) {
        (Flavor::Azure, _) => 1.0,
        (_, Some("flex")) => 0.5,
        (Flavor::OpenAi, Some("fast")) | (_, Some("priority")) if model.id == "gpt-5.5" => 2.5,
        (Flavor::OpenAi, Some("fast")) | (_, Some("priority")) => 2.0,
        _ => 1.0,
    }
}

/// pi's `clampOpenAIPromptCacheKey`: at most 64 characters.
fn prompt_cache_key(session: &str) -> String {
    session.chars().take(64).collect()
}

/// Request fields for `azure-openai-responses`.
fn build_azure_params(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
    max_tokens: u64,
    effort: Option<ThinkingLevel>,
    deployment: &str,
) -> Result<Value, String> {
    let (request_tools, _) = resolve_transcript_tools(
        messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let input = convert_messages(model, messages, compat)?;
    let mut params = Map::new();
    params.insert("model".into(), json!(deployment));
    params.insert("input".into(), Value::Array(input));
    params.insert("stream".into(), json!(true));
    if let Some(session) = &options.session_id {
        params.insert("prompt_cache_key".into(), json!(prompt_cache_key(session)));
    }
    params.insert("store".into(), json!(false));
    if max_tokens > 0 {
        params.insert(
            "max_output_tokens".into(),
            json!(max_tokens.max(MIN_OUTPUT_TOKENS)),
        );
    }
    if let Some(temperature) = options.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    if !request_tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_tools(&request_tools, compat, false)?),
        );
    }
    if model.reasoning {
        if let Some(level) = effort {
            let effort = match model.thinking_level_value(level) {
                Some(Some(value)) => value.to_owned(),
                _ => level.as_str().to_owned(),
            };
            params.insert(
                "reasoning".into(),
                json!({"effort": effort, "summary": "auto"}),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else {
            match model.thinking_level_value(ThinkingLevel::Off) {
                Some(None) => {}
                Some(Some(value)) => {
                    params.insert("reasoning".into(), json!({"effort": value}));
                }
                None => {
                    params.insert("reasoning".into(), json!({"effort": "none"}));
                }
            }
        }
    }
    for (key, value) in model.sampling_params.iter().flatten() {
        params.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(params))
}

/// The request body for `openai-codex-responses`: the initial system prompt
/// goes in `instructions`, and Codex's fixed fields follow it.
fn build_codex_body(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
    effort: Option<ThinkingLevel>,
) -> Result<Value, String> {
    let (request_tools, _) = resolve_transcript_tools(
        messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let input = convert_messages(model, messages, compat)?;
    let instructions = match messages.first() {
        Some(Message::System(system)) => system.text(),
        _ => String::new(),
    };
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert(
        "instructions".into(),
        json!(if instructions.is_empty() {
            "You are a helpful assistant."
        } else {
            instructions.as_str()
        }),
    );
    body.insert("input".into(), Value::Array(input));
    body.insert("text".into(), json!({"verbosity": "low"}));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if options.resolved_cache_retention() != CacheRetention::None
        && let Some(session) = &options.session_id
    {
        body.insert("prompt_cache_key".into(), json!(prompt_cache_key(session)));
    }
    body.insert("tool_choice".into(), json!("auto"));
    body.insert("parallel_tool_calls".into(), json!(true));
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if !request_tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_tools(&request_tools, compat, false)?),
        );
    }
    if let Some(level) = effort {
        let effort = match model.thinking_level_value(level) {
            Some(Some(value)) => Some(value.to_owned()),
            Some(None) => None,
            None => Some(level.as_str().to_owned()),
        };
        if let Some(effort) = effort {
            body.insert(
                "reasoning".into(),
                json!({"effort": effort, "summary": "auto"}),
            );
        }
    } else if model.reasoning {
        match model.thinking_level_value(ThinkingLevel::Off) {
            Some(None) => {}
            Some(Some(value)) => {
                body.insert("reasoning".into(), json!({"effort": value}));
            }
            None => {
                body.insert("reasoning".into(), json!({"effort": "none"}));
            }
        }
    }
    Ok(Value::Object(body))
}

/// pi's `resolveAzureConfig`: the base URL from `AZURE_OPENAI_BASE_URL`, else
/// `AZURE_OPENAI_RESOURCE_NAME`, else the model, with `/openai/v1` added to
/// bare Azure hosts; and the API version.
fn azure_endpoint(model: &Model) -> Result<String, String> {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let version = env("AZURE_OPENAI_API_VERSION").unwrap_or_else(|| "v1".into());
    let base = env("AZURE_OPENAI_BASE_URL")
        .or_else(|| env("AZURE_OPENAI_RESOURCE_NAME").map(|name| format!("https://{name}.openai.azure.com/openai/v1")))
        .or_else(|| Some(model.base_url.clone()).filter(|url| !url.is_empty()))
        .ok_or_else(|| "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.".to_owned())?;
    let trimmed = base.trim().trim_end_matches('/');
    let mut url =
        url::Url::parse(trimmed).map_err(|_| format!("Invalid Azure OpenAI base URL: {base}"))?;
    let host = url.host_str().unwrap_or_default().to_owned();
    let azure = [
        ".openai.azure.com",
        ".cognitiveservices.azure.com",
        ".ai.azure.com",
    ]
    .iter()
    .any(|suffix| host.ends_with(suffix));
    let path = url.path().trim_end_matches('/').to_owned();
    if azure && matches!(path.as_str(), "" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    let base = url.to_string();
    let base = base.trim_end_matches('/');
    let separator = if base.contains('?') { '&' } else { '?' };
    Ok(format!("{base}/responses{separator}api-version={version}"))
}

/// The deployment for a model: `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`
/// (`model=deployment,...`), else the model id.
fn azure_deployment(model: &Model) -> String {
    std::env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")
        .ok()
        .and_then(|map| {
            map.split(',').find_map(|entry| {
                let (id, deployment) = entry.trim().split_once('=')?;
                (id.trim() == model.id && !deployment.trim().is_empty())
                    .then(|| deployment.trim().to_owned())
            })
        })
        .unwrap_or_else(|| model.id.clone())
}

/// `<base>/codex/responses`, as pi's `resolveCodexUrl`.
fn codex_url(base_url: &str) -> String {
    let base = if base_url.trim().is_empty() {
        "https://chatgpt.com/backend-api"
    } else {
        base_url
    };
    let base = base.trim_end_matches('/');
    if base.ends_with("/codex/responses") {
        base.to_owned()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}

/// The ChatGPT account id in a Codex access token.
fn codex_account_id(token: &str) -> Result<String, String> {
    crate::auth::codex::decode_jwt(token)
        .and_then(|payload| {
            payload["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| "Failed to extract accountId from token".to_owned())
}

/// pi's `parseErrorResponse` for Codex: a usage-limit notice, else the
/// error's message, else the body.
fn codex_error_message(status: u16, body: &str) -> String {
    let mut message = if body.is_empty() {
        "Request failed".to_owned()
    } else {
        body.to_owned()
    };
    let mut friendly = None;
    if let Ok(parsed) = serde_json::from_str::<Value>(body)
        && let Some(error) = parsed.get("error").filter(|error| is_truthy(error))
    {
        let code = [&error["code"], &error["type"]]
            .into_iter()
            .find_map(|value| value.as_str().filter(|text| !text.is_empty()))
            .unwrap_or_default()
            .to_lowercase();
        let limited = [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|pattern| code.contains(pattern));
        if limited || status == 429 {
            let plan = error["plan_type"]
                .as_str()
                .map_or_else(String::new, |plan| {
                    format!(" ({} plan)", plan.to_lowercase())
                });
            let when = error["resets_at"]
                .as_f64()
                .filter(|at| *at != 0.0)
                .map_or_else(String::new, |at| {
                    let minutes = ((at * 1000.0 - now_ms() as f64) / 60000.0).round().max(0.0);
                    format!(" Try again in ~{minutes} min.")
                });
            friendly = Some(
                format!("You have hit your ChatGPT usage limit{plan}.{when}")
                    .trim()
                    .to_owned(),
            );
        }
        if let Some(text) = error["message"].as_str().filter(|text| !text.is_empty()) {
            message = text.to_owned();
        } else if let Some(friendly) = &friendly {
            message = friendly.clone();
        }
    }
    friendly.unwrap_or(message)
}

async fn run(request: Request, sender: EventSender, flavor: Flavor) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let compat = Compat::new(&model, flavor);
    let normalized = resolve_transcript(&messages, compat.supports_mid_convo_system_messages);
    let output = new_output(&model, now_ms());
    let fail = |output, message: String| send_error(&sender, output, &options.cancel, message);
    let api_key = match options.api_key.clone().filter(|key| !key.is_empty()) {
        Some(key) => key,
        None if flavor == Flavor::OpenAi
            && (options.has_header("authorization")
                || options.has_header("cf-aig-authorization")) =>
        {
            "unused".to_owned()
        }
        None => {
            fail(
                output,
                format!("No API key for provider: {}", model.provider),
            );
            return;
        }
    };
    let max_tokens = clamp_max_tokens_to_context(
        &model,
        &messages,
        options.max_tokens.unwrap_or(model.max_tokens),
    );
    let effort = options
        .reasoning
        .map(|level| clamp_level(&model, level))
        .filter(|level| *level != ThinkingLevel::Off);

    let mut headers: IndexMap<String, Option<String>> = IndexMap::new();
    let mut set = |name: &str, value: Option<String>| {
        headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
        headers.insert(name.to_owned(), value);
    };
    let prepared = match flavor {
        Flavor::OpenAi => build_params(&model, &normalized, &options, &compat, max_tokens, effort)
            .map(|params| {
                set("authorization", Some(format!("Bearer {api_key}")));
                set("content-type", Some("application/json".into()));
                set("accept", Some("application/json".into()));
                for (key, value) in model.headers.iter().flatten() {
                    set(key, Some(value.clone()));
                }
                if model.provider == "github-copilot" {
                    for (key, value) in super::copilot_headers(&messages) {
                        set(key, Some(value));
                    }
                }
                if options.resolved_cache_retention() != CacheRetention::None
                    && let Some(session) = &options.session_id
                {
                    if compat.session_affinity_format == "openrouter" {
                        set("x-session-id", Some(session.clone()));
                    } else {
                        if compat.session_affinity_format == "openai" {
                            set("session_id", Some(session.clone()));
                        }
                        set("x-client-request-id", Some(session.clone()));
                    }
                }
                for (key, value) in &options.headers {
                    set(key, value.clone());
                }
                (
                    params,
                    format!("{}/responses", model.base_url.trim_end_matches('/')),
                )
            }),
        Flavor::Azure => azure_endpoint(&model).and_then(|url| {
            let deployment = azure_deployment(&model);
            build_azure_params(
                &model,
                &normalized,
                &options,
                &compat,
                max_tokens,
                effort,
                &deployment,
            )
            .map(|params| {
                set("api-key", Some(api_key.clone()));
                set("content-type", Some("application/json".into()));
                set("accept", Some("application/json".into()));
                for (key, value) in model.headers.iter().flatten() {
                    set(key, Some(value.clone()));
                }
                for (key, value) in &options.headers {
                    set(key, value.clone());
                }
                (params, url)
            })
        }),
        Flavor::Codex => codex_account_id(&api_key).and_then(|account| {
            build_codex_body(&model, &normalized, &options, &compat, effort).map(|body| {
                for (key, value) in model.headers.iter().flatten() {
                    set(key, Some(value.clone()));
                }
                for (key, value) in &options.headers {
                    set(key, value.clone());
                }
                set("authorization", Some(format!("Bearer {api_key}")));
                set("chatgpt-account-id", Some(account));
                // The value the Codex backend expects from this client id.
                set("originator", Some("pi".into()));
                set("openai-beta", Some("responses=experimental".into()));
                set("accept", Some("text/event-stream".into()));
                set("content-type", Some("application/json".into()));
                if options.resolved_cache_retention() != CacheRetention::None
                    && let Some(session) = &options.session_id
                {
                    let session = prompt_cache_key(session);
                    set("session-id", Some(session.clone()));
                    set("x-client-request-id", Some(session));
                }
                (body, codex_url(&model.base_url))
            })
        }),
    };
    let (params, url) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            fail(output, message);
            return;
        }
    };
    let body = match yapi_types::json::to_string(&params) {
        Ok(body) => body,
        Err(err) => {
            fail(output, err.to_string());
            return;
        }
    };
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
            let message = match (flavor, failure) {
                (Flavor::Codex, Failure::Status { status, body }) => {
                    codex_error_message(status, &body)
                }
                (Flavor::Codex, Failure::Aborted) => http::ABORTED_DURING_STREAM.to_owned(),
                (Flavor::Azure, failure) => failure_message(failure, "Azure OpenAI API error"),
                (_, failure) => {
                    let prefix = if model.provider == "openai" {
                        "OpenAI API error".to_owned()
                    } else {
                        format!("{} API error", model.provider)
                    };
                    failure_message(failure, &prefix)
                }
            };
            fail(output, message);
            return;
        }
    };

    sender.send(StreamEvent::Start(output.clone()));
    let mut state = State {
        flavor,
        output,
        slots: IndexMap::new(),
        partial_args: IndexMap::new(),
        reasoning_by_id: IndexMap::new(),
        terminal: false,
    };
    let result = state
        .consume(SseReader::new(response), &model, &sender, &options)
        .await
        .and_then(|()| {
            if options.cancel.is_cancelled() {
                return Err(http::ABORTED_DURING_STREAM.to_owned());
            }
            match state.output.stop_reason {
                StopReason::Pending => Err(match flavor {
                    Flavor::OpenAi => "OpenAI Responses stream ended without a stop reason",
                    Flavor::Azure => "Azure OpenAI Responses stream ended without a stop reason",
                    Flavor::Codex => "Codex stream ended without a stop reason",
                }
                .to_owned()),
                StopReason::Aborted | StopReason::Error => Err(state
                    .output
                    .error_message
                    .clone()
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "An unknown error occurred".into())),
                _ => Ok(()),
            }
        });
    match result {
        Ok(()) => sender.send(StreamEvent::Done(state.output)),
        Err(message) => send_error(&sender, state.output, &options.cancel, message),
    }
}

fn failure_message(failure: Failure, prefix: &str) -> String {
    let message = match failure {
        Failure::Status { status, body } => match serde_json::from_str::<Value>(&body) {
            Ok(json) => {
                let error = json.get("error");
                let message = http::sdk_status_message(status, error, None);
                http::provider_error_message(&message, Some(status), error, Some(prefix))
            }
            Err(_) => {
                let message = http::sdk_status_message(status, None, Some(&body));
                http::provider_error_message(&message, Some(status), None, Some(prefix))
            }
        },
        other => other.plain_message().unwrap_or_default(),
    };
    if message.contains("subscription_sharing_usage_limit_exceeded") {
        format!("{message}\nCheck your ChatGPT usage: {CHATGPT_USAGE_URL}")
    } else {
        message
    }
}

/// What an output index streams into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Thinking(usize),
    Text(usize),
    ToolCall(usize),
}

struct State {
    flavor: Flavor,
    output: AssistantMessage,
    slots: IndexMap<u64, Slot>,
    /// Argument text of tool calls still streaming, by content position.
    partial_args: IndexMap<usize, String>,
    reasoning_by_id: IndexMap<String, usize>,
    terminal: bool,
}

/// A JS template-literal rendering of a JSON value.
fn template(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

fn string_or<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback)
}

impl State {
    fn apply_phase(&mut self, item: &Value) {
        if item["type"] == "message" && item["phase"] == "final_answer" {
            self.output.stop_reason = StopReason::Stop;
        }
    }

    fn create_slot(&mut self, index: u64, item: &Value, sender: &EventSender) -> Option<Slot> {
        let position = self.output.content.len();
        let (slot, event) = match item["type"].as_str()? {
            "reasoning" => {
                self.output
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                (
                    Slot::Thinking(position),
                    AssistantMessageEvent::ThinkingStart {
                        content_index: position,
                    },
                )
            }
            "message" => {
                self.apply_phase(item);
                self.output.content.push(ContentBlock::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                (
                    Slot::Text(position),
                    AssistantMessageEvent::TextStart {
                        content_index: position,
                    },
                )
            }
            "function_call" => {
                let id = format!(
                    "{}|{}",
                    template_or_undefined(item.get("call_id")),
                    template_or_undefined(item.get("id"))
                );
                let name = item["name"].as_str().unwrap_or_default().to_owned();
                self.output.content.push(ContentBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: Map::new(),
                    thought_signature: None,
                    namespace: item["namespace"].as_str().map(str::to_owned),
                }));
                self.partial_args
                    .insert(position, string_or(&item["arguments"], "").to_owned());
                (
                    Slot::ToolCall(position),
                    AssistantMessageEvent::ToolcallStart {
                        content_index: position,
                        id,
                        tool_name: name,
                    },
                )
            }
            _ => return None,
        };
        self.slots.insert(index, slot);
        sender.update(&self.output, event);
        Some(slot)
    }

    fn thinking(&mut self, position: usize) -> Option<&mut ThinkingContent> {
        match self.output.content.get_mut(position) {
            Some(ContentBlock::Thinking(thinking)) => Some(thinking),
            _ => None,
        }
    }

    fn text(&mut self, position: usize) -> Option<&mut TextContent> {
        match self.output.content.get_mut(position) {
            Some(ContentBlock::Text(text)) => Some(text),
            _ => None,
        }
    }

    fn tool_call(&mut self, position: usize) -> Option<&mut ToolCall> {
        match self.output.content.get_mut(position) {
            Some(ContentBlock::ToolCall(call)) => Some(call),
            _ => None,
        }
    }

    fn thinking_delta(&mut self, index: u64, delta: &str, sender: &EventSender) {
        let Some(Slot::Thinking(position)) = self.slots.get(&index).copied() else {
            return;
        };
        if let Some(block) = self.thinking(position) {
            block.thinking.push_str(delta);
        }
        sender.update(
            &self.output,
            AssistantMessageEvent::ThinkingDelta {
                content_index: position,
                delta: delta.to_owned(),
            },
        );
    }

    fn text_delta(&mut self, index: u64, delta: &str, sender: &EventSender) {
        let Some(Slot::Text(position)) = self.slots.get(&index).copied() else {
            return;
        };
        if let Some(block) = self.text(position) {
            block.text.push_str(delta);
        }
        sender.update(
            &self.output,
            AssistantMessageEvent::TextDelta {
                content_index: position,
                delta: delta.to_owned(),
            },
        );
    }

    fn streaming_call(&self, index: u64) -> Option<usize> {
        match self.slots.get(&index) {
            Some(Slot::ToolCall(position)) if self.partial_args.contains_key(position) => {
                Some(*position)
            }
            _ => None,
        }
    }

    fn set_partial_args(&mut self, position: usize, partial: String) {
        let arguments = parse_streaming_json(&partial);
        self.partial_args.insert(position, partial);
        if let Some(call) = self.tool_call(position) {
            call.arguments = arguments;
        }
    }

    fn item_done(&mut self, index: u64, item: &Value, sender: &EventSender) {
        self.apply_phase(item);
        let slot = match self.slots.get(&index).copied() {
            Some(slot) => Some(slot),
            None => self.create_slot(index, item, sender),
        };
        match (item["type"].as_str(), slot) {
            (Some("reasoning"), Some(Slot::Thinking(position))) => {
                let join = |key: &str| {
                    item[key]
                        .as_array()
                        .map(|parts| {
                            parts
                                .iter()
                                .map(|part| join_part(&part["text"]))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                };
                let (summary, content) = (join("summary"), join("content"));
                let signature = yapi_types::json::to_string(item).unwrap_or_default();
                let Some(block) = self.thinking(position) else {
                    return;
                };
                if !summary.is_empty() {
                    block.thinking = summary;
                } else if !content.is_empty() {
                    block.thinking = content;
                }
                block.thinking_signature = Some(signature);
                let thinking = block.thinking.clone();
                if let Some(id) = item["id"].as_str() {
                    self.reasoning_by_id.insert(id.to_owned(), position);
                }
                sender.update(
                    &self.output,
                    AssistantMessageEvent::ThinkingEnd {
                        content_index: position,
                        content: thinking,
                    },
                );
                self.slots.shift_remove(&index);
            }
            (Some("message"), Some(Slot::Text(position))) => {
                let text: String = item["content"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                if part["type"] == "output_text" {
                                    join_part(&part["text"])
                                } else {
                                    join_part(&part["refusal"])
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let signature = encode_text_signature(
                    item["id"].as_str(),
                    item["phase"].as_str().filter(|phase| !phase.is_empty()),
                );
                let Some(block) = self.text(position) else {
                    return;
                };
                block.text = text.clone();
                block.text_signature = Some(signature);
                sender.update(
                    &self.output,
                    AssistantMessageEvent::TextEnd {
                        content_index: position,
                        content: text,
                    },
                );
                self.slots.shift_remove(&index);
            }
            (Some("function_call"), Some(Slot::ToolCall(position)))
                if self.partial_args.contains_key(&position) =>
            {
                let partial = self
                    .partial_args
                    .shift_remove(&position)
                    .unwrap_or_default();
                let source = item["arguments"]
                    .as_str()
                    .filter(|arguments| !arguments.is_empty())
                    .map(str::to_owned)
                    .unwrap_or(if partial.is_empty() {
                        "{}".to_owned()
                    } else {
                        partial
                    });
                let arguments = parse_streaming_json(&source);
                let Some(call) = self.tool_call(position) else {
                    return;
                };
                call.arguments = arguments;
                if let Some(namespace) = item["namespace"].as_str() {
                    call.namespace = Some(namespace.to_owned());
                }
                let tool_call = call.clone();
                sender.update(
                    &self.output,
                    AssistantMessageEvent::ToolcallEnd {
                        content_index: position,
                        tool_call,
                    },
                );
                self.slots.shift_remove(&index);
            }
            _ => {}
        }
    }

    /// Azure can leave `encrypted_content` out of `output_item.done` and send it
    /// only in the final response; copy it into the stored reasoning items.
    fn backfill_reasoning(&mut self, output: &Value) {
        for item in output.as_array().into_iter().flatten() {
            let Some(encrypted) = item["encrypted_content"]
                .as_str()
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            if item["type"] != "reasoning" {
                continue;
            }
            let Some(position) = item["id"]
                .as_str()
                .and_then(|id| self.reasoning_by_id.get(id).copied())
            else {
                continue;
            };
            let Some(block) = self.thinking(position) else {
                continue;
            };
            let Some(mut stored) = block
                .thinking_signature
                .as_deref()
                .and_then(|signature| serde_json::from_str::<Map<String, Value>>(signature).ok())
            else {
                continue;
            };
            if stored
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                continue;
            }
            stored.insert("encrypted_content".into(), json!(encrypted));
            block.thinking_signature = yapi_types::json::to_string(&stored).ok();
        }
    }

    fn finalize(&mut self, response: &Value, model: &Model) -> Result<(), String> {
        self.terminal = true;
        self.backfill_reasoning(&response["output"]);
        if let Some(id) = response["id"].as_str().filter(|id| !id.is_empty()) {
            self.output.response_id = Some(id.to_owned());
        }
        let usage = &response["usage"];
        if usage.is_object() {
            let count = |value: &Value| value.as_u64().unwrap_or(0);
            let cached = count(&usage["input_tokens_details"]["cached_tokens"]);
            let written = count(&usage["input_tokens_details"]["cache_write_tokens"]);
            let usage = yapi_types::message::Usage {
                input: count(&usage["input_tokens"]).saturating_sub(cached + written),
                output: count(&usage["output_tokens"]),
                cache_read: cached,
                cache_write: written,
                reasoning: Some(count(&usage["output_tokens_details"]["reasoning_tokens"])),
                total_tokens: Some(count(&usage["total_tokens"])),
                ..Default::default()
            };
            self.output.usage = usage;
        }
        calculate_cost(model, &mut self.output.usage);
        let multiplier =
            service_tier_multiplier(model, response["service_tier"].as_str(), self.flavor);
        if multiplier != 1.0 {
            let cost = &mut self.output.usage.cost;
            cost.input *= multiplier;
            cost.output *= multiplier;
            cost.cache_read *= multiplier;
            cost.cache_write *= multiplier;
            cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
        }
        let status = response["status"].as_str();
        let reason = response["incomplete_details"]["reason"].as_str();
        self.output.raw_stop_reason = match (status, reason) {
            (Some(status), Some(reason)) => Some(format!("{status}.{reason}")),
            (None, Some(reason)) => Some(format!("undefined.{reason}")),
            (status, None) => status.map(str::to_owned),
        };
        let (stop, message) = match status {
            None | Some("completed" | "in_progress" | "queued") => (StopReason::Stop, None),
            Some("incomplete") => match reason {
                Some("max_output_tokens") => (StopReason::Length, None),
                Some(reason) => (
                    StopReason::Error,
                    Some(format!("Response incomplete: {reason}")),
                ),
                None => (
                    StopReason::Error,
                    Some("Response incomplete without a provider reason".to_owned()),
                ),
            },
            Some("failed" | "cancelled") => (StopReason::Error, None),
            Some(other) => return Err(format!("Unhandled stop reason: {other}")),
        };
        self.output.stop_reason = stop;
        self.output.error_message = message;
        if stop == StopReason::Stop
            && self
                .output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            self.output.stop_reason = StopReason::ToolUse;
        }
        Ok(())
    }

    /// pi's `mapCodexEvents`: Codex errors and its terminal events. Returns
    /// `true` when the event was handled.
    fn handle_codex(&mut self, event: &Value, model: &Model) -> Result<bool, String> {
        match event["type"].as_str().unwrap_or_default() {
            "error" => {
                let nested = &event["error"];
                let pick = |name: &str| {
                    event[name]
                        .as_str()
                        .or_else(|| nested[name].as_str())
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                };
                let detail = pick("message")
                    .or_else(|| pick("code"))
                    .unwrap_or_else(|| yapi_types::json::to_string(event).unwrap_or_default());
                Err(format!("Codex error: {detail}"))
            }
            "response.failed" => Err(event["response"]["error"]["message"]
                .as_str()
                .filter(|text| !text.is_empty())
                .unwrap_or("Codex response failed")
                .to_owned()),
            "response.done" | "response.completed" | "response.incomplete" => {
                let mut response = event["response"].clone();
                if let Some(end_turn) = response["end_turn"].as_bool() {
                    self.output.end_turn = Some(end_turn);
                }
                let known = [
                    "completed",
                    "incomplete",
                    "failed",
                    "cancelled",
                    "queued",
                    "in_progress",
                ];
                if let Some(object) = response.as_object_mut()
                    && !object
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| known.contains(&status))
                {
                    object.remove("status");
                }
                self.finalize(&response, model)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn handle(&mut self, event: &Value, model: &Model, sender: &EventSender) -> Result<(), String> {
        if self.flavor == Flavor::Codex && self.handle_codex(event, model)? {
            return Ok(());
        }
        let index = event["output_index"].as_u64().unwrap_or(0);
        let delta = event["delta"].as_str().unwrap_or_default();
        match event["type"].as_str().unwrap_or_default() {
            "response.created" => {
                self.output.response_id = event["response"]["id"].as_str().map(str::to_owned);
            }
            "response.output_item.added" => {
                self.create_slot(index, &event["item"], sender);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.thinking_delta(index, delta, sender);
            }
            "response.reasoning_summary_part.done" => self.thinking_delta(index, "\n\n", sender),
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text_delta(index, delta, sender);
            }
            "response.function_call_arguments.delta" => {
                if let Some(position) = self.streaming_call(index) {
                    let partial = format!("{}{delta}", self.partial_args[&position]);
                    self.set_partial_args(position, partial);
                    sender.update(
                        &self.output,
                        AssistantMessageEvent::ToolcallDelta {
                            content_index: position,
                            delta: delta.to_owned(),
                        },
                    );
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(position) = self.streaming_call(index) {
                    let previous = self.partial_args[&position].clone();
                    let arguments = event["arguments"].as_str().unwrap_or_default().to_owned();
                    self.set_partial_args(position, arguments.clone());
                    if let Some(rest) = arguments
                        .strip_prefix(previous.as_str())
                        .filter(|rest| !rest.is_empty())
                    {
                        sender.update(
                            &self.output,
                            AssistantMessageEvent::ToolcallDelta {
                                content_index: position,
                                delta: rest.to_owned(),
                            },
                        );
                    }
                }
            }
            "response.output_item.done" => self.item_done(index, &event["item"], sender),
            "response.completed" | "response.incomplete" => {
                self.finalize(&event["response"], model)?;
            }
            "error" => {
                return Err(format!(
                    "Error Code {}: {}",
                    template_or_undefined(event.get("code")),
                    template_or_undefined(event.get("message"))
                ));
            }
            "response.failed" => {
                self.terminal = true;
                let response = &event["response"];
                self.output.raw_stop_reason = response["status"].as_str().map(str::to_owned);
                let error = &response["error"];
                let reason = &response["incomplete_details"]["reason"];
                return Err(if is_truthy(error) {
                    format!(
                        "{}: {}",
                        truthy_or(&error["code"], "unknown"),
                        truthy_or(&error["message"], "no message")
                    )
                } else if is_truthy(reason) {
                    format!("incomplete: {}", template(reason))
                } else {
                    "Unknown error (no error details in response)".to_owned()
                });
            }
            _ => {}
        }
        Ok(())
    }

    async fn consume(
        &mut self,
        mut reader: SseReader,
        model: &Model,
        sender: &EventSender,
        options: &StreamOptions,
    ) -> Result<(), String> {
        loop {
            // The SDK ends the iteration quietly when the request is aborted.
            let chunk = match http::next_openai_chunk(&mut reader, &options.cancel).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) if options.cancel.is_cancelled() => break,
                Err(message) => return Err(message),
            };
            self.handle(&chunk, model, sender)?;
            // Codex ends its stream at the first terminal response.
            if self.flavor == Flavor::Codex && self.terminal {
                break;
            }
        }
        if !self.terminal {
            return Err("OpenAI Responses stream ended before a terminal response event".into());
        }
        if self.output.stop_reason == StopReason::ToolUse
            && let Some(call) =
                self.output
                    .content
                    .iter()
                    .enumerate()
                    .find_map(|(position, block)| match block {
                        ContentBlock::ToolCall(call)
                            if self.partial_args.contains_key(&position) =>
                        {
                            Some(call)
                        }
                        _ => None,
                    })
        {
            return Err(format!(
                "OpenAI Responses stream completed with an unfinished tool call: {} ({})",
                call.name, call.id
            ));
        }
        Ok(())
    }
}

/// An element as `Array.prototype.join` renders it: null and undefined are empty.
fn join_part(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => template(other),
    }
}

fn template_or_undefined(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_owned(), template)
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn truthy_or(value: &Value, fallback: &str) -> String {
    if is_truthy(value) {
        template(value)
    } else {
        fallback.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_round_trip() {
        let signature = encode_text_signature(Some("msg_1"), Some("final_answer"));
        assert_eq!(signature, r#"{"v":1,"id":"msg_1","phase":"final_answer"}"#);
        assert_eq!(
            parse_text_signature(Some(&signature)),
            Some(("msg_1".into(), Some("final_answer".into())))
        );
        assert_eq!(
            parse_text_signature(Some("legacy")),
            Some(("legacy".into(), None))
        );
        assert_eq!(parse_text_signature(Some("")), None);
    }

    #[test]
    fn normalizes_ids_for_responses() {
        assert_eq!(normalize_id_part("call.1__"), "call_1");
        assert_eq!(normalize_id_part("a😀b"), "a__b");
        assert_eq!(
            foreign_item_id("item"),
            format!("fc_{}", short_hash("item"))
        );
    }
}
