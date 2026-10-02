//! The models ri can use and the credentials to use them.
//!
//! Layers, as in pi's provider composer: the built-in catalog, then `models.json`
//! (provider base URLs, headers and compat, added or replaced models, overrides),
//! then extension providers. Credentials resolve in pi's order: a runtime key
//! (`--api-key`), `auth.json`, `models.json` `apiKey`, then environment variables.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use ri_types::auth::{AuthFile, Credential};
use ri_types::model::{Model, Pricing};
use ri_types::models::{ModelDefinition, ModelOverride, ModelsConfig, ProviderConfig};
use serde_json::{Map, Value};

use crate::catalog;
use crate::credentials::{self, BEARER_TOKEN_ENV, ProviderEnv};

/// Credentials for one request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Auth {
    /// The API key, if the provider takes one.
    pub api_key: Option<String>,
    /// Headers that carry or accompany the credential; `None` removes a header.
    pub headers: IndexMap<String, Option<String>>,
    /// Where the credential came from, for messages.
    pub source: Option<String>,
}

/// All known models with their configuration.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    config: ModelsConfig,
    auth: AuthFile,
    auth_path: Option<PathBuf>,
    runtime_keys: IndexMap<String, String>,
    error: Option<String>,
}

/// Removes `//` comments and trailing commas outside strings, as pi does for
/// `models.json`.
pub fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c == '"' {
            out.push(c);
            index += 1;
            while index < chars.len() {
                let d = chars[index];
                out.push(d);
                index += 1;
                if d == '\\' && index < chars.len() {
                    out.push(chars[index]);
                    index += 1;
                } else if d == '"' {
                    break;
                }
            }
        } else if c == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
        } else {
            out.push(c);
            index += 1;
        }
    }
    // Trailing commas: a comma followed only by whitespace and a closing bracket.
    let chars: Vec<char> = out.chars().collect();
    let mut result = String::with_capacity(out.len());
    let mut index = 0;
    let mut in_string = false;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            result.push(c);
            if c == '\\' && index + 1 < chars.len() {
                result.push(chars[index + 1]);
                index += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            result.push(c);
        } else if c == ',' {
            let next = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                result.push(c);
            }
        } else {
            result.push(c);
        }
        index += 1;
    }
    result
}

fn merge_compat(
    base: Option<&Map<String, Value>>,
    over: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let Some(over) = over else {
        return base.cloned();
    };
    let mut merged = base.cloned().unwrap_or_default();
    for (key, value) in over {
        merged.insert(key.clone(), value.clone());
    }
    for key in [
        "openRouterRouting",
        "vercelGatewayRouting",
        "chatTemplateKwargs",
        "chatTemplateArgs",
    ] {
        let base_value = base
            .and_then(|base| base.get(key))
            .and_then(Value::as_object);
        let over_value = over.get(key).and_then(Value::as_object);
        if base_value.is_some() || over_value.is_some() {
            let mut nested = base_value.cloned().unwrap_or_default();
            for (k, v) in over_value.into_iter().flatten() {
                nested.insert(k.clone(), v.clone());
            }
            merged.insert(key.into(), Value::Object(nested));
        }
    }
    Some(merged)
}

fn model_from_definition(
    provider: &str,
    definition: &ModelDefinition,
    config: &ProviderConfig,
    defaults: Option<&Model>,
) -> Result<Model, String> {
    let api = definition
        .api
        .clone()
        .or_else(|| config.api.clone())
        .or_else(|| defaults.map(|model| model.api.clone()))
        .ok_or_else(|| {
            format!(
                "Provider {provider}, model {}: no \"api\" specified. Set at provider or model level.",
                definition.id
            )
        })?;
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| config.base_url.clone())
        .or_else(|| defaults.map(|model| model.base_url.clone()))
        .ok_or_else(|| {
            format!("Provider {provider}: \"baseUrl\" is required when defining custom models.")
        })?;
    let options = &definition.options;
    let cost = options
        .cost
        .as_ref()
        .map_or_else(Pricing::default, |cost| Pricing {
            input: cost.input.unwrap_or(0.0),
            output: cost.output.unwrap_or(0.0),
            cache_read: cost.cache_read.unwrap_or(0.0),
            cache_write: cost.cache_write.unwrap_or(0.0),
            tiers: cost.tiers.clone(),
        });
    Ok(Model {
        id: definition.id.clone(),
        name: definition
            .name
            .clone()
            .unwrap_or_else(|| definition.id.clone()),
        api,
        provider: provider.to_owned(),
        base_url,
        reasoning: options.reasoning.unwrap_or(false),
        input: options
            .input
            .clone()
            .unwrap_or_else(|| vec![ri_types::models::InputKind::Text]),
        cost,
        context_window: options.context_window.unwrap_or(128_000),
        max_tokens: options.max_tokens.unwrap_or(16_384),
        thinking_level_map: options.thinking_level_map.clone(),
        prompt_cache: options.prompt_cache.clone(),
        sampling_params: options.sampling_params.clone(),
        compat: merge_compat(config.compat.as_ref(), options.compat.as_ref()),
        input_limits: options.input_limits.clone(),
        headers: None,
        kind: None,
    })
}

