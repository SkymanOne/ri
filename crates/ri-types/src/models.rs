//! `models.json`: custom providers, models and overrides.
//!
//! Mirrors the TypeBox schema in `packages/coding-agent/src/core/model-config.ts` in
//! pi `v1.0.0`. pi reads this file but never writes it. Provider `compat` options
//! stay raw JSON until the wire APIs that use them exist.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub providers: IndexMap<String, ProviderConfig>,
}

/// A new provider, or overrides for a built-in one. `apiKey` and header values use
/// the same `$VAR` and `!command` forms as `auth.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Only `"radius"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<IndexMap<String, ModelOverride>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(flatten)]
    pub options: ModelOptions,
}

/// Fields shared by model definitions and overrides.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// pi thinking level to provider value; `None` marks the level unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<IndexMap<String, Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<InputKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<InputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
    /// Required rates in a definition; any subset in an override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

/// Changes to a built-in model; every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub options: ModelOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    Text,
    Image,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageLimits>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize: Option<ImageResize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_message: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_request: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageResize {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg_quality: Option<u8>,
}

/// Prompt cache retention, in seconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<f64>,
}

/// US dollars per million tokens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Higher rates above an input size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CostTier>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostTier {
    pub input_tokens_above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}
