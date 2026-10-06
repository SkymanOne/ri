//! `bedrock-converse-stream`: Amazon Bedrock's ConverseStream API.
//!
//! Port of `packages/ai/src/api/bedrock-converse-stream.ts` in pi `v1.0.0`,
//! with the parts of the AWS SDK it relies on: endpoint and region
//! resolution, SigV4 or bearer authentication, the standard retry strategy,
//! error deserialization and the event stream.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, ImageContent, Message, StopReason, ThinkingLevel,
    ToolCall, ToolDeclaration, ToolResultMessage,
};
use yapi_types::model::Model;

use crate::aws::{self, AwsEnv, Credentials, eventstream, sigv4};
use crate::cost::calculate_cost;
use crate::credentials::provider_env_value;
use crate::http::{self, Headers};
use crate::json_parse::parse_streaming_json;
use crate::schema;
use crate::stream::{
    CacheRetention, EventSender, Request, StreamEvent, StreamOptions, check_complete, new_output,
    now_ms, send_error,
};
use crate::thinking::{budgeted, requested_max_tokens};
use crate::transcript::{
    current_tools, initial_system_message, resolve_transcript, transform_messages,
};

const EMPTY_TEXT: &str = "<empty>";
const REDACTED_THINKING: &str = "[Reasoning redacted]";
const DATA_RETENTION_DOCS: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";

/// Lowercase forms of the model id and name, with separators as dashes too.
fn match_candidates(model: &Model) -> Vec<String> {
    let mut values = vec![model.id.to_lowercase()];
    if !model.name.is_empty() {
        values.push(model.name.to_lowercase());
    }
    values
        .into_iter()
        .flat_map(|lower| {
            let mut dashed = String::new();
            let mut previous_separator = false;
            for c in lower.chars() {
                if c.is_whitespace() || matches!(c, '_' | '.' | ':') {
                    if !previous_separator {
                        dashed.push('-');
                    }
                    previous_separator = true;
                } else {
                    dashed.push(c);
                    previous_separator = false;
                }
            }
            [lower, dashed]
        })
        .collect()
}

fn is_claude(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn any_candidate(model: &Model, needles: &[&str]) -> bool {
    match_candidates(model)
        .iter()
        .any(|candidate| needles.iter().any(|needle| candidate.contains(needle)))
}

fn supports_adaptive_thinking(model: &Model) -> bool {
    any_candidate(
        model,
        &[
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "opus-5",
            "sonnet-4-6",
            "sonnet-5",
            "fable-5",
        ],
    )
}

fn supports_native_xhigh(model: &Model) -> bool {
    any_candidate(
        model,
        &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "fable-5"],
    )
}

fn effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && supports_native_xhigh(model) {
        return "xhigh".into();
    }
    if let Some(Some(mapped)) = model.thinking_level_value(level) {
        return mapped.to_owned();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .into()
}

fn supports_prompt_caching(model: &Model, env: &dyn Fn(&str) -> Option<String>) -> bool {
    let candidates = match_candidates(model);
    if !candidates.iter().any(|c| c.contains("claude")) {
        return env("AWS_BEDROCK_FORCE_CACHE").as_deref() == Some("1");
    }
    candidates.iter().any(|c| {
        c.contains("fable-5")
            || c.contains("opus-5")
            || c.contains("sonnet-5")
            || c.contains("-4-")
            || c.contains("claude-3-7-sonnet")
            || c.contains("claude-3-5-haiku")
    })
}

fn cache_point(retention: CacheRetention) -> Value {
    let mut point = Map::new();
    point.insert("type".into(), json!("default"));
    if retention == CacheRetention::Long {
        point.insert("ttl".into(), json!("1h"));
    }
    json!({ "cachePoint": point })
}

fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({ "text": text }))
}

fn required_text_block(text: &str) -> Value {
    text_block(text).unwrap_or_else(|| json!({ "text": EMPTY_TEXT }))
}

/// Base64 re-encoded, as the SDK decodes pi's base64 and encodes the bytes.
fn normalize_base64(data: &str) -> Result<String, String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine
        .decode(data.trim())
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(data.trim().trim_end_matches('='))
        })
        .map_err(|_| "The string to be decoded is not correctly encoded.".to_owned())?;
    Ok(engine.encode(bytes))
}

fn image_block(image: &ImageContent) -> Result<Value, String> {
    let format = match image.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({
        "image": { "format": format, "source": { "bytes": normalize_base64(&image.data)? } }
    }))
}

/// Drops empty keys, which Bedrock documents reject.
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.clone(), sanitize_document(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn tool_result_content(content: &[ContentBlock]) -> Result<Vec<Value>, String> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Image(image) => blocks.push(image_block(image)?),
            ContentBlock::Text(text) => blocks.extend(text_block(&text.text)),
            _ => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "text": EMPTY_TEXT }));
    }
    Ok(blocks)
}