fn apply_override(model: &mut Model, over: &ModelOverride) {
    let options = &over.options;
    if let Some(name) = &over.name {
        model.name = name.clone();
    }
    if let Some(reasoning) = options.reasoning {
        model.reasoning = reasoning;
    }
    if let Some(map) = &options.thinking_level_map {
        let mut merged = model.thinking_level_map.clone().unwrap_or_default();
        merged.extend(map.clone());
        model.thinking_level_map = Some(merged);
    }
    if let Some(input) = &options.input {
        model.input = input.clone();
    }
    if let Some(limits) = &options.input_limits {
        let mut merged = model.input_limits.clone().unwrap_or_default();
        if limits.max_request_bytes.is_some() {
            merged.max_request_bytes = limits.max_request_bytes;
        }
        if let Some(images) = &limits.images {
            let mut base = merged.images.clone().unwrap_or_default();
            if images.max_per_message.is_some() {
                base.max_per_message = images.max_per_message;
            }
            if images.max_per_request.is_some() {
                base.max_per_request = images.max_per_request;
            }
            if images.resize.is_some() {
                base.resize = images.resize.clone();
            }
            merged.images = Some(base);
        }
        model.input_limits = Some(merged);
    }
    if let Some(cost) = &options.cost {
        model.cost = Pricing {
            input: cost.input.unwrap_or(model.cost.input),
            output: cost.output.unwrap_or(model.cost.output),
            cache_read: cost.cache_read.unwrap_or(model.cost.cache_read),
            cache_write: cost.cache_write.unwrap_or(model.cost.cache_write),
            tiers: cost.tiers.clone().or_else(|| model.cost.tiers.clone()),
        };
    }
    if let Some(cache) = &options.prompt_cache {
        let mut merged = model.prompt_cache.clone().unwrap_or_default();
        if cache.short.is_some() {
            merged.short = cache.short;
        }
        if cache.long.is_some() {
            merged.long = cache.long;
        }
        model.prompt_cache = Some(merged);
    }
    if let Some(window) = options.context_window {
        model.context_window = window;
    }
    if let Some(max) = options.max_tokens {
        model.max_tokens = max;
    }
    if let Some(params) = &options.sampling_params {
        let mut merged = model.sampling_params.clone().unwrap_or_default();
        merged.extend(params.clone());
        model.sampling_params = Some(merged);
    }
    model.compat = merge_compat(model.compat.as_ref(), options.compat.as_ref());
}

impl ModelRegistry {
    /// Loads the catalog with `models.json` and `auth.json` from `agent_dir`. A
    /// broken `models.json` is reported by [`ModelRegistry::error`] and ignored.
    pub fn load(agent_dir: &Path) -> ModelRegistry {
        let mut registry = ModelRegistry {
            auth_path: Some(agent_dir.join("auth.json")),
            ..ModelRegistry::default()
        };
        registry.auth = std::fs::read_to_string(agent_dir.join("auth.json"))
            .ok()
            .and_then(|text| {
                serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()
            })
            .unwrap_or_default();
        let models_path = agent_dir.join("models.json");
        if let Ok(text) = std::fs::read_to_string(&models_path) {
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            match serde_json::from_str::<ModelsConfig>(&strip_json_comments(text)) {
                Ok(config) => registry.config = config,
                Err(err) => {
                    registry.error =
                        Some(format!("Failed to load {}: {err}", models_path.display()));
                }
            }
        }
        registry.rebuild();
        registry
    }

    /// The catalog alone, with no files.
    pub fn builtin() -> ModelRegistry {
        let mut registry = ModelRegistry::default();
        registry.rebuild();
        registry
    }

    fn rebuild(&mut self) {
        let mut models = catalog::all_builtin_models();
        let mut providers: Vec<String> = catalog::builtin_providers().map(str::to_owned).collect();
        for id in self.config.providers.keys() {
            if !providers.contains(id) {
                providers.push(id.clone());
            }
        }
        let mut errors = Vec::new();
        for (provider, config) in self.config.providers.clone() {
            let (mut own, others): (Vec<Model>, Vec<Model>) = models
                .into_iter()
                .partition(|model| model.provider == provider);
            models = others;
            for model in &mut own {
                if config.oauth.as_deref() != Some("radius")
                    && let Some(base_url) = &config.base_url
                {
                    model.base_url = base_url.clone();
                }
                model.compat = merge_compat(model.compat.as_ref(), config.compat.as_ref());
            }
            for definition in config.models.iter().flatten() {
                let defaults = own
                    .iter()
                    .find(|model| model.id == definition.id)
                    .or_else(|| {
                        let api = definition.api.as_ref().or(config.api.as_ref());
                        api.and_then(|api| own.iter().find(|model| &model.api == api))
                    })
                    .or_else(|| own.iter().find(|model| model.api == "openai-completions"))
                    .or_else(|| own.first())
                    .cloned();
                match model_from_definition(&provider, definition, &config, defaults.as_ref()) {
                    Ok(model) => match own.iter().position(|existing| existing.id == model.id) {
                        Some(index) => own[index] = model,
                        None => own.push(model),
                    },
                    Err(err) => errors.push(err),
                }
            }
            for model in &mut own {
                if let Some(over) = config
                    .model_overrides
                    .as_ref()
                    .and_then(|o| o.get(&model.id))
                {
                    apply_override(model, over);
                }
            }
            models.extend(own);
        }
        // Keep provider order: built-ins in catalog order, then custom providers.
        models.sort_by_key(|model| providers.iter().position(|p| *p == model.provider));
        self.models = models;
        if !errors.is_empty() && self.error.is_none() {
            self.error = Some(errors.join("\n"));
        }
    }

