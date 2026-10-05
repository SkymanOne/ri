//! `openai-completions`: the OpenAI Chat Completions API and the many compatible
//! endpoints, with per-provider compat flags.
//!
//! Port of `packages/ai/src/api/openai-completions.ts` in pi `v1.0.0`. Not yet
//! ported: grammar-constrained custom tools and GitHub Copilot dynamic headers.

use serde_json::{Map, Value, json};
use yapi_types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, ThinkingContent, ThinkingLevel, ToolCall,
    ToolDeclaration, Usage,
};
use yapi_types::model::{Model, OpenAiCompletionsCompat};

use super::sanitize_id_part;
use crate::cost::calculate_cost;
use crate::http::{self, Failure, Headers, SseReader};
use crate::json_parse::parse_streaming_json;
use crate::schema;
use crate::stream::{
    CacheRetention, EventSender, Request, StreamEvent, StreamOptions, new_output, now_ms,
    send_error,
};
use crate::thinking::{
    MIN_ANSWER_TOKENS, budget_for_level, clamp_level, clamp_max_tokens_to_context,
};
use crate::transcript::{resolve_transcript, resolve_transcript_tools, transform_messages};

/// Compat flags with detected defaults applied.
#[derive(Clone, Debug)]
struct Compat {
    supports_store: bool,
    supports_developer_role: bool,
    supports_reasoning_effort: bool,
    supports_usage_in_streaming: bool,
    supports_finish_reason: bool,
    max_tokens_field: String,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content_on_assistant_messages: bool,
    thinking_format: String,
    open_router_routing: Option<Map<String, Value>>,
    vercel_gateway_routing: Option<Map<String, Value>>,
    chat_template_kwargs: Map<String, Value>,
    chat_template_args: Map<String, Value>,
    zai_tool_stream: bool,
    thinking_token_budget_field: Option<String>,
    supports_strict_mode: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_additions: bool,
    cache_control_format: Option<String>,
    send_session_affinity_headers: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    vllm_priority: Option<f64>,
}

impl Compat {
    fn new(model: &Model) -> Compat {
        let provider = model.provider.as_str();
        let url = model.base_url.as_str();
        let is_zai = matches!(provider, "zai" | "zai-coding-cn")
            || url.contains("api.z.ai")
            || url.contains("open.bigmodel.cn");
        let is_together = provider == "together"
            || url.contains("api.together.ai")
            || url.contains("api.together.xyz");
        let is_moonshot =
            matches!(provider, "moonshotai" | "moonshotai-cn") || url.contains("api.moonshot.");
        let is_open_router = provider == "openrouter" || url.contains("openrouter.ai");
        let is_cf_workers =
            provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
        let is_cf_gateway =
            provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let is_nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
        let is_ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
        let is_cerebras = provider == "cerebras" || url.contains("cerebras.ai");
        let is_deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
        let is_grok = provider == "xai" || url.contains("api.x.ai");
        let non_standard = is_nvidia
            || is_cerebras
            || is_grok
            || is_together
            || url.contains("chutes.ai")
            || is_deepseek
            || is_zai
            || is_moonshot
            || provider == "opencode"
            || url.contains("opencode.ai")
            || is_cf_workers
            || is_cf_gateway
            || is_ant_ling;
        let use_max_tokens = url.contains("chutes.ai")
            || is_deepseek
            || is_moonshot
            || is_cf_gateway
            || is_together
            || is_nvidia
            || is_ant_ling
            || is_zai;
        let developer_role_model = is_open_router
            && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let detected_format = if is_deepseek {
            "deepseek"
        } else if is_zai {
            "zai"
        } else if is_together {
            "together"
        } else if is_ant_ling {
            "ant-ling"
        } else if is_open_router {
            "openrouter"
        } else {
            "openai"
        };
        let raw: OpenAiCompletionsCompat = model.compat();
        let has_raw = model.compat.is_some();
        Compat {
            supports_store: raw.supports_store.unwrap_or(!non_standard),
            supports_developer_role: raw
                .supports_developer_role
                .unwrap_or(developer_role_model || (!non_standard && !is_open_router)),
            supports_reasoning_effort: raw.supports_reasoning_effort.unwrap_or(
                !is_grok
                    && !is_zai
                    && !is_moonshot
                    && !is_together
                    && !is_cf_gateway
                    && !is_nvidia
                    && !is_ant_ling,
            ),
            supports_usage_in_streaming: raw.supports_usage_in_streaming.unwrap_or(true),
            supports_finish_reason: raw.supports_finish_reason.unwrap_or(true),
            max_tokens_field: raw.max_tokens_field.unwrap_or_else(|| {
                if use_max_tokens {
                    "max_tokens"
                } else {
                    "max_completion_tokens"
                }
                .to_owned()
            }),
            requires_tool_result_name: raw.requires_tool_result_name.unwrap_or(false),
            requires_assistant_after_tool_result: raw
                .requires_assistant_after_tool_result
                .unwrap_or(false),
            requires_thinking_as_text: raw.requires_thinking_as_text.unwrap_or(false),
            requires_reasoning_content_on_assistant_messages: raw
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(is_deepseek),
            thinking_format: raw
                .thinking_format
                .unwrap_or_else(|| detected_format.to_owned()),
            open_router_routing: if has_raw {
                raw.open_router_routing
            } else {
                None
            },
            vercel_gateway_routing: raw.vercel_gateway_routing,
            chat_template_kwargs: raw.chat_template_kwargs.unwrap_or_default(),
            chat_template_args: raw.chat_template_args.unwrap_or_default(),
            zai_tool_stream: raw.zai_tool_stream.unwrap_or(false),
            thinking_token_budget_field: raw.thinking_token_budget_field.or_else(|| {
                (raw.supports_thinking_token_budget == Some(true))
                    .then(|| "thinking_token_budget".to_owned())
            }),
            supports_strict_mode: raw.supports_strict_mode.unwrap_or(false),
            supports_mid_convo_system_messages: raw
                .supports_mid_convo_system_messages
                .unwrap_or(false),
            supports_mid_convo_tool_additions: raw
                .supports_mid_convo_tool_additions
                .unwrap_or(false),
            cache_control_format: raw.cache_control_format.or_else(|| {
                (provider == "openrouter" && model.id.starts_with("anthropic/"))
                    .then(|| "anthropic".to_owned())
            }),
            send_session_affinity_headers: raw
                .send_session_affinity_headers
                .unwrap_or(is_open_router),
            session_affinity_format: raw.session_affinity_format.unwrap_or_else(|| {
                if is_open_router {
                    "openrouter"
                } else {
                    "openai"
                }
                .to_owned()
            }),
            supports_long_cache_retention: raw.supports_long_cache_retention.unwrap_or(
                !(is_together || is_cf_workers || is_cf_gateway || is_nvidia || is_ant_ling),
            ),
            vllm_priority: raw.vllm_priority,
        }
    }
}