fn tool_result(result: &ToolResultMessage) -> Result<Value, String> {
    Ok(json!({
        "toolResult": {
            "toolUseId": result.tool_call_id,
            "content": tool_result_content(&result.content)?,
            "status": if result.is_error { "error" } else { "success" },
        }
    }))
}

fn assistant_blocks(assistant: &AssistantMessage, model: &Model) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Text(text) => blocks.extend(text_block(&text.text)),
            ContentBlock::ToolCall(call) => blocks.push(json!({
                "toolUse": {
                    "toolUseId": call.id,
                    "name": call.name,
                    "input": sanitize_document(&Value::Object(call.arguments.clone())),
                }
            })),
            ContentBlock::Thinking(thinking) => {
                if thinking.redacted == Some(true) {
                    // Encrypted reasoning replays as the opaque payload; one
                    // that is not base64 is dropped.
                    if let Some(payload) = thinking
                        .thinking_signature
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .and_then(|s| normalize_base64(s).ok())
                        .filter(|s| !s.is_empty())
                    {
                        blocks.push(json!({ "reasoningContent": { "redactedContent": payload } }));
                    }
                    continue;
                }
                if thinking.thinking.trim().is_empty() {
                    continue;
                }
                if is_claude(model) {
                    match thinking
                        .thinking_signature
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                    {
                        Some(signature) => blocks.push(json!({
                            "reasoningContent": {
                                "reasoningText": { "text": thinking.thinking, "signature": signature }
                            }
                        })),
                        None => blocks.push(json!({ "text": thinking.thinking })),
                    }
                } else {
                    blocks.push(json!({
                        "reasoningContent": { "reasoningText": { "text": thinking.thinking } }
                    }));
                }
            }
            ContentBlock::Image(_) => {}
        }
    }
    blocks
}

fn convert_messages(
    messages: &[Message],
    model: &Model,
    retention: CacheRetention,
    caching: bool,
) -> Result<Vec<Value>, String> {
    let without_system: Vec<Message> = match messages.first() {
        Some(Message::System(_)) => messages[1..].to_vec(),
        _ => messages.to_vec(),
    };
    let normalize = |id: &str, _: &AssistantMessage| super::normalize_tool_call_id(id);
    let transformed = transform_messages(&without_system, model, Some(&normalize), now_ms());
    let mut result = Vec::new();
    let mut index = 0;
    while index < transformed.len() {
        match &transformed[index] {
            Message::User(user) => {
                let content = match &user.content {
                    Content::Text(text) => vec![required_text_block(text)],
                    Content::Blocks(blocks) => {
                        let mut content = Vec::new();
                        for block in blocks {
                            match block {
                                ContentBlock::Text(text) => content.extend(text_block(&text.text)),
                                ContentBlock::Image(image) => content.push(image_block(image)?),
                                _ => {}
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({ "text": EMPTY_TEXT }));
                        }
                        content
                    }
                };
                result.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let blocks = assistant_blocks(assistant, model);
                if !blocks.is_empty() {
                    result.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult(first) => {
                // Bedrock takes every consecutive tool result in one message.
                let mut results = vec![tool_result(first)?];
                while let Some(Message::ToolResult(next)) = transformed.get(index + 1) {
                    results.push(tool_result(next)?);
                    index += 1;
                }
                result.push(json!({ "role": "user", "content": results }));
            }
            _ => {}
        }
        index += 1;
    }
    if retention != CacheRetention::None
        && caching
        && let Some(last) = result.last_mut()
        && last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(cache_point(retention));
    }
    Ok(result)
}

fn tool_config(tools: &[ToolDeclaration], supports_strict: bool) -> Result<Option<Value>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut specs = Vec::new();
    for tool in tools {
        let strict = schema::strict_sampling(tool, supports_strict, None)?;
        let mut spec = Map::new();
        spec.insert("name".into(), json!(tool.name));
        spec.insert(
            "inputSchema".into(),
            json!({ "json": schema::tool_parameters(tool, strict) }),
        );
        spec.insert("description".into(), json!(tool.description));
        if strict {
            spec.insert("strict".into(), json!(true));
        }
        specs.push(json!({ "toolSpec": spec }));
    }
    Ok(Some(json!({ "tools": specs })))
}

/// The thinking setup of a request: the level and the budgets after pi's
/// `streamSimple` adjustments, and the output limit.
struct Thinking {
    level: Option<ThinkingLevel>,
    budget: Option<u64>,
    max_tokens: u64,
}

fn thinking(model: &Model, messages: &[Message], options: &StreamOptions) -> Thinking {
    let base = requested_max_tokens(model, messages, options);
    let level = options
        .reasoning
        .filter(|level| *level != ThinkingLevel::Off);
    let Some(level) = level else {
        return Thinking {
            level: None,
            budget: None,
            max_tokens: base,
        };
    };
    if is_claude(model) && !supports_adaptive_thinking(model) {
        let (max_tokens, budget) =
            budgeted(model, messages, base, level, &options.thinking_budgets);
        return Thinking {
            level: Some(level),
            budget: Some(budget),
            max_tokens,
        };
    }
    Thinking {
        level: Some(level),
        budget: None,
        max_tokens: base,
    }
}

