//! Catalog models: what a provider serves and how to talk to it.
//!
//! Mirrors `Model` and the compat interfaces in `packages/ai/src/types.ts` in pi
//! `v1.0.0`. The same shape is used by the built-in catalog, by models resolved from
//! `models.json` and by extension-registered providers.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::message::ThinkingLevel;
use crate::models::{CostTier, ImageResize, InputKind, InputLimits, PromptCache};

/// A chat model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    /// Wire API id, such as `anthropic-messages`.
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<InputKind>,
    pub cost: Pricing,
    pub context_window: u64,
    pub max_tokens: u64,
    /// pi thinking level to provider value; `None` marks the level unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<IndexMap<String, Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    /// Wire-API-specific options; read through [`Model::compat`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<InputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// `"chat"` or absent.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// An image-generation model, usable with `generateImages()` only. Fields
/// follow pi's catalog order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModel {
    /// Always `"image"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<InputKind>,
    /// Output modalities; always includes `image`.
    pub output: Vec<InputKind>,
    pub cost: Pricing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<InputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}

/// A structured classifier model, usable with `classify()` only. Fields
/// follow pi's catalog order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierModel {
    /// Always `"classifier"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<InputKind>,
    pub cost: Pricing,
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<InputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}

impl ImageModel {
    /// `provider/id`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

impl ClassifierModel {
    /// `provider/id`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

impl Model {
    /// Whether this is model `id` of `provider`.
    pub fn is(&self, provider: &str, id: &str) -> bool {
        self.provider == provider && self.id == id
    }

    /// `provider/id`, the reference pi prints and accepts.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Reads `compat` as the view for this model's wire API. Unknown or invalid keys
    /// are ignored, as in pi.
    pub fn compat<T: DeserializeOwned + Default>(&self) -> T {
        self.compat
            .as_ref()
            .and_then(|compat| T::deserialize(Value::Object(compat.clone())).ok())
            .unwrap_or_default()
    }

    /// Whether the model accepts image input.
    pub fn accepts_images(&self) -> bool {
        self.input.contains(&InputKind::Image)
    }

    /// The resize profile for images that enter the conversation, from
    /// `inputLimits.images.resize`.
    pub fn image_resize(&self) -> Option<&ImageResize> {
        self.input_limits.as_ref()?.images.as_ref()?.resize.as_ref()
    }

    /// The provider value for a thinking level: `Some(None)` when the map marks it
    /// unsupported, `None` when the map does not mention it.
    pub fn thinking_level_value(&self, level: ThinkingLevel) -> Option<Option<&str>> {
        self.thinking_level_map
            .as_ref()?
            .get(level.as_str())
            .map(Option::as_deref)
    }
}

/// US dollars per million tokens. pi calls this `ModelCost`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// Request-wide rates; the highest tier below the input size applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CostTier>>,
}

/// `compat` for `anthropic-messages`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicMessagesCompat {
    pub supports_eager_tool_input_streaming: Option<bool>,
    pub supports_long_cache_retention: Option<bool>,
    pub send_session_affinity_headers: Option<bool>,
    pub session_affinity_format: Option<String>,
    pub supports_cache_control_on_tools: Option<bool>,
    pub supports_temperature: Option<bool>,
    pub force_adaptive_thinking: Option<bool>,
    pub allow_empty_signature: Option<bool>,
    pub supports_strict_tools: Option<bool>,
    pub supports_mid_convo_effort: Option<bool>,
    pub supports_mid_convo_system_messages: Option<bool>,
    pub supports_mid_convo_tool_changes: Option<bool>,
}

/// `compat` for `openai-completions`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiCompletionsCompat {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    /// `max_completion_tokens` or `max_tokens`.
    pub max_tokens_field: Option<String>,
    pub requires_tool_result_name: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub requires_thinking_as_text: Option<bool>,
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    pub thinking_format: Option<String>,
    pub chat_template_kwargs: Option<Map<String, Value>>,
    pub chat_template_args: Option<Map<String, Value>>,
    pub open_router_routing: Option<Map<String, Value>>,
    pub vercel_gateway_routing: Option<Map<String, Value>>,
    pub zai_tool_stream: Option<bool>,
    pub thinking_token_budget_field: Option<String>,
    pub supports_thinking_token_budget: Option<bool>,
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    pub supports_mid_convo_system_messages: Option<bool>,
    pub supports_mid_convo_tool_additions: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub cache_control_format: Option<String>,
    pub send_session_affinity_headers: Option<bool>,
    pub session_affinity_format: Option<String>,
    pub supports_long_cache_retention: Option<bool>,
    pub vllm_priority: Option<f64>,
}

/// `compat` for the OpenAI Responses family.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiResponsesCompat {
    pub supports_developer_role: Option<bool>,
    pub supports_mid_convo_system_messages: Option<bool>,
    pub session_affinity_format: Option<String>,
    pub supports_long_cache_retention: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    pub supports_additional_tools: Option<bool>,
    pub supports_tool_search: Option<bool>,
    pub supports_explicit_prompt_cache_mode: Option<bool>,
    pub supports_max_output_tokens: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_compat_views() {
        let model: Model = serde_json::from_str(
            r#"{"id":"m","name":"M","api":"openai-completions","provider":"p","baseUrl":"u",
                "reasoning":true,"input":["text"],"cost":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0},
                "contextWindow":10,"maxTokens":5,
                "compat":{"maxTokensField":"max_tokens","supportsStore":false,"supportsOpenAIGrammarTools":true,"unknown":1},
                "thinkingLevelMap":{"minimal":null,"high":"high"}}"#,
        )
        .unwrap();
        let compat: OpenAiCompletionsCompat = model.compat();
        assert_eq!(compat.max_tokens_field.as_deref(), Some("max_tokens"));
        assert_eq!(compat.supports_store, Some(false));
        assert_eq!(compat.supports_open_ai_grammar_tools, Some(true));
        assert_eq!(compat.supports_developer_role, None);
        assert_eq!(
            model.thinking_level_value(ThinkingLevel::Minimal),
            Some(None)
        );
        assert_eq!(
            model.thinking_level_value(ThinkingLevel::High),
            Some(Some("high"))
        );
        assert_eq!(model.thinking_level_value(ThinkingLevel::Low), None);
    }
}