fn normalize_tool_call_id(id: &str, provider: &str) -> String {
    if let Some((call, item)) = id.split_once('|') {
        let call = sanitize_id_part(call);
        let item = sanitize_id_part(item);
        let combined = if item.is_empty() {
            call.clone()
        } else {
            format!("{call}_{item}")
        };
        if combined.len() <= 40 {
            return combined;
        }
        let hash: String = crate::hash::short_hash(id).chars().take(8).collect();
        let prefix: String = call
            .chars()
            .take(40usize.saturating_sub(hash.len() + 1).max(1))
            .collect();
        return format!("{prefix}_{hash}");
    }
    if provider == "openai" && id.len() > 40 {
        return id.chars().take(40).collect();
    }
    id.to_owned()
}

fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(object) = detail.as_object() else {
        return false;
    };
    let common = object
        .get("id")
        .is_none_or(|id| id.is_null() || id.is_string())
        && object.get("format").is_none_or(Value::is_string)
        && object.get("index").is_none_or(Value::is_number);
    common
        && match object.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => object.get("summary").is_some_and(Value::is_string),
            Some("reasoning.encrypted") => object.get("data").is_some_and(Value::is_string),
            Some("reasoning.text") => {
                object.get("text").is_some_and(Value::is_string)
                    && object
                        .get("signature")
                        .is_none_or(|signature| signature.is_null() || signature.is_string())
            }
            _ => false,
        }
}

fn parse_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let details = parsed.as_array()?;
    (!details.is_empty() && details.iter().all(is_reasoning_detail)).then(|| details.clone())
}

fn legacy_encrypted_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    (is_reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed["id"].as_str().is_some_and(|id| !id.is_empty())
        && parsed["data"].as_str().is_some_and(|data| !data.is_empty()))
    .then_some(parsed)
}

fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let fill = |target: &mut Value, source: &Value| {
        if target.get("id").is_none_or(Value::is_null) && source.get("id").is_some() {
            target["id"] = source["id"].clone();
        }
        if target
            .get("format")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            && let Some(format) = source.get("format")
        {
            target["format"] = format.clone();
        }
        if target.get("index").is_none() && source.get("index").is_some() {
            target["index"] = source["index"].clone();
        }
    };
    if let Some(last) = details.last_mut() {
        if detail["type"] == "reasoning.text" && last["type"] == "reasoning.text" {
            let text = format!(
                "{}{}",
                last["text"].as_str().unwrap_or_default(),
                detail["text"].as_str().unwrap_or_default()
            );
            last["text"] = Value::from(text);
            if last
                .get("signature")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                && let Some(signature) = detail.get("signature")
            {
                last["signature"] = signature.clone();
            }
            fill(last, detail);
            return;
        }
        if detail["type"] == "reasoning.summary" && last["type"] == "reasoning.summary" {
            let summary = format!(
                "{}{}",
                last["summary"].as_str().unwrap_or_default(),
                detail["summary"].as_str().unwrap_or_default()
            );
            last["summary"] = Value::from(summary);
            fill(last, detail);
            return;
        }
    }
    details.push(detail.clone());
}

fn convert_tools(tools: &[ToolDeclaration], compat: &Compat) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = schema::strict_sampling(tool, compat.supports_strict_mode, None)?;
            let mut function = Map::new();
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), schema::tool_parameters(tool, strict));
            if compat.supports_strict_mode {
                function.insert("strict".into(), json!(strict));
            }
            Ok(json!({"type": "function", "function": function}))
        })
        .collect()
}

fn image_url(mime_type: &str, data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}})
}

fn convert_messages(
    model: &Model,
    messages: &[Message],
    compat: &Compat,
) -> Result<Vec<Value>, String> {
    let provider = model.provider.clone();
    let normalize = move |id: &str, _: &AssistantMessage| normalize_tool_call_id(id, &provider);
    let transformed = transform_messages(messages, model, Some(&normalize), now_ms());
    let (_, anchors) = resolve_transcript_tools(
        messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let mut params: Vec<Value> = Vec::new();
    let mut last_role: Option<&str> = None;
    let mut index = 0;
    while index < transformed.len() {
        let message = &transformed[index];
        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(message, Message::User(_))
        {
            params.push(
                json!({"role": "assistant", "content": "I have processed the tool results."}),
            );
        }
        match message {
            Message::System(system) => {
                let added = system.tools_added.clone().unwrap_or_default();
                if index > 0 && anchors && !added.is_empty() {
                    params.push(json!({"role": "system", "tools": convert_tools(&added, compat)?}));
                }
                let text = if index == 0 {
                    system.text()
                } else {
                    system.render_update()
                };
                if !text.is_empty() {
                    params.push(json!({"role": instruction_role, "content": text}));
                }
                last_role = Some("system");
            }
            Message::User(user) => {
                match &user.content {
                    yapi_types::message::Content::Text(text) => {
                        params.push(json!({"role": "user", "content": text}));
                    }
                    yapi_types::message::Content::Blocks(blocks) => {
                        let content: Vec<Value> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) if !text.text.is_empty() => {
                                    Some(json!({"type": "text", "text": text.text}))
                                }
                                ContentBlock::Image(image) => {
                                    Some(image_url(&image.mime_type, &image.data))
                                }
                                _ => None,
                            })
                            .collect();
                        if content.is_empty() {
                            index += 1;
                            continue;
                        }
                        params.push(json!({"role": "user", "content": content}));
                    }
                }
                last_role = Some("user");
            }
            Message::Assistant(assistant) => {
                let mut out = Map::new();
                out.insert("role".into(), json!("assistant"));
                out.insert(
                    "content".into(),
                    if compat.requires_assistant_after_tool_result {
                        json!("")
                    } else {
                        Value::Null
                    },
                );
                let text_parts: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                            Some(text.text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                let text = text_parts.concat();
                let thinking: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking(thinking) => Some(thinking),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<&ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .collect();
                let signed = thinking
                    .iter()
                    .find_map(|block| parse_reasoning_details(block.thinking_signature.as_deref()));
                let legacy: Vec<Value> = calls
                    .iter()
                    .filter_map(|call| legacy_encrypted_detail(call.thought_signature.as_deref()))
                    .collect();
                let preserved = signed.or((!legacy.is_empty()).then_some(legacy));
                let non_empty: Vec<&&ThinkingContent> = thinking
                    .iter()
                    .filter(|block| !block.thinking.trim().is_empty())
                    .collect();
                if !non_empty.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty
                            .iter()
                            .map(|block| block.thinking.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut content = vec![json!({"type": "text", "text": thinking_text})];
                        content.extend(
                            text_parts
                                .iter()
                                .map(|text| json!({"type": "text", "text": text})),
                        );
                        out.insert("content".into(), Value::Array(content));
                    } else {
                        if !text.is_empty() {
                            out.insert("content".into(), json!(text));
                        }
                        if preserved.is_none() {
                            let mut signature = non_empty[0].thinking_signature.clone();
                            if model.provider == "opencode-go"
                                && signature.as_deref() == Some("reasoning")
                            {
                                signature = Some("reasoning_content".into());
                            }
                            if let Some(field) = signature.filter(|field| {
                                matches!(
                                    field.as_str(),
                                    "reasoning" | "reasoning_content" | "reasoning_text"
                                )
                            }) {
                                let joined = non_empty
                                    .iter()
                                    .map(|block| block.thinking.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                out.insert(field, json!(joined));
                            }
                        }
                    }
                } else if !text.is_empty() {
                    out.insert("content".into(), json!(text));
                }
                if !calls.is_empty() {
                    let tool_calls: Vec<Value> = calls
                        .iter()
                        .map(|call| {
                            json!({"id": call.id, "type": "function", "function": {
                                "name": call.name,
                                "arguments": yapi_types::json::to_string(&call.arguments).unwrap_or_default()}})
                        })
                        .collect();
                    out.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                if let Some(details) = preserved {
                    out.insert("reasoning_details".into(), Value::Array(details));
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !out.contains_key("reasoning_content")
                {
                    out.insert("reasoning_content".into(), json!(""));
                }
                let has_content = match out.get("content") {
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(parts)) => !parts.is_empty(),
                    _ => false,
                };
                if !has_content && !out.contains_key("tool_calls") {
                    index += 1;
                    continue;
                }
                params.push(Value::Object(out));
                last_role = Some("assistant");
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = transformed.get(index) {
                    let text = yapi_types::message::blocks_text(&result.content, "\n");
                    let has_images = result
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Image(_)));
                    let content = if !text.is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".to_owned()
                    } else {
                        "(no tool output)".to_owned()
                    };
                    let mut tool = json!({"role": "tool", "content": content, "tool_call_id": result.tool_call_id});
                    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
                        tool["name"] = json!(result.tool_name);
                    }
                    params.push(tool);
                    if has_images && model.accepts_images() {
                        for block in &result.content {
                            if let ContentBlock::Image(image) = block {
                                images.push(image_url(&image.mime_type, &image.data));
                            }
                        }
                    }
                    index += 1;
                }
                if images.is_empty() {
                    last_role = Some("toolResult");
                } else {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({"role": "assistant", "content": "I have processed the tool results."}));
                    }
                    let mut content = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    content.extend(images);
                    params.push(json!({"role": "user", "content": content}));
                    last_role = Some("user");
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    Ok(params)
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_))),
        _ => false,
    })
}