fn is_gov_cloud(model: &Model, region: Option<&str>) -> bool {
    if region.is_some_and(|region| region.to_lowercase().starts_with("us-gov-")) {
        return true;
    }
    let id = model.id.to_lowercase();
    id.starts_with("us-gov.") || id.starts_with("arn:aws-us-gov:")
}

fn additional_fields(
    model: &Model,
    thinking: &Thinking,
    options: &StreamOptions,
    configured_region: Option<&str>,
) -> Option<Value> {
    let level = thinking.level?;
    if !model.reasoning || !is_claude(model) {
        return None;
    }
    let display = (!is_gov_cloud(model, configured_region)).then_some("summarized");
    let mut config = Map::new();
    config.insert(
        "type".into(),
        json!(if supports_adaptive_thinking(model) {
            "adaptive"
        } else {
            "enabled"
        }),
    );
    let mut fields = Map::new();
    if supports_adaptive_thinking(model) {
        if let Some(display) = display {
            config.insert("display".into(), json!(display));
        }
        fields.insert("thinking".into(), Value::Object(config));
        fields.insert(
            "output_config".into(),
            json!({ "effort": effort(model, level) }),
        );
    } else {
        let budget = thinking
            .budget
            .unwrap_or_else(|| crate::thinking::budget_for_level(level, &options.thinking_budgets));
        config.insert("budget_tokens".into(), json!(budget));
        if let Some(display) = display {
            config.insert("display".into(), json!(display));
        }
        fields.insert("thinking".into(), Value::Object(config));
        fields.insert(
            "anthropic_beta".into(),
            json!(["interleaved-thinking-2025-05-14"]),
        );
    }
    Some(Value::Object(fields))
}

/// The request body, in the SDK's member order.
fn build_body(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    thinking: &Thinking,
    configured_region: Option<&str>,
) -> Result<Value, String> {
    let env = |name: &str| provider_env_value(name, options.env.as_ref());
    let normalized = resolve_transcript(messages, false);
    let retention = options.resolved_cache_retention();
    let caching = supports_prompt_caching(model, &env);
    let mut body = Map::new();
    body.insert(
        "messages".into(),
        Value::Array(convert_messages(&normalized, model, retention, caching)?),
    );
    let system = initial_system_message(&normalized)
        .map(|system| system.text())
        .unwrap_or_default();
    if !system.is_empty() {
        let mut blocks = vec![json!({ "text": system })];
        if retention != CacheRetention::None && caching {
            blocks.push(cache_point(retention));
        }
        body.insert("system".into(), Value::Array(blocks));
    }
    let mut inference = Map::new();
    inference.insert("maxTokens".into(), json!(thinking.max_tokens));
    if let Some(temperature) = options.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    body.insert("inferenceConfig".into(), Value::Object(inference));
    let supports_strict = model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("supportsStrictMode"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(config) = tool_config(&current_tools(&normalized), supports_strict)? {
        body.insert("toolConfig".into(), config);
    }
    if let Some(fields) = additional_fields(model, thinking, options, configured_region) {
        body.insert("additionalModelRequestFields".into(), fields);
    }
    Ok(Value::Object(body))
}

/// The region of a standard Bedrock runtime endpoint.
fn endpoint_region(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.to_lowercase();
    let rest = host
        .strip_prefix("bedrock-runtime-fips.")
        .or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = rest
        .strip_suffix(".amazonaws.com.cn")
        .or_else(|| rest.strip_suffix(".amazonaws.com"))?;
    (!region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| region.to_owned())
}

/// The region embedded in an inference profile ARN.
fn arn_region(id: &str) -> Option<String> {
    let rest = id.strip_prefix("arn:aws")?;
    let rest = match rest.strip_prefix('-') {
        Some(partition) => partition.split_once(':')?.1,
        None => rest.strip_prefix(':')?,
    };
    let region = rest.strip_prefix("bedrock:")?.split(':').next()?;
    (!region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| region.to_owned())
}

/// How a request authenticates.
enum Signer {
    Bearer(String),
    SigV4(Credentials),
}

/// Where a request goes: URL, signing region and authentication.
struct Target {
    url: String,
    region: String,
    signer: Signer,
}

async fn target(model: &Model, options: &StreamOptions) -> Result<Target, String> {
    let env = |name: &str| provider_env_value(name, options.env.as_ref());
    let process_profile = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|profile| !profile.is_empty());
    let options_profile = options
        .env
        .as_ref()
        .and_then(|env| env.get("AWS_PROFILE"))
        .filter(|profile| !profile.is_empty())
        .cloned();
    let configured_region = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION"));
    let endpoint_region = endpoint_region(&model.base_url);
    let explicit =
        endpoint_region.is_none() || (configured_region.is_none() && process_profile.is_none());
    let profile = options_profile.clone().or_else(|| env("AWS_PROFILE"));
    let aws_env = AwsEnv::from_process(options.env.as_ref());
    let region = if let Some(region) = arn_region(&model.id) {
        region
    } else if let Some(region) = configured_region {
        region
    } else if let (Some(region), true) = (endpoint_region, explicit) {
        region
    } else if process_profile.is_none() {
        "us-east-1".into()
    } else {
        aws::profile_region(&aws_env, profile.as_deref())
            .await
            .ok_or_else(|| "Region is missing".to_owned())?
    };
    let skip_auth = env("AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1");
    let bearer = options
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| env("AWS_BEARER_TOKEN_BEDROCK"));
    let signer = match bearer {
        Some(token) if !skip_auth => Signer::Bearer(token),
        _ if skip_auth => Signer::SigV4(Credentials {
            access_key_id: "dummy-access-key".into(),
            secret_access_key: "dummy-secret-key".into(),
            session_token: None,
            expiration_ms: None,
        }),
        _ => {
            let static_keys = env("AWS_ACCESS_KEY_ID").zip(env("AWS_SECRET_ACCESS_KEY"));
            match static_keys {
                Some((id, secret)) if options_profile.is_none() => Signer::SigV4(Credentials {
                    access_key_id: id,
                    secret_access_key: secret,
                    session_token: env("AWS_SESSION_TOKEN"),
                    expiration_ms: None,
                }),
                _ => Signer::SigV4(
                    aws::default_credentials(
                        &aws_env,
                        profile.as_deref(),
                        Some(&region),
                        &options.cancel,
                    )
                    .await?,
                ),
            }
        }
    };
    let endpoint = if explicit {
        model.base_url.clone()
    } else if let Some(url) =
        env("AWS_ENDPOINT_URL_BEDROCK_RUNTIME").or_else(|| env("AWS_ENDPOINT_URL"))
    {
        url
    } else {
        let fips = env("AWS_USE_FIPS_ENDPOINT").as_deref() == Some("true");
        let host = if fips {
            "bedrock-runtime-fips"
        } else {
            "bedrock-runtime"
        };
        let suffix = if region.starts_with("cn-") { ".cn" } else { "" };
        format!("https://{host}.{region}.amazonaws.com{suffix}")
    };
    Ok(Target {
        url: format!(
            "{}/model/{}/converse-stream",
            endpoint.trim_end_matches('/'),
            sigv4::escape(&model.id)
        ),
        region,
        signer,
    })
}