    /// Why `models.json` could not be applied, if it could not.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Every chat model.
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// A model by provider and id.
    pub fn find(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
    }

    /// Adds models from an extension provider, replacing the provider's models.
    pub fn register_provider(&mut self, provider: &str, models: Vec<Model>) {
        self.models.retain(|model| model.provider != provider);
        self.models.extend(models);
    }

    /// Uses `key` for `provider` for this process, ahead of every stored credential.
    pub fn set_runtime_key(&mut self, provider: &str, key: String) {
        self.runtime_keys.insert(provider.to_owned(), key);
    }

    fn credential(&self, provider: &str) -> Option<&Credential> {
        self.auth.get(provider)
    }

    /// Whether some credential is configured for `provider`, without running
    /// commands.
    pub fn has_auth(&self, provider: &str) -> bool {
        if self.runtime_keys.contains_key(provider) {
            return true;
        }
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) => {
                if let Some(key) = &credential.key
                    && (credentials::is_command(key)
                        || credentials::is_configured(key, credential.env.as_ref()))
                {
                    return true;
                }
            }
            Some(Credential::OAuth(_)) => return true,
            None => {}
        }
        if let Some(key) = self
            .config
            .providers
            .get(provider)
            .and_then(|c| c.api_key.as_ref())
            && (credentials::is_command(key) || credentials::is_configured(key, None))
        {
            return true;
        }
        credentials::env_api_key(provider, None).is_some()
    }

    /// Credentials for a request to `model`, with its configured headers.
    pub async fn auth(&self, model: &Model) -> Auth {
        let provider = model.provider.as_str();
        let mut auth = Auth {
            headers: self.headers(model).await,
            ..Auth::default()
        };
        if let Some(key) = self.runtime_keys.get(provider) {
            auth.api_key = Some(key.clone());
            auth.source = Some("--api-key".into());
            return auth;
        }
        let mut env: Option<ProviderEnv> = None;
        if let Some(Credential::ApiKey(credential)) = self.credential(provider) {
            env = credential.env.clone();
            if let Some(key) = &credential.key
                && let Some(value) = credentials::resolve(key, env.as_ref(), true).await
            {
                auth.api_key = Some(value);
                auth.source = Some("stored credential".into());
                return auth;
            }
        }
        if let Some(Credential::OAuth(credential)) = self.credential(provider) {
            auth.api_key = Some(credential.access.clone());
            auth.source = Some("oauth".into());
            return auth;
        }
        if let Some(key) = self
            .config
            .providers
            .get(provider)
            .and_then(|c| c.api_key.as_ref())
            && let Some(value) = credentials::resolve(key, None, false).await
        {
            auth.api_key = Some(value);
            auth.source = Some("models.json".into());
            return auth;
        }
        if let Some((name, value)) = credentials::env_api_key(provider, env.as_ref()) {
            if name == BEARER_TOKEN_ENV {
                auth.headers
                    .insert("Authorization".into(), Some(format!("Bearer {value}")));
            } else {
                auth.api_key = Some(value);
            }
            auth.source = Some(name.to_owned());
        }
        auth
    }

    /// `models.json` headers for a model: provider headers, then the model's
    /// override and definition headers. Values use the credential forms.
    async fn headers(&self, model: &Model) -> IndexMap<String, Option<String>> {
        let mut headers = IndexMap::new();
        let Some(config) = self.config.providers.get(&model.provider) else {
            return headers;
        };
        let override_headers = config
            .model_overrides
            .as_ref()
            .and_then(|overrides| overrides.get(&model.id))
            .and_then(|over| over.options.headers.as_ref());
        let definition_headers = config
            .models
            .iter()
            .flatten()
            .find(|definition| definition.id == model.id)
            .and_then(|definition| definition.options.headers.as_ref());
        for source in [
            config.headers.as_ref(),
            override_headers,
            definition_headers,
        ]
        .into_iter()
        .flatten()
        {
            for (name, value) in source {
                headers.insert(name.clone(), credentials::resolve(value, None, false).await);
            }
        }
        headers
    }

    /// Where `auth.json` lives.
    pub fn auth_path(&self) -> Option<&Path> {
        self.auth_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_and_trailing_commas() {
        assert_eq!(
            strip_json_comments("{\"a\": \"x//y\", // note\n \"b\": [1, 2,],\n}"),
            "{\"a\": \"x//y\", \n \"b\": [1, 2]\n}"
        );
    }
}
