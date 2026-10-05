//! Image generation over OpenRouter's chat completions endpoint.
//!
//! Port of `packages/ai/src/api/openrouter-images.ts` in pi `v1.0.0`.

use indexmap::IndexMap;
use ri_types::classify::{AssistantImages, ImagesContent, ImagesContext, OutcomeReason};
use ri_types::message::{Cost, ImageContent, TextContent, Usage};
use ri_types::model::ImageModel;
use ri_types::models::InputKind;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::http::{self, Failure};
use crate::stream::{StreamOptions, now_ms};

/// Options of one image request.
#[derive(Clone, Debug, Default)]
pub struct ImagesOptions {
    /// Credential for the provider.
    pub api_key: Option<String>,
    /// Extra headers; `None` removes one.
    pub headers: IndexMap<String, Option<String>>,
    /// Retries of retryable failures; none when absent, as the SDK call.
    pub max_retries: Option<u32>,
    /// Largest server-requested retry delay to honor.
    pub max_retry_delay_ms: Option<u64>,
    /// Per-attempt timeout.
    pub timeout_ms: Option<u64>,
    /// Cancels the request.
    pub cancel: CancellationToken,
}

/// Generates images with `model`. Failures are reported in the result, as
/// pi's `generateImages()` does.
pub async fn generate_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: OutcomeReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };
    let result = match model.api.as_str() {
        "openrouter-images" => openrouter(model, context, options, &mut output).await,
        api => Err(format!("No API provider registered for api: {api}")),
    };
    if let Err(message) = result {
        output.stop_reason = if options.cancel.is_cancelled() {
            OutcomeReason::Aborted
        } else {
            OutcomeReason::Error
        };
        output.error_message = Some(message);
    }
    output
}

fn params(model: &ImageModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            // Rust strings hold no lone surrogates, so pi's sanitizing is a no-op.
            ImagesContent::Text(text) => json!({"type": "text", "text": text.text}),
            ImagesContent::Image(image) => json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", image.mime_type, image.data)},
            }),
        })
        .collect();
    let modalities = if model.output.contains(&InputKind::Text) {
        json!(["image", "text"])
    } else {
        json!(["image"])
    };
    json!({
        "model": model.id,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "modalities": modalities,
    })
}

fn usage(raw: &Value, model: &ImageModel) -> Usage {
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    let prompt = count(&raw["prompt_tokens"]);
    let cached = count(&raw["prompt_tokens_details"]["cached_tokens"]);
    let cache_write = count(&raw["prompt_tokens_details"]["cache_write_tokens"]);
    let cache_read = if cache_write > 0 {
        cached.saturating_sub(cache_write)
    } else {
        cached
    };
    let input = prompt.saturating_sub(cache_read + cache_write);
    let output = count(&raw["completion_tokens"]);
    let rate = |per_million: f64, tokens: u64| (per_million / 1_000_000.0) * tokens as f64;
    let mut cost = Cost {
        input: rate(model.cost.input, input),
        output: rate(model.cost.output, output),
        cache_read: rate(model.cost.cache_read, cache_read),
        cache_write: rate(model.cost.cache_write, cache_write),
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: None,
        total_tokens: Some(input + output + cache_read + cache_write),
        cost,
        cache_write_1h: None,
    }
}

async fn openrouter(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    output: &mut AssistantImages,
) -> Result<(), String> {
    let Some(api_key) = options.api_key.as_deref().filter(|key| !key.is_empty()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };
    let body = ri_types::json::to_string(&params(model, context)).unwrap_or_default();
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let authorization = format!("Bearer {api_key}");
    let headers = super::classify::merge_headers([
        vec![
            ("Authorization", Some(authorization.as_str())),
            ("Content-Type", Some("application/json")),
            ("Accept", Some("application/json")),
        ],
        model
            .headers
            .iter()
            .flatten()
            .map(|(name, value)| (name.as_str(), Some(value.as_str())))
            .collect(),
        options
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_deref()))
            .collect(),
    ]);
    let retry = StreamOptions {
        max_retries: options.max_retries.unwrap_or(0),
        max_retry_delay_ms: options.max_retry_delay_ms,
        cancel: options.cancel.clone(),
        ..StreamOptions::default()
    };
    let build = || {
        let mut request = http::client().post(&url).body(body.clone());
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(timeout) = options.timeout_ms {
            request = request.timeout(std::time::Duration::from_millis(timeout));
        }
        request
    };
    let response = match http::send(build, &retry).await {
        Ok(response) => response,
        Err(Failure::Status { status, body }) => {
            return Err(match serde_json::from_str::<Value>(&body) {
                Ok(json) => {
                    let error = json.get("error");
                    let message = http::sdk_status_message(status, error, None);
                    http::provider_error_message(&message, Some(status), error, None)
                }
                Err(_) => http::sdk_status_message(status, None, Some(&body)),
            });
        }
        Err(other) => return Err(other.plain_message().unwrap_or_default()),
    };
    let bytes = tokio::select! {
        () = options.cancel.cancelled() => return Err(http::ABORTED_DURING_STREAM.into()),
        bytes = response.bytes() => bytes.map_err(|_| "Connection error.".to_owned())?,
    };
    let response: Value = serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
    output.response_id = response["id"].as_str().map(str::to_owned);
    if response["usage"].is_object() {
        output.usage = Some(usage(&response["usage"], model));
    }
    let message = &response["choices"][0]["message"];
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        output.output.push(ImagesContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        }));
    }
    for image in message["images"].as_array().into_iter().flatten() {
        let url = image["image_url"]
            .as_str()
            .or_else(|| image["image_url"]["url"].as_str())
            .unwrap_or_default();
        let Some(data_url) = url.strip_prefix("data:") else {
            continue;
        };
        let Some((mime_type, data)) = data_url.split_once(";base64,") else {
            continue;
        };
        if mime_type.is_empty() || mime_type.contains(';') || data.is_empty() {
            continue;
        }
        output.output.push(ImagesContent::Image(ImageContent {
            data: data.to_owned(),
            mime_type: mime_type.to_owned(),
        }));
    }
    Ok(())
}