/// Headers callers may not set: SigV4 and auth headers.
fn reserved(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("x-amz-") || lower == "authorization" || lower == "host"
}

fn signed_headers(target: &Target, body: &str, options: &StreamOptions) -> Result<Headers, String> {
    let url = url::Url::parse(&target.url).map_err(|err| err.to_string())?;
    let mut headers = Headers::default();
    headers.set("content-type", Some("application/json"));
    for (name, value) in &options.headers {
        if let Some(value) = value
            && !reserved(name)
        {
            headers.set(name, Some(value.as_str()));
        }
    }
    match &target.signer {
        Signer::Bearer(token) => headers.set("authorization", Some(format!("Bearer {token}"))),
        Signer::SigV4(credentials) => sigv4::sign_request(
            &url,
            &mut headers.0,
            body.as_bytes(),
            credentials,
            &target.region,
            "bedrock",
        ),
    }
    Ok(headers)
}

/// A failed request or stream, as the SDK reports it.
struct Failure {
    message: String,
    status: Option<u16>,
    code: Option<String>,
    request_id: Option<String>,
    retryable: bool,
}

impl Failure {
    /// A failure with only a message, never retried.
    fn plain(message: String) -> Failure {
        Failure {
            message,
            status: None,
            code: None,
            request_id: None,
            retryable: false,
        }
    }
}

const ERROR_PREFIXES: &[(&str, &str)] = &[
    ("InternalServerException", "Internal server error"),
    ("ModelStreamErrorException", "Model stream error"),
    ("ValidationException", "Validation error"),
    ("ThrottlingException", "Throttling error"),
    ("ServiceUnavailableException", "Service unavailable"),
];

const THROTTLING_CODES: &[&str] = &[
    "BandwidthLimitExceeded",
    "EC2ThrottledException",
    "LimitExceededException",
    "PriorRequestNotComplete",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "RequestThrottledException",
    "SlowDown",
    "ThrottledException",
    "Throttling",
    "ThrottlingException",
    "TooManyRequestsException",
    "TransactionInProgressException",
];

/// pi's `formatBedrockError` for a modeled service error.
fn service_message(name: &str, message: &str) -> String {
    let prefix = ERROR_PREFIXES
        .iter()
        .find(|(code, _)| *code == name)
        .map_or(name, |(_, prefix)| prefix);
    format!("{prefix}: {message}{}", retention_hint(message))
}