fn add_cache_control_to_text(message: &mut Value, cache_control: &Value) -> bool {
    match &mut message["content"] {
        Value::String(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            message["content"] =
                json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            true
        }
        Value::Array(parts) => {
            if let Some(part) = parts.iter_mut().rev().find(|part| part["type"] == "text") {
                part["cache_control"] = cache_control.clone();
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn mapped_effort(model: &Model, level: ThinkingLevel) -> Option<Value> {
    match model.thinking_level_value(level) {
        Some(Some(value)) => Some(json!(value)),
        Some(None) => Some(Value::Null),
        None => Some(json!(level.as_str())),
    }
}

fn chat_template_values(
    model: &Model,
    effort: Option<ThinkingLevel>,
    values: &Map<String, Value>,
    budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let mut resolved = Map::new();
    for (key, value) in values {
        let value = match value.as_object() {
            None => Some(value.clone()),
            Some(object) => {
                if effort.is_none() && object.get("omitWhenOff") == Some(&json!(true)) {
                    None
                } else {
                    match object.get("$var").and_then(Value::as_str) {
                        Some("thinking.enabled") => Some(json!(effort.is_some())),
                        Some("thinking.budget") => budget.map(|budget| json!(budget)),
                        _ => {
                            let mapped = match effort {
                                Some(level) => model.thinking_level_value(level),
                                None => model.thinking_level_value(ThinkingLevel::Off),
                            };
                            match mapped {
                                None => effort.map(|level| json!(level.as_str())),
                                Some(Some(value)) => Some(json!(value)),
                                Some(None) => None,
                            }
                        }
                    }
                }
            }
        };
        if let Some(value) = value {
            resolved.insert(key.clone(), value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

fn build_params(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
    retention: CacheRetention,
    max_tokens: u64,
    effort: Option<ThinkingLevel>,
) -> Result<Value, String> {
    let (request_tools, _) = resolve_transcript_tools(
        messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let mut converted = convert_messages(model, messages, compat)?;
    let cache_control = (compat.cache_control_format.as_deref() == Some("anthropic")
        && retention != CacheRetention::None)
        .then(|| {
            if retention == CacheRetention::Long && compat.supports_long_cache_retention {
                json!({"type": "ephemeral", "ttl": "1h"})
            } else {
                json!({"type": "ephemeral"})
            }
        });

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Null);
    params.insert("stream".into(), json!(true));
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    if ((model.base_url.contains("api.openai.com") && retention != CacheRetention::None) || long)
        && let Some(session) = &options.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            json!(session.chars().take(64).collect::<String>()),
        );
    }
    if long {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }
    if max_tokens > 0 {
        params.insert(compat.max_tokens_field.clone(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    let mut tools = None;
    if !request_tools.is_empty() {
        tools = Some(convert_tools(&request_tools, compat)?);
        params.insert("tools".into(), Value::Null);
        if compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    } else if has_tool_history(messages) {
        tools = Some(Vec::new());
        params.insert("tools".into(), Value::Null);
    }
    if let Some(cache_control) = &cache_control {
        if let Some(first) = converted
            .iter_mut()
            .find(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
        {
            add_cache_control_to_text(first, cache_control);
        }
        if let Some(last) = tools.as_mut().and_then(|tools| tools.last_mut()) {
            last["cache_control"] = cache_control.clone();
        }
        for message in converted.iter_mut().rev() {
            if matches!(
                message["role"].as_str(),
                Some("user" | "assistant" | "tool")
            ) && add_cache_control_to_text(message, cache_control)
            {
                break;
            }
        }
    }
    params.insert("messages".into(), Value::Array(converted));
    if let Some(tools) = tools {
        params.insert("tools".into(), Value::Array(tools));
    }
    if let Some(priority) = compat.vllm_priority {
        params.insert("priority".into(), json!(priority));
    }

    let budget_field = compat.thinking_token_budget_field.clone();
    let budget = effort.filter(|_| model.reasoning).and_then(|level| {
        let ceiling = params
            .get("max_tokens")
            .or_else(|| params.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(model.max_tokens);
        let budget = budget_for_level(level, &options.thinking_budgets)
            .min(ceiling.saturating_sub(MIN_ANSWER_TOKENS));
        (budget > 0).then_some(budget)
    });
    let format = compat.thinking_format.as_str();
    if model.reasoning {
        match format {
            "zai" => {
                params.insert(
                    "thinking".into(),
                    if effort.is_some() {
                        json!({"type": "enabled", "clear_thinking": false})
                    } else {
                        json!({"type": "disabled"})
                    },
                );
                if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort)
                    && let Some(Value::String(value)) = mapped_effort(model, level)
                {
                    params.insert("reasoning_effort".into(), json!(value));
                }
            }
            "qwen" => {
                params.insert("enable_thinking".into(), json!(effort.is_some()));
                if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                    let value = match model.thinking_level_value(level) {
                        Some(Some(value)) => Some(value.to_owned()),
                        _ => Some(level.as_str().to_owned()),
                    };
                    if let Some(value) = value {
                        params.insert("reasoning_effort".into(), json!(value));
                    }
                }
            }
            "qwen-chat-template" => {
                params.insert(
                    "chat_template_kwargs".into(),
                    json!({"enable_thinking": effort.is_some(), "preserve_thinking": true}),
                );
            }
            "chat-template" => {
                if let Some(values) =
                    chat_template_values(model, effort, &compat.chat_template_kwargs, budget)
                {
                    params.insert("chat_template_kwargs".into(), Value::Object(values));
                }
            }
            "baseten" => {
                if let Some(values) =
                    chat_template_values(model, effort, &compat.chat_template_args, budget)
                {
                    params.insert("chat_template_args".into(), Value::Object(values));
                }
                if compat.supports_reasoning_effort {
                    let mapped = match effort {
                        Some(level) => model.thinking_level_value(level),
                        None => model.thinking_level_value(ThinkingLevel::Off),
                    };
                    let value = match mapped {
                        None => effort.map(|level| level.as_str().to_owned()),
                        Some(Some(value)) => Some(value.to_owned()),
                        Some(None) => None,
                    };
                    if let Some(value) = value {
                        params.insert("reasoning_effort".into(), json!(value));
                    }
                }
            }
            "deepseek" => {
                if effort.is_some() {
                    params.insert("thinking".into(), json!({"type": "enabled"}));
                } else if model.thinking_level_value(ThinkingLevel::Off) != Some(None) {
                    params.insert("thinking".into(), json!({"type": "disabled"}));
                }
                if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                    let value = match model.thinking_level_value(level) {
                        Some(Some(value)) => json!(value),
                        _ => json!(level.as_str()),
                    };
                    params.insert("reasoning_effort".into(), value);
                }
            }
            "openrouter" => match effort {
                Some(level) => {
                    let value = match model.thinking_level_value(level) {
                        Some(Some(value)) => json!(value),
                        _ => json!(level.as_str()),
                    };
                    params.insert("reasoning".into(), json!({"effort": value}));
                }
                None => match model.thinking_level_value(ThinkingLevel::Off) {
                    Some(None) => {}
                    Some(Some(value)) => {
                        params.insert("reasoning".into(), json!({"effort": value}));
                    }
                    None => {
                        params.insert("reasoning".into(), json!({"effort": "none"}));
                    }
                },
            },
            "ant-ling" => {
                if let Some(level) = effort
                    && let Some(Some(value)) = model.thinking_level_value(level)
                {
                    params.insert("reasoning".into(), json!({"effort": value}));
                }
            }
            "together" => {
                params.insert("reasoning".into(), json!({"enabled": effort.is_some()}));
                if let Some(level) = effort.filter(|_| compat.supports_reasoning_effort) {
                    let value = match model.thinking_level_value(level) {
                        Some(Some(value)) => json!(value),
                        _ => json!(level.as_str()),
                    };
                    params.insert("reasoning_effort".into(), value);
                }
            }
            "string-thinking" => match effort {
                Some(level) => {
                    let value = match model.thinking_level_value(level) {
                        Some(Some(value)) => json!(value),
                        _ => json!(level.as_str()),
                    };
                    params.insert("thinking".into(), value);
                }
                None => match model.thinking_level_value(ThinkingLevel::Off) {
                    Some(None) => {}
                    Some(Some(value)) => {
                        params.insert("thinking".into(), json!(value));
                    }
                    None => {
                        params.insert("thinking".into(), json!("none"));
                    }
                },
            },
            _ => {
                if compat.supports_reasoning_effort {
                    match effort {
                        Some(level) => {
                            let value = match model.thinking_level_value(level) {
                                Some(Some(value)) => json!(value),
                                _ => json!(level.as_str()),
                            };
                            params.insert("reasoning_effort".into(), value);
                        }
                        None => {
                            if let Some(Some(off)) = model.thinking_level_value(ThinkingLevel::Off)
                            {
                                params.insert("reasoning_effort".into(), json!(off));
                            }
                        }
                    }
                }
            }
        }
    }
    if let (Some(field), Some(budget)) = (budget_field, budget) {
        params.insert(field, json!(budget));
    }
    if let Some(routing) = &compat.open_router_routing {
        params.insert("provider".into(), Value::Object(routing.clone()));
    }
    if let Some(routing) = &compat.vercel_gateway_routing {
        let mut gateway = Map::new();
        for key in ["only", "order"] {
            if let Some(value) = routing.get(key) {
                gateway.insert(key.into(), value.clone());
            }
        }
        if !gateway.is_empty() {
            params.insert("providerOptions".into(), json!({"gateway": gateway}));
        }
    }
    for (key, value) in model.sampling_params.iter().flatten() {
        params.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(params))
}

fn parse_usage(raw: &Value, model: &Model) -> Usage {
    let prompt = raw["prompt_tokens"].as_u64().unwrap_or(0);
    let cache_read = raw["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| raw["prompt_cache_hit_tokens"].as_u64())
        .or_else(|| raw["cached_tokens"].as_u64())
        .unwrap_or(0);
    let cache_write = raw["prompt_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .unwrap_or(0);
    let input = prompt.saturating_sub(cache_read + cache_write);
    let output = raw["completion_tokens"].as_u64().unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(
            raw["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0),
        ),
        total_tokens: Some(input + output + cache_read + cache_write),
        ..Usage::default()
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

pub(super) async fn run(request: Request, sender: EventSender) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let compat = Compat::new(&model);
    let output = new_output(&model, now_ms());
    let response = match connect(&model, &compat, &messages, &options).await {
        Ok(response) => response,
        Err(message) => return send_error(&sender, output, &options.cancel, message),
    };

    sender.send(StreamEvent::Start(output.clone()));
    let mut state = State {
        output,
        text: None,
        thinking: None,
        calls: Vec::new(),
        details: None,
        finished: false,
    };
    let result = state
        .consume(SseReader::new(response), &model, &compat, &sender, &options)
        .await;
    match result {
        Ok(()) => sender.send(StreamEvent::Done(state.output)),
        Err(message) => {
            state.apply_details();
            send_error(&sender, state.output, &options.cancel, message);
        }
    }
}

/// Sends the request; the response once its status is a success.
async fn connect(
    model: &Model,
    compat: &Compat,
    messages: &[Message],
    options: &StreamOptions,
) -> Result<reqwest::Response, String> {
    let normalized = resolve_transcript(messages, compat.supports_mid_convo_system_messages);
    let api_key = match options.api_key.clone().filter(|key| !key.is_empty()) {
        Some(key) => key,
        None if options.has_header("authorization")
            || options.has_header("cf-aig-authorization") =>
        {
            "unused".to_owned()
        }
        None => return Err(format!("No API key for provider: {}", model.provider)),
    };
    // streamSimple: clamp output to the context and the level to the model.
    let max_tokens = clamp_max_tokens_to_context(
        model,
        messages,
        options.max_tokens.unwrap_or(model.max_tokens),
    );
    let effort = options
        .reasoning
        .map(|level| clamp_level(model, level))
        .filter(|level| *level != ThinkingLevel::Off);
    let retention = options.resolved_cache_retention();
    let params = build_params(
        model,
        &normalized,
        options,
        compat,
        retention,
        max_tokens,
        effort,
    )?;

    let mut headers = Headers::default();
    headers.set("authorization", Some(format!("Bearer {api_key}")));
    headers.set("content-type", Some("application/json"));
    headers.set("accept", Some("application/json"));
    headers.extend_model(model.headers.as_ref());
    if model.provider == "github-copilot" {
        for (key, value) in super::copilot_headers(messages) {
            headers.set(key, Some(value));
        }
    }
    if let Some(session) = options
        .session_id
        .as_deref()
        .filter(|_| retention != CacheRetention::None && compat.send_session_affinity_headers)
    {
        if compat.session_affinity_format == "openrouter" {
            headers.set("x-session-id", Some(session));
        } else {
            if compat.session_affinity_format == "openai" {
                headers.set("session_id", Some(session));
            }
            headers.set("x-client-request-id", Some(session));
            headers.set("x-session-affinity", Some(session));
        }
    }
    headers.extend(&options.headers);

    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let body = yapi_types::json::to_string(&params).map_err(|err| err.to_string())?;
    let build = || headers.apply(http::client().post(&url).body(body.clone()));
    http::send(build, options).await.map_err(failure_message)
}

fn failure_message(failure: Failure) -> String {
    let Failure::Status { status, body } = failure else {
        return failure.plain_message().unwrap_or_default();
    };
    let mut message = http::openai_status_message(status, &body, None);
    let json = serde_json::from_str::<Value>(&body).ok();
    if let Some(raw) = json
        .as_ref()
        .and_then(|json| json.pointer("/error/metadata/raw"))
    {
        let raw = raw
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| raw.to_string());
        if !message.contains(&raw) {
            message += &format!("\n{raw}");
        }
    }
    message
}

struct CallState {
    position: usize,
    stream_index: Option<u64>,
    id: String,
    partial_args: String,
}

struct State {
    output: AssistantMessage,
    text: Option<usize>,
    thinking: Option<usize>,
    calls: Vec<CallState>,
    details: Option<Vec<Value>>,
    finished: bool,
}

impl State {
    fn apply_details(&mut self) {
        if let (Some(position), Some(details)) = (self.thinking, &self.details)
            && let ContentBlock::Thinking(thinking) = &mut self.output.content[position]
        {
            thinking.thinking_signature = yapi_types::json::to_string(details).ok();
        }
    }

    fn ensure_text(&mut self, sender: &EventSender) -> usize {
        if let Some(position) = self.text {
            return position;
        }
        let position = sender.start(&mut self.output, ContentBlock::text(""));
        self.text = Some(position);
        position
    }

    fn ensure_thinking(&mut self, signature: &str, sender: &EventSender) -> usize {
        if let Some(position) = self.thinking {
            return position;
        }
        let block = ContentBlock::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some(signature.to_owned()),
            redacted: None,
        });
        let position = sender.start(&mut self.output, block);
        self.thinking = Some(position);
        position
    }

    fn ensure_call(&mut self, delta: &Value, sender: &EventSender) -> usize {
        let stream_index = delta["index"].as_u64();
        let id = delta["id"].as_str().unwrap_or_default();
        let name = delta["function"]["name"].as_str().unwrap_or_default();
        let existing = stream_index
            .and_then(|index| {
                self.calls
                    .iter()
                    .position(|call| call.stream_index == Some(index))
            })
            .or_else(|| {
                (!id.is_empty())
                    .then(|| self.calls.iter().position(|call| call.id == id))
                    .flatten()
            });
        let index = match existing {
            Some(index) => index,
            None => {
                let block = ContentBlock::ToolCall(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: Map::new(),
                    thought_signature: None,
                    namespace: None,
                });
                let position = sender.start(&mut self.output, block);
                self.calls.push(CallState {
                    position,
                    stream_index,
                    id: id.to_owned(),
                    partial_args: String::new(),
                });
                self.calls.len() - 1
            }
        };
        let call = &mut self.calls[index];
        if call.stream_index.is_none() {
            call.stream_index = stream_index;
        }
        if !id.is_empty() {
            call.id = id.to_owned();
        }
        if let ContentBlock::ToolCall(block) = &mut self.output.content[call.position] {
            if block.id.is_empty() && !id.is_empty() {
                block.id = id.to_owned();
            }
            if block.name.is_empty() && !name.is_empty() {
                block.name = name.to_owned();
            }
        }
        index
    }

    fn finish_blocks(&mut self, sender: &EventSender) {
        self.apply_details();
        for position in 0..self.output.content.len() {
            if let ContentBlock::ToolCall(call) = &mut self.output.content[position]
                && let Some(state) = self.calls.iter().find(|state| state.position == position)
            {
                call.arguments = parse_streaming_json(&state.partial_args);
            }
            sender.end(&self.output, position);
        }
    }

    async fn consume(
        &mut self,
        mut reader: SseReader,
        model: &Model,
        compat: &Compat,
        sender: &EventSender,
        options: &StreamOptions,
    ) -> Result<(), String> {
        while let Some(chunk) = http::next_openai_chunk(&mut reader, &options.cancel).await? {
            self.handle(&chunk, model, sender);
        }
        self.finish_blocks(sender);
        if options.cancel.is_cancelled() || self.output.stop_reason == StopReason::Aborted {
            return Err(http::ABORTED_DURING_STREAM.into());
        }
        if !self.finished && !compat.supports_finish_reason {
            let has_calls = self
                .output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)));
            self.output.stop_reason = if has_calls {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
        }
        if self.output.stop_reason == StopReason::Error {
            return Err(self
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".into()));
        }
        if (compat.supports_finish_reason && !self.finished)
            || self.output.stop_reason == StopReason::Pending
        {
            return Err("Stream ended without finish_reason".into());
        }
        Ok(())
    }

    fn handle(&mut self, chunk: &Value, model: &Model, sender: &EventSender) {
        if self.output.response_id.is_none()
            && let Some(id) = chunk["id"].as_str().filter(|id| !id.is_empty())
        {
            self.output.response_id = Some(id.to_owned());
        }
        if let Some(name) = chunk["model"]
            .as_str()
            .filter(|name| !name.is_empty() && *name != model.id)
            && self.output.response_model.is_none()
        {
            self.output.response_model = Some(name.to_owned());
        }
        if chunk["usage"].is_object() {
            self.output.usage = parse_usage(&chunk["usage"], model);
        }
        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return;
        };
        if !chunk["usage"].is_object() && choice["usage"].is_object() {
            self.output.usage = parse_usage(&choice["usage"], model);
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.output.raw_stop_reason = Some(reason.to_owned());
            let (stop, message) = map_stop_reason(reason);
            self.output.stop_reason = stop;
            if let Some(message) = message {
                self.output.error_message = Some(message);
            }
            self.finished = true;
        }
        let delta = &choice["delta"];
        if !delta.is_object() {
            return;
        }
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            let position = self.ensure_text(sender);
            sender.delta(&mut self.output, position, text);
        }
        if let Some((field, text)) = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|field| {
                delta[field]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(|text| (field, text))
            })
        {
            let signature = if model.provider == "opencode-go" && field == "reasoning" {
                "reasoning_content"
            } else {
                field
            };
            let position = self.ensure_thinking(signature, sender);
            sender.delta(&mut self.output, position, text);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = self.ensure_call(call, sender);
                let position = self.calls[index].position;
                let arguments = call["function"]["arguments"]
                    .as_str()
                    .filter(|a| !a.is_empty());
                if let Some(arguments) = arguments {
                    self.calls[index].partial_args.push_str(arguments);
                    let parsed = parse_streaming_json(&self.calls[index].partial_args);
                    if let ContentBlock::ToolCall(block) = &mut self.output.content[position] {
                        block.arguments = parsed;
                    }
                }
                sender.delta(&mut self.output, position, arguments.unwrap_or_default());
            }
        }
        if let Some(details) = delta["reasoning_details"].as_array() {
            for detail in details.iter().filter(|detail| is_reasoning_detail(detail)) {
                self.ensure_thinking("", sender);
                append_reasoning_detail(self.details.get_or_insert_with(Vec::new), detail);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tool_call_ids() {
        // Values computed with pi's shortHash in Node.
        assert_eq!(
            normalize_tool_call_id("call_1|item 2", "openai"),
            "call_1_item_2"
        );
    }
}