fn retention_hint(message: &str) -> String {
    if message.to_lowercase().contains("data retention mode") {
        format!(" See {DATA_RETENTION_DOCS} for supported data retention modes.")
    } else {
        String::new()
    }
}

/// V8's `JSON.parse` message for a body that is not JSON.
fn json_parse_error(text: &str) -> String {
    let err = match serde_json::from_str::<Value>(text) {
        Ok(_) => return String::new(),
        Err(err) => err,
    };
    if text.trim().is_empty() || err.is_eof() {
        return "Unexpected end of JSON input".into();
    }
    // serde reports the 1-based column after the offending character.
    let line_start: usize = text
        .split_inclusive('\n')
        .take(err.line().saturating_sub(1))
        .map(|line| line.chars().count())
        .sum();
    let chars: Vec<char> = text.chars().collect();
    let position = (line_start + err.column().saturating_sub(1)).min(chars.len().saturating_sub(1));
    let token = chars[position];
    let slice =
        |from: usize, to: usize| chars[from..to.min(chars.len())].iter().collect::<String>();
    const CONTEXT: usize = 10;
    let source = if chars.len() < CONTEXT * 2 + 1 {
        format!("\"{text}\"")
    } else if position < CONTEXT {
        format!("\"{}\"...", slice(0, position + CONTEXT))
    } else if position + CONTEXT >= chars.len() {
        format!("...\"{}\"", slice(position - CONTEXT, chars.len()))
    } else {
        format!(
            "...\"{}\"...",
            slice(position - CONTEXT, position + CONTEXT)
        )
    };
    format!("Unexpected token '{token}', {source} is not valid JSON")
}

/// The SDK's error for a non-2xx response.
fn response_failure(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> Failure {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let request_id = header("x-amzn-requestid").and_then(|id| diagnostic_value(&id));
    let parsed = if body.is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str::<Value>(body)
    };
    let Ok(json) = parsed else {
        return Failure {
            message: format!(
                "{}\n  Deserialization error: to see the raw response, inspect the hidden field {{error}}.$response on this object.",
                json_parse_error(body)
            ),
            status: Some(status),
            code: None,
            request_id,
            retryable: matches!(status, 429 | 500 | 502 | 503 | 504),
        };
    };
    let sanitize = |code: &str| {
        let code = code.split(':').next().unwrap_or(code);
        code.rsplit('#').next().unwrap_or(code).to_owned()
    };
    let name = header("x-amzn-errortype")
        .map(|code| sanitize(&code))
        .or_else(|| json["code"].as_str().map(sanitize))
        .or_else(|| json["__type"].as_str().map(sanitize))
        .unwrap_or_else(|| "Unknown".to_owned());
    let message = json["message"]
        .as_str()
        .or_else(|| json["Message"].as_str())
        .unwrap_or("UnknownError");
    Failure {
        message: service_message(&name, message),
        status: Some(status),
        code: name.ends_with("Exception").then(|| name.clone()),
        request_id,
        retryable: matches!(status, 429 | 500 | 502 | 503 | 504)
            || THROTTLING_CODES.contains(&name.as_str()),
    }
}

/// pi's `normalizeDiagnosticValue`: trimmed, dropped when blank or too long.
fn diagnostic_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty() && trimmed.chars().count() <= 200).then(|| trimmed.to_owned())
}

fn append_diagnostic(
    output: &mut AssistantMessage,
    failure: &Failure,
    fallback_request_id: Option<&str>,
) {
    let mut details = Map::new();
    if let Some(status) = failure.status {
        details.insert("status".into(), json!(status));
    }
    if let Some(code) = &failure.code {
        details.insert("errorCode".into(), json!(code));
    }
    if let Some(id) = failure
        .request_id
        .clone()
        .or_else(|| fallback_request_id.map(str::to_owned))
    {
        details.insert("requestId".into(), json!(id));
    }
    if details.is_empty() {
        return;
    }
    output.diagnostics.get_or_insert_with(Vec::new).push(json!({
        "type": "bedrock_response_failure",
        "timestamp": now_ms(),
        "details": details,
    }));
}

/// `AWS_MAX_ATTEMPTS`, else the SDK's three attempts.
fn max_attempts(options: &StreamOptions) -> u32 {
    provider_env_value("AWS_MAX_ATTEMPTS", options.env.as_ref())
        .and_then(|value| value.parse().ok())
        .filter(|attempts: &u32| *attempts > 0)
        .unwrap_or(3)
}

/// The standard strategy's delay before retry `attempt` (1-based): full
/// jitter over 100 ms (500 ms when throttled) doubling, at most 20 s.
fn retry_delay(attempt: u32, throttled: bool) -> Duration {
    let base: u64 = if throttled { 500 } else { 100 };
    let ceiling = (base << attempt.min(16)).min(20_000);
    let random = u64::from_le_bytes(yapi_types::time::random_bytes());
    let fraction = (random % 10_000) as f64 / 10_000.0;
    Duration::from_millis((ceiling as f64 * fraction) as u64)
}

/// Sends the request with the SDK's retries, re-signing each attempt.
async fn send(
    target: &Target,
    body: &str,
    options: &StreamOptions,
) -> Result<reqwest::Response, Failure> {
    let attempts = max_attempts(options);
    let mut attempt = 1;
    loop {
        let headers = signed_headers(target, body, options).map_err(Failure::plain)?;
        let request = headers.apply(http::client().post(&target.url).body(body.to_owned()));
        let Some(result) = options.cancel.run_until_cancelled(request.send()).await else {
            return Err(aborted());
        };
        let failure = match result {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let text = response.text().await.unwrap_or_default();
                response_failure(status, &headers, &text)
            }
            Err(err) => Failure {
                message: crate::auth::network_message(&err),
                status: None,
                code: None,
                request_id: None,
                retryable: true,
            },
        };
        if !failure.retryable || attempt >= attempts {
            return Err(failure);
        }
        let throttled = failure.status == Some(429)
            || failure
                .code
                .as_deref()
                .is_some_and(|code| THROTTLING_CODES.contains(&code));
        options
            .cancel
            .run_until_cancelled(tokio::time::sleep(retry_delay(attempt, throttled)))
            .await
            .ok_or_else(aborted)?;
        attempt += 1;
    }
}

fn aborted() -> Failure {
    Failure::plain(http::ABORTED_BEFORE_RESPONSE.into())
}

/// A streamed content block: its Bedrock index while open, and scratch data.
struct Open {
    index: Option<u64>,
    partial_json: String,
    redacted: Vec<u8>,
}

struct State {
    output: AssistantMessage,
    open: Vec<Open>,
    started: bool,
}

impl State {
    fn position(&self, index: u64) -> Option<usize> {
        self.open.iter().position(|open| open.index == Some(index))
    }

    fn push(&mut self, block: ContentBlock, index: u64, sender: &EventSender) -> usize {
        self.open.push(Open {
            index: Some(index),
            partial_json: String::new(),
            redacted: Vec::new(),
        });
        sender.start(&mut self.output, block)
    }

    fn flush_redacted(&mut self, position: usize) {
        let redacted = std::mem::take(&mut self.open[position].redacted);
        if let ContentBlock::Thinking(thinking) = &mut self.output.content[position]
            && thinking.redacted == Some(true)
        {
            thinking.thinking_signature =
                Some(base64::engine::general_purpose::STANDARD.encode(redacted));
        }
    }

    /// Strips streaming scratch state from every block, as pi does when a
    /// stream settles.
    fn finalize(&mut self) {
        for position in 0..self.open.len() {
            self.open[position].index = None;
            if !self.open[position].redacted.is_empty() {
                self.flush_redacted(position);
            }
        }
    }

    fn handle(
        &mut self,
        kind: &str,
        event: &Value,
        model: &Model,
        sender: &EventSender,
    ) -> Result<(), String> {
        match kind {
            "messageStart" => {
                if event["role"] != "assistant" {
                    return Err(
                        "Unexpected assistant message start but got user message start instead"
                            .into(),
                    );
                }
                self.started = true;
                sender.send(StreamEvent::Start(self.output.clone()));
            }
            "contentBlockStart" => {
                let index = event["contentBlockIndex"].as_u64().unwrap_or_default();
                let tool = &event["start"]["toolUse"];
                if tool.is_object() {
                    let call = ToolCall {
                        id: tool["toolUseId"].as_str().unwrap_or_default().to_owned(),
                        name: tool["name"].as_str().unwrap_or_default().to_owned(),
                        arguments: Map::new(),
                        thought_signature: None,
                        namespace: None,
                    };
                    self.push(ContentBlock::ToolCall(call), index, sender);
                }
            }
            "contentBlockDelta" => self.delta(event, sender),
            "contentBlockStop" => {
                let Some(position) = event["contentBlockIndex"]
                    .as_u64()
                    .and_then(|i| self.position(i))
                else {
                    return Ok(());
                };
                self.open[position].index = None;
                match &mut self.output.content[position] {
                    ContentBlock::Thinking(_) => self.flush_redacted(position),
                    ContentBlock::ToolCall(call) => {
                        call.arguments = parse_streaming_json(&self.open[position].partial_json);
                    }
                    _ => {}
                }
                sender.end(&self.output, position);
            }
            "messageStop" => {
                let reason = event["stopReason"].as_str();
                self.output.raw_stop_reason = reason.map(str::to_owned);
                self.output.stop_reason = match reason {
                    Some("end_turn" | "stop_sequence") => StopReason::Stop,
                    Some("max_tokens" | "model_context_window_exceeded") => StopReason::Length,
                    Some("tool_use") => StopReason::ToolUse,
                    Some(other) => {
                        self.output.error_message = Some(format!("Provider stopped with: {other}"));
                        StopReason::Error
                    }
                    None => StopReason::Error,
                };
            }
            "metadata" => {
                let usage_json = &event["usage"];
                if usage_json.is_object() {
                    let usage = &mut self.output.usage;
                    let count = |name: &str| usage_json[name].as_u64().unwrap_or(0);
                    usage.input = count("inputTokens");
                    usage.output = count("outputTokens");
                    usage.cache_read = count("cacheReadInputTokens");
                    usage.cache_write = count("cacheWriteInputTokens");
                    usage.cache_write_1h = usage_json["cacheDetails"].as_array().map(|details| {
                        details
                            .iter()
                            .filter(|detail| detail["ttl"] == "1h")
                            .map(|detail| detail["inputTokens"].as_u64().unwrap_or(0))
                            .sum()
                    });
                    let total = count("totalTokens");
                    usage.total_tokens = Some(if total > 0 {
                        total
                    } else {
                        usage.input + usage.output
                    });
                    calculate_cost(model, usage);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn delta(&mut self, event: &Value, sender: &EventSender) {
        let index = event["contentBlockIndex"].as_u64().unwrap_or_default();
        let delta = &event["delta"];
        let position = self.position(index);
        if let Some(text) = delta["text"].as_str() {
            let position =
                position.unwrap_or_else(|| self.push(ContentBlock::text(""), index, sender));
            if let ContentBlock::Text(_) = self.output.content[position] {
                sender.delta(&mut self.output, position, text);
            }
        } else if delta["toolUse"].is_object() {
            let Some(position) = position else {
                return;
            };
            let piece = delta["toolUse"]["input"].as_str().unwrap_or_default();
            self.open[position].partial_json.push_str(piece);
            let arguments = parse_streaming_json(&self.open[position].partial_json);
            if let ContentBlock::ToolCall(call) = &mut self.output.content[position] {
                call.arguments = arguments;
                sender.delta(&mut self.output, position, piece);
            }
        } else if delta["reasoningContent"].is_object() {
            let reasoning = &delta["reasoningContent"];
            let position = position.unwrap_or_else(|| {
                let block = ContentBlock::thinking("", Some(String::new()));
                self.push(block, index, sender)
            });
            let ContentBlock::Thinking(_) = self.output.content[position] else {
                return;
            };
            if let Some(text) = reasoning["text"].as_str().filter(|text| !text.is_empty()) {
                sender.delta(&mut self.output, position, text);
            }
            let ContentBlock::Thinking(thinking) = &mut self.output.content[position] else {
                return;
            };
            if let Some(signature) = reasoning["signature"].as_str().filter(|s| !s.is_empty())
                && thinking.redacted != Some(true)
            {
                thinking
                    .thinking_signature
                    .get_or_insert_with(String::new)
                    .push_str(signature);
            }
            if let Some(redacted) = reasoning["redactedContent"]
                .as_str()
                .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .filter(|bytes| !bytes.is_empty())
            {
                if thinking.redacted != Some(true) {
                    thinking.redacted = Some(true);
                    thinking.thinking_signature = Some(String::new());
                    sender.delta(&mut self.output, position, REDACTED_THINKING);
                }
                self.open[position].redacted.extend(redacted);
            }
        }
    }
}

/// The modeled exception name of an event stream `:exception-type`.
fn exception_name(kind: &str) -> String {
    let mut chars = kind.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

async fn consume(
    state: &mut State,
    mut response: reqwest::Response,
    model: &Model,
    sender: &EventSender,
    options: &StreamOptions,
) -> Result<(), Failure> {
    let plain = Failure::plain;
    let mut decoder = eventstream::Decoder::default();
    loop {
        let chunk =
            match http::read_chunk(&mut response, &options.cancel, http::ABORTED_DURING_STREAM)
                .await
            {
                Ok(chunk) => chunk,
                Err(message) => return Err(plain(message)),
            };
        let Some(bytes) = chunk else {
            break;
        };
        decoder.push(&bytes);
        while let Some(message) = decoder.next_message().map_err(plain)? {
            let payload: Value = serde_json::from_slice(&message.payload).unwrap_or(Value::Null);
            match message.header(":message-type") {
                Some("event") => {
                    let kind = message.header(":event-type").unwrap_or_default();
                    let mut item = serde_json::Map::new();
                    item.insert(kind.to_owned(), payload.clone());
                    options.hooks.stream_event(&Value::Object(item)).await;
                    state.handle(kind, &payload, model, sender).map_err(plain)?;
                }
                Some("exception") => {
                    let name =
                        exception_name(message.header(":exception-type").unwrap_or_default());
                    let text = payload["message"]
                        .as_str()
                        .or_else(|| payload["Message"].as_str())
                        .unwrap_or("UnknownError");
                    return Err(Failure {
                        message: service_message(&name, text),
                        status: None,
                        code: name.ends_with("Exception").then(|| name.clone()),
                        request_id: None,
                        retryable: false,
                    });
                }
                Some("error") => {
                    let code = message
                        .header(":error-code")
                        .unwrap_or("UnknownError")
                        .to_owned();
                    let text = message.header(":error-message").unwrap_or_default();
                    return Err(Failure {
                        message: text.to_owned(),
                        status: None,
                        code: code.ends_with("Exception").then_some(code),
                        request_id: None,
                        retryable: false,
                    });
                }
                _ => {}
            }
        }
    }
    check_complete(
        &state.output,
        &options.cancel,
        "Bedrock stream ended without a stop reason",
    )
    .map_err(plain)
}

pub(super) async fn run(request: Request, sender: EventSender) {
    let Request {
        model,
        messages,
        options,
    } = request;
    let fail = |mut output: AssistantMessage, failure: Failure, request_id: Option<&str>| {
        if !options.cancel.is_cancelled() {
            append_diagnostic(&mut output, &failure, request_id);
        }
        send_error(&sender, output, &options.cancel, failure.message);
    };
    let output = new_output(&model, now_ms());
    let response = match connect(&model, &messages, &options).await {
        Ok(response) => response,
        Err(failure) => return fail(output, failure, None),
    };
    options.hooks.response(&response).await;
    let request_id = response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|value| value.to_str().ok())
        .and_then(diagnostic_value);
    let mut state = State {
        output,
        open: Vec::new(),
        started: false,
    };
    let result = consume(&mut state, response, &model, &sender, &options).await;
    state.finalize();
    match result {
        Ok(()) => sender.send(StreamEvent::Done(state.output)),
        Err(failure) => fail(state.output, failure, request_id.as_deref()),
    }
}

/// Builds and sends the request; the response once its status is a success.
async fn connect(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
) -> Result<reqwest::Response, Failure> {
    let configured_region = provider_env_value("AWS_REGION", options.env.as_ref())
        .or_else(|| provider_env_value("AWS_DEFAULT_REGION", options.env.as_ref()));
    let thinking = thinking(model, messages, options);
    let body = build_body(
        model,
        messages,
        options,
        &thinking,
        configured_region.as_deref(),
    )
    .map_err(Failure::plain)?;
    let body = yapi_types::json::stringify(&options.hooks.payload(body).await);
    let target = target(model, options).await.map_err(Failure::plain)?;
    send(&target, &body, options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_regions_from_endpoints_and_arns() {
        assert_eq!(
            endpoint_region("https://bedrock-runtime.eu-central-1.amazonaws.com").as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(
            endpoint_region("https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com").as_deref(),
            Some("us-gov-west-1")
        );
        assert_eq!(endpoint_region("http://127.0.0.1:9"), None);
        assert_eq!(
            arn_region("arn:aws:bedrock:us-west-2:123:application-inference-profile/x").as_deref(),
            Some("us-west-2")
        );
        assert_eq!(
            arn_region("arn:aws-us-gov:bedrock:us-gov-east-1:1:x").as_deref(),
            Some("us-gov-east-1")
        );
        assert_eq!(arn_region("anthropic.claude"), None);
    }

    #[test]
    fn formats_errors_like_the_sdk() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-amzn-errortype",
            "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/"
                .parse()
                .unwrap(),
        );
        headers.insert("x-amzn-requestid", "rid-400".parse().unwrap());
        let failure = response_failure(
            400,
            &headers,
            r#"{"message":"The provided model identifier is invalid."}"#,
        );
        assert_eq!(
            failure.message,
            "Validation error: The provided model identifier is invalid."
        );
        assert_eq!(failure.code.as_deref(), Some("ValidationException"));
        assert_eq!(failure.request_id.as_deref(), Some("rid-400"));
        let none = reqwest::header::HeaderMap::new();
        assert_eq!(
            response_failure(403, &none, r#"{"message":"Forbidden"}"#).message,
            "Unknown: Forbidden"
        );
        assert_eq!(
            response_failure(403, &none, "").message,
            "Unknown: UnknownError"
        );
        assert_eq!(
            response_failure(400, &none, r#"{"code":"SomeError","message":"x"}"#).message,
            "SomeError: x"
        );
        assert_eq!(
            response_failure(400, &none, r#"{"__type":"com.amazon.coral.validate#ValidationException","message":"bad input"}"#).message,
            "Validation error: bad input"
        );
        assert_eq!(
            response_failure(502, &none, "<html>Bad Gateway</html>").message,
            "Unexpected token '<', \"<html>Bad \"... is not valid JSON\n  Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object."
        );
        assert_eq!(
            json_parse_error("Forbidden by gateway"),
            "Unexpected token 'F', \"Forbidden by gateway\" is not valid JSON"
        );
        assert!(response_failure(502, &none, "x").retryable);
        assert!(!response_failure(403, &none, "{}").retryable);
        assert_eq!(exception_name("throttlingException"), "ThrottlingException");
    }
}
