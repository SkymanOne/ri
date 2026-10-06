//! The models yapi can use and the credentials to use them.
//!
//! Layers, as in pi's provider composer: the built-in catalog, then `models.json`
//! (provider base URLs, headers and compat, added or replaced models, overrides),
//! then extension providers. Credentials resolve in pi's order: a runtime key
//! (`--api-key`), `auth.json`, `models.json` `apiKey`, then environment variables.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use indexmap::IndexMap;
use regex_lite::{Captures, Regex};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::{ApiKeyCredential, Credential, OAuthCredential};
use yapi_types::classify::{AssistantImages, ClassifierContext, ClassifierResult, ImagesContext};
use yapi_types::model::{ClassifierModel, ImageModel, Model, Pricing};
use yapi_types::models::{ModelDefinition, ModelOverride, ModelsConfig, ProviderConfig};

use crate::api::classify::{ClassifyOptions, failure_reason};
use crate::api::images::ImagesOptions;
use crate::auth::{
    AuthError, AuthPrompt, CredentialKind, CredentialStore, Interaction, LoginOptions, OAuthAuth,
    OAuthProvider, builtin_oauth, copilot,
};
use crate::catalog;
use crate::credentials::{self, BEARER_TOKEN_ENV, ProviderEnv};
use crate::key_auth::{self, Ambient};
use crate::model_catalog::{self, ModelsStore, ProviderModels, Source, Target};
use crate::providers;

/// Tokens with less validity left than this are refreshed before use.
const OAUTH_MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;
const OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// Credentials for one request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Auth {
    /// The API key, if the provider takes one.
    pub api_key: Option<String>,
    /// Headers that carry or accompany the credential; `None` removes a header.
    pub headers: IndexMap<String, Option<String>>,
    /// Where the credential came from, for messages.
    pub source: Option<String>,
    /// Replaces the model's base URL, for accounts with their own endpoint.
    pub base_url: Option<String>,
    /// Why credentials could not be resolved; a request with it fails.
    pub error: Option<String>,
    /// Provider settings from the credential, such as a Cloudflare account
    /// id, that the request reads ahead of the process environment.
    pub env: Option<ProviderEnv>,
}

impl Auth {
    /// Takes the credentials of a provider-specific resolution.
    fn use_resolved(&mut self, resolved: key_auth::Resolved) {
        self.api_key = resolved.api_key;
        self.headers.extend(resolved.headers);
        self.env = resolved.env;
        self.source = Some(resolved.source);
        if resolved.base_url.is_some() {
            self.base_url = resolved.base_url;
        }
    }

    /// Applies the credentials beneath a classifier or image request's own:
    /// the caller's key and headers win, and the account's base URL replaces
    /// `base_url`.
    fn apply_under(
        self,
        base_url: &mut String,
        api_key: &mut Option<String>,
        headers: &mut IndexMap<String, Option<String>>,
    ) {
        if let Some(url) = self.base_url {
            *base_url = url;
        }
        if api_key.is_none() {
            *api_key = self.api_key;
        }
        let caller = std::mem::replace(headers, self.headers);
        headers.extend(caller);
    }

    /// Applies the credentials to a request: the key when there is one,
    /// headers over the request's, and the account's base URL. Fails
    /// with [`Auth::error`] when credentials could not be resolved.
    pub fn apply(self, request: &mut crate::stream::Request) -> Result<(), String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.api_key.is_some() {
            request.options.api_key = self.api_key;
        }
        request.options.headers.extend(self.headers);
        if let Some(base_url) = self.base_url {
            request.model.base_url = base_url;
        }
        if self.env.is_some() {
            request.options.env = self.env;
        }
        Ok(())
    }
}

/// Where `provider`'s credential comes from when nothing is stored or
/// configured: its key variables, else the provider's ambient sources.
fn environment_source(provider: &str) -> Option<String> {
    if key_auth::is_custom(provider) {
        return key_auth::resolve(provider, None, &Ambient::process()).map(|r| r.source);
    }
    credentials::env_api_key(provider, None)
        .map(|(name, _)| name.to_owned())
        .or_else(|| {
            (provider == "anthropic")
                .then(|| key_auth::anthropic_federation(&Ambient::process()))
                .flatten()
                .map(|resolved| resolved.source)
        })
}

/// A model that stands for a provider in provider-level auth.
/// Replaces the models `key` matches by id with `refreshed` ones, and
/// appends the new ones; pi's `mergeModels`.
fn overlay<T: Clone>(models: &mut Vec<T>, refreshed: &[T], key: impl Fn(&T) -> Option<&str>) {
    for model in refreshed {
        let Some(id) = key(model) else {
            continue;
        };
        match models.iter().position(|existing| key(existing) == Some(id)) {
            Some(index) => models[index] = model.clone(),
            None => models.push(model.clone()),
        }
    }
}

fn placeholder_model(provider: &str) -> Option<Model> {
    serde_json::from_value(serde_json::json!({
        "id": "",
        "name": "",
        "api": "pi-messages",
        "provider": provider,
        "baseUrl": "",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 0,
        "maxTokens": 0,
    }))
    .ok()
}

/// The models of a Radius gateway config that sign-ins before the models
/// store kept in their credential.
fn legacy_radius_models(provider: &str, config: &Value) -> Vec<Model> {
    let Some(base_url) = config["baseUrl"].as_str() else {
        return Vec::new();
    };
    config["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let mut model = model.clone();
            model["api"] = Value::String("pi-messages".into());
            model["provider"] = Value::String(provider.to_owned());
            model["baseUrl"] = Value::String(base_url.to_owned());
            serde_json::from_value(model).ok()
        })
        .collect()
}

/// How `/login` authenticates a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginKind {
    /// An account sign-in.
    OAuth,
    /// A stored API key.
    ApiKey,
}

/// All known models with their configuration.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    config: ModelsConfig,
    store: Arc<CredentialStore>,
    oauth: IndexMap<String, Arc<dyn OAuthProvider>>,
    runtime_keys: IndexMap<String, String>,
    /// Providers whose `apiKey` an extension configures.
    extension_keys: HashSet<String>,
    error: Option<String>,
    /// Classifier models, built-in and refreshed.
    classifiers: Vec<ClassifierModel>,
    /// Image-generation models, built-in and refreshed.
    images: Vec<ImageModel>,
    /// Models from refreshed catalogs, by provider, over the built-in ones.
    dynamic: IndexMap<String, ProviderModels>,
    /// Whether pi's built-in `llama.cpp` extension runs, which provides the
    /// llama.cpp provider.
    llama: bool,
    /// Models extension providers register, by provider.
    extension_models: IndexMap<String, Vec<Model>>,
    /// Where refreshed catalogs persist.
    models_store: Option<ModelsStore>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("models", &self.models.len())
            .field("store", &self.store)
            .field("oauth", &self.oauth.keys().collect::<Vec<_>>())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// Removes `//` comments and trailing commas outside strings, as pi's
/// `stripJsonComments` does for `models.json`.
pub fn strip_json_comments(input: &str) -> String {
    // pi's two regular expressions, with JavaScript's `\s` spelled out.
    static PATTERNS: LazyLock<[Regex; 2]> = LazyLock::new(|| {
        [
            r#""(?:\\.|[^"\\])*"|//[^\n]*"#,
            r#""(?:\\.|[^"\\])*"|,([\t\n\x0B\f\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]*[}\]])"#,
        ]
        .map(|pattern| {
            Regex::new(pattern).expect("constant pattern; `strips_comments_and_trailing_commas`")
        })
    });
    let [comments, commas] = &*PATTERNS;
    let stripped = comments.replace_all(input, |found: &Captures| {
        if found[0].starts_with('"') {
            found[0].to_owned()
        } else {
            String::new()
        }
    });
    commas
        .replace_all(&stripped, |found: &Captures| {
            found
                .get(1)
                .map_or(&found[0], |tail| tail.as_str())
                .to_owned()
        })
        .into_owned()
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
            .unwrap_or_else(|| vec![yapi_types::models::InputKind::Text]),
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
            store: Arc::new(CredentialStore::open(agent_dir.join("auth.json"))),
            ..ModelRegistry::default()
        };
        let models_path = agent_dir.join("models.json");
        let file = models_path.display();
        // pi's three failures: reading, parsing and the schema.
        match std::fs::read_to_string(&models_path) {
            Ok(text) => {
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                match serde_json::from_str::<ModelsConfig>(&strip_json_comments(text)) {
                    Ok(config) => registry.config = config,
                    Err(err) if err.is_data() => {
                        registry.error = Some(format!(
                            "Invalid models.json schema:\n  - {err}\n\nFile: {file}"
                        ));
                    }
                    Err(err) => {
                        registry.error = Some(format!(
                            "Failed to parse models.json: {err}\n\nFile: {file}"
                        ));
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                registry.error = Some(format!("Failed to load models.json: {err}\n\nFile: {file}"));
            }
        }
        registry.models_store = Some(ModelsStore::new(agent_dir.join("models-store.json")));
        registry.restore_catalogs();
        registry.rebuild();
        registry
    }

    /// Restores the catalogs earlier refreshes stored, and catalogs older
    /// Radius sign-ins kept in their credential.
    fn restore_catalogs(&mut self) {
        let Some(store) = &self.models_store else {
            return;
        };
        let stored = store.read_all();
        for (provider, source) in self.catalog_sources() {
            if let Some(entry) = stored.get(&provider) {
                self.dynamic.insert(
                    provider.clone(),
                    model_catalog::restore(&provider, &source, entry),
                );
            } else if matches!(source, Source::Radius { .. })
                && let Some(Credential::OAuth(credential)) = self.store.get(&provider)
                && let Some(config) = credential.extra.get("gatewayConfig")
            {
                let models = legacy_radius_models(&provider, config);
                if !models.is_empty() {
                    self.dynamic.insert(provider, ProviderModels::chat(models));
                }
            }
        }
    }

    /// Providers whose catalogs refresh, and from where: pi.dev for the
    /// built-ins, the gateway for Radius and `models.json` Radius gateways.
    fn catalog_sources(&self) -> Vec<(String, Source)> {
        let mut sources: Vec<(String, Source)> = catalog::builtin_providers()
            .filter(|provider| *provider != "radius")
            .map(|provider| (provider.to_owned(), Source::Remote))
            .collect();
        sources.push((
            "radius".into(),
            Source::Radius {
                gateway: crate::auth::radius::DEFAULT_GATEWAY.into(),
            },
        ));
        for provider in self.config.providers.keys() {
            if let Some(gateway) = self.radius_gateway(provider) {
                let gateway = crate::auth::radius::normalize_gateway(&gateway);
                sources.retain(|(id, _)| id != provider);
                sources.push((provider.clone(), Source::Radius { gateway }));
            }
        }
        if self.llama {
            // pi refreshes llama.cpp only for a stored server.
            let server = match self.store.get(crate::llama::PROVIDER_ID) {
                Some(Credential::ApiKey(credential)) => credential
                    .env
                    .as_ref()
                    .and_then(|env| env.get(crate::llama::BASE_URL_ENV))
                    .and_then(|url| crate::llama::normalize_server_url(url).ok()),
                _ => None,
            };
            sources.push((
                crate::llama::PROVIDER_ID.into(),
                Source::Llama {
                    server: server.unwrap_or_default(),
                },
            ));
        }
        sources
    }

    /// Provides pi's llama.cpp provider, as its built-in extension does.
    pub fn enable_llama(&mut self) {
        if self.llama {
            return;
        }
        self.llama = true;
        // Without a stored catalog the provider has no models yet, so the
        // model list stays as it is.
        if let Some(store) = &self.models_store
            && let Some(entry) = store.read(crate::llama::PROVIDER_ID)
        {
            let source = Source::Llama {
                server: String::new(),
            };
            self.dynamic.insert(
                crate::llama::PROVIDER_ID.into(),
                model_catalog::restore(crate::llama::PROVIDER_ID, &source, &entry),
            );
            self.rebuild();
        }
    }

    /// Whether the llama.cpp provider is present.
    pub fn llama_enabled(&self) -> bool {
        self.llama
    }

    /// The llama.cpp server and its key, when the provider is configured.
    pub async fn llama_server(&self) -> Option<(String, Option<String>)> {
        if !self.llama {
            return None;
        }
        let credential = match self.store.get(crate::llama::PROVIDER_ID) {
            Some(Credential::ApiKey(credential)) => Some(credential),
            _ => None,
        };
        let resolved = key_auth::resolve(
            crate::llama::PROVIDER_ID,
            credential.as_ref(),
            &Ambient::process(),
        )?;
        let server = resolved
            .env
            .as_ref()
            .and_then(|env| env.get(crate::llama::BASE_URL_ENV))
            .cloned()?;
        // A stored key, or the environment's; pi sends no "local" placeholder.
        let key = credential
            .and_then(|credential| credential.key)
            .or_else(|| std::env::var("LLAMA_API_KEY").ok());
        Some((server, key))
    }

    /// What a catalog refresh needs: each refreshable provider, whether it
    /// is configured, and its token for gateways that require one.
    pub async fn catalog_targets(&self) -> Vec<Target> {
        let mut targets = Vec::new();
        for (provider, source) in self.catalog_sources() {
            let configured = self.has_auth(&provider);
            let token = match (&source, configured) {
                (Source::Radius { .. }, true) => self.provider_token(&provider).await,
                // pi sends the stored key only.
                (Source::Llama { .. }, true) => match self.store.get(&provider) {
                    Some(Credential::ApiKey(credential)) => credential.key,
                    _ => None,
                },
                _ => None,
            };
            let configured =
                configured && !matches!(&source, Source::Llama { server } if server.is_empty());
            targets.push(Target {
                provider,
                source,
                token,
                configured,
            });
        }
        targets
    }

    /// Where refreshed catalogs persist, when the registry was loaded from an
    /// agent directory.
    pub fn models_store(&self) -> Option<&ModelsStore> {
        self.models_store.as_ref()
    }

    /// Replaces the refreshed catalogs of the providers in `models`.
    pub fn apply_catalogs(&mut self, models: IndexMap<String, ProviderModels>) {
        self.dynamic.extend(models);
        self.rebuild();
    }

    /// A token for provider-level calls such as catalog refreshes: the API key
    /// or bearer token a request would send, refreshed first when it is an
    /// expiring OAuth token.
    pub async fn provider_token(&self, provider: &str) -> Option<String> {
        let model = self
            .models
            .iter()
            .find(|model| model.provider == provider)
            .cloned()
            .or_else(|| placeholder_model(provider))?;
        let auth = self.auth(&model).await;
        if auth.error.is_some() {
            return None;
        }
        auth.api_key.or_else(|| {
            auth.headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .and_then(|(_, value)| value.as_deref())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::to_owned)
        })
    }

    /// The catalog alone, with no files.
    pub fn builtin() -> ModelRegistry {
        let mut registry = ModelRegistry::default();
        registry.rebuild();
        registry
    }

    fn rebuild(&mut self) {
        let mut models = catalog::all_builtin_models();
        let mut classifiers = catalog::all_builtin_classifiers();
        let mut images = catalog::all_builtin_image_models();
        for (provider, refreshed) in &self.dynamic {
            overlay(&mut models, &refreshed.chat, |model| {
                (model.provider == *provider).then_some(model.id.as_str())
            });
            overlay(&mut classifiers, &refreshed.classifiers, |model| {
                (model.provider == *provider).then_some(model.id.as_str())
            });
            overlay(&mut images, &refreshed.images, |model| {
                (model.provider == *provider).then_some(model.id.as_str())
            });
        }
        self.classifiers = classifiers;
        self.images = images;
        let mut providers: Vec<String> = catalog::builtin_providers().map(str::to_owned).collect();
        for id in self.config.providers.keys() {
            if !providers.contains(id) {
                providers.push(id.clone());
            }
        }
        if self.llama && !providers.iter().any(|id| id == crate::llama::PROVIDER_ID) {
            providers.push(crate::llama::PROVIDER_ID.into());
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
        for (provider, registered) in &self.extension_models {
            models.retain(|model| model.provider != *provider);
            models.extend(registered.iter().cloned());
        }
        // Keep provider order: built-ins in catalog order, then custom providers.
        models.sort_by_cached_key(|model| providers.iter().position(|p| *p == model.provider));
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

    /// Every classifier model, built-in and refreshed.
    pub fn classifiers(&self) -> &[ClassifierModel] {
        &self.classifiers
    }

    /// Every image-generation model, built-in and refreshed.
    pub fn image_models(&self) -> &[ImageModel] {
        &self.images
    }

    /// Classifier models whose providers are configured.
    pub fn available_classifiers(&self) -> Vec<&ClassifierModel> {
        self.configured(&self.classifiers, |model| &model.provider)
    }

    /// Image models whose providers are configured.
    pub fn available_image_models(&self) -> Vec<&ImageModel> {
        self.configured(&self.images, |model| &model.provider)
    }

    /// The `models` whose providers are configured.
    fn configured<'a, T>(&self, models: &'a [T], provider: fn(&T) -> &String) -> Vec<&'a T> {
        let mut configured: HashMap<&str, bool> = HashMap::new();
        models
            .iter()
            .filter(|model| {
                *configured
                    .entry(provider(model))
                    .or_insert_with(|| self.has_auth(provider(model)))
            })
            .collect()
    }

    /// Credentials for a provider-level request with `base_url`, `api` and
    /// static `headers`: pi's `applyAuth` for any model type. Fails with pi's
    /// message when the provider is not configured.
    async fn model_auth(
        &self,
        provider: &str,
        id: &str,
        api: &str,
        base_url: &str,
        headers: Option<&IndexMap<String, String>>,
    ) -> Result<Auth, String> {
        let mut model = placeholder_model(provider).ok_or("invalid model")?;
        model.id = id.to_owned();
        model.api = api.to_owned();
        model.base_url = base_url.to_owned();
        model.headers = headers.cloned();
        let auth = self.auth(&model).await;
        if let Some(error) = auth.error {
            return Err(error);
        }
        if auth.api_key.is_none() && auth.source.is_none() {
            return Err(format!("Provider is not configured: {provider}"));
        }
        Ok(auth)
    }

    /// Classifies `context` with `model`, resolving the provider's
    /// credentials; pi's `Models.classify`. Failures are in the result.
    pub async fn classify(
        &self,
        model: &ClassifierModel,
        context: &ClassifierContext,
        mut options: ClassifyOptions,
    ) -> ClassifierResult {
        let mut model = model.clone();
        let auth = self
            .model_auth(
                &model.provider,
                &model.id,
                &model.api,
                &model.base_url,
                model.headers.as_ref(),
            )
            .await;
        match auth {
            Ok(auth) => auth.apply_under(
                &mut model.base_url,
                &mut options.api_key,
                &mut options.headers,
            ),
            Err(message) => {
                return ClassifierResult {
                    api: model.api,
                    provider: model.provider,
                    model: model.id,
                    answers: IndexMap::new(),
                    usage: None,
                    stop_reason: failure_reason(&options.cancel),
                    error_message: Some(message),
                    timestamp: crate::stream::now_ms(),
                };
            }
        }
        crate::api::classify::classify(&model, context, &options).await
    }

    /// Generates images with `model`, resolving the provider's credentials;
    /// pi's `Models.generateImages`. Failures are in the result.
    pub async fn generate_images(
        &self,
        model: &ImageModel,
        context: &ImagesContext,
        mut options: ImagesOptions,
    ) -> AssistantImages {
        let mut model = model.clone();
        let auth = self
            .model_auth(
                &model.provider,
                &model.id,
                &model.api,
                &model.base_url,
                model.headers.as_ref(),
            )
            .await;
        match auth {
            Ok(auth) => auth.apply_under(
                &mut model.base_url,
                &mut options.api_key,
                &mut options.headers,
            ),
            Err(message) => {
                return AssistantImages {
                    api: model.api,
                    provider: model.provider,
                    model: model.id,
                    output: Vec::new(),
                    response_id: None,
                    usage: None,
                    stop_reason: failure_reason(&options.cancel),
                    error_message: Some(message),
                    timestamp: crate::stream::now_ms(),
                };
            }
        }
        crate::api::images::generate_images(&model, context, &options).await
    }

    /// Adds a provider an extension configures, as a `models.json` entry
    /// that replaces any of the same name; pi's `registerProvider`.
    pub fn register_config(&mut self, provider: &str, config: ProviderConfig) {
        if config.api_key.is_some() {
            self.extension_keys.insert(provider.to_owned());
        }
        self.config.providers.insert(provider.to_owned(), config);
        self.rebuild();
    }

    /// Adds models from an extension provider, replacing the provider's models.
    pub fn register_provider(&mut self, provider: &str, models: Vec<Model>) {
        self.extension_models
            .insert(provider.to_owned(), models.clone());
        self.models.retain(|model| model.provider != provider);
        self.models.extend(models);
    }

    /// Uses `flow` for `provider`'s sign-in and refresh, replacing the built-in.
    pub fn register_oauth(&mut self, provider: &str, flow: Arc<dyn OAuthProvider>) {
        self.oauth.insert(provider.to_owned(), flow);
    }

    /// The sign-in for `provider`: a registered one, a `models.json` Radius
    /// gateway's, else yapi's built-in.
    pub fn oauth_flow(&self, provider: &str) -> Option<Arc<dyn OAuthProvider>> {
        if let Some(flow) = self.oauth.get(provider) {
            return Some(Arc::clone(flow));
        }
        if let Some(gateway) = self.radius_gateway(provider) {
            let name = self.provider_name(provider);
            return Some(Arc::new(crate::auth::radius::RadiusOAuth::new(
                &name, &gateway,
            )));
        }
        builtin_oauth(provider)
    }

    /// The gateway of a `models.json` provider with `"oauth": "radius"`: its
    /// base URL without a trailing `/v1`.
    pub fn radius_gateway(&self, provider: &str) -> Option<String> {
        let config = self.config.providers.get(provider)?;
        if config.oauth.as_deref() != Some("radius") {
            return None;
        }
        let base = config.base_url.as_deref()?.trim_end_matches('/');
        Some(base.strip_suffix("/v1").unwrap_or(base).to_owned())
    }

    /// Uses `key` for `provider` for this process, ahead of every stored credential.
    pub fn set_runtime_key(&mut self, provider: &str, key: String) {
        self.runtime_keys.insert(provider.to_owned(), key);
    }

    fn credential(&self, provider: &str) -> Option<Credential> {
        self.store.get(provider)
    }

    /// The credential store behind this registry.
    pub fn store(&self) -> &Arc<CredentialStore> {
        &self.store
    }

    /// Whether some credential is configured for `provider`, without running
    /// commands.
    pub fn has_auth(&self, provider: &str) -> bool {
        self.auth_source(provider).is_some()
    }

    /// pi's failure for `provider` while `auth.json` cannot be read: requests
    /// that would consult it fail instead of using other credentials.
    pub fn store_error(&self, provider: &str) -> Option<String> {
        if self.runtime_keys.contains_key(provider) {
            return None;
        }
        self.store
            .read_error()
            .map(|error| format!("Credential store read failed for {provider}: {error}"))
    }

    /// Whether `provider`'s credential comes from a `!command`, whose output
    /// decides whether there is a key at all.
    pub fn uses_command_key(&self, provider: &str) -> bool {
        if self.runtime_keys.contains_key(provider) {
            return false;
        }
        let stored = match self.credential(provider) {
            Some(Credential::ApiKey(credential)) => credential.key,
            Some(Credential::OAuth(_)) => return false,
            None => None,
        };
        let configured = self
            .config
            .providers
            .get(provider)
            .and_then(|config| config.api_key.clone());
        [stored, configured]
            .iter()
            .flatten()
            .any(|key| credentials::is_command(key))
    }

    /// Where `provider`'s credential comes from, without running commands;
    /// `None` when it has none.
    fn auth_source(&self, provider: &str) -> Option<String> {
        if self.runtime_keys.contains_key(provider) {
            return Some("--api-key".into());
        }
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) => {
                if key_auth::is_custom(provider) {
                    // Whether the key resolves is unknown without running its
                    // command, so a command counts as a key here.
                    let key = credential.key.clone().filter(|key| {
                        credentials::is_command(key)
                            || credentials::is_configured(key, credential.env.as_ref())
                    });
                    let credential = ApiKeyCredential { key, ..credential };
                    return key_auth::resolve(provider, Some(&credential), &Ambient::process())
                        .map(|_| "stored credential".into());
                }
                // An empty stored key counts as none, as in pi.
                if let Some(key) = credential.key.as_ref().filter(|key| !key.is_empty())
                    && (credentials::is_command(key)
                        || credentials::is_configured(key, credential.env.as_ref()))
                {
                    return Some("stored credential".into());
                }
            }
            Some(Credential::OAuth(_)) => return Some("OAuth".into()),
            None => {}
        }
        // A `models.json` key replaces the provider's own resolution, so the
        // environment is not consulted when it cannot be resolved.
        if let Some(key) = self.configured_key(provider) {
            return (credentials::is_command(key) || credentials::is_configured(key, None))
                .then(|| "configured API key".into());
        }
        environment_source(provider)
    }

    /// How `provider` is configured, as pi's `getProviderAuthStatus` labels
    /// it in `/login`: `runtime`, `stored`, `models_json_command`,
    /// `models_json_key`, `fallback` (an extension's key), or the environment
    /// variables the key comes from. `None` when it is not configured.
    pub fn login_status(&self, provider: &str) -> Option<String> {
        if self.runtime_keys.contains_key(provider) {
            return Some("runtime".into());
        }
        if self.store.list().iter().any(|(id, _)| id == provider) {
            return Some("stored".into());
        }
        let configured = self
            .config
            .providers
            .get(provider)
            .and_then(|config| config.api_key.as_ref());
        if let Some(key) = configured {
            if credentials::is_command(key) {
                return Some("models_json_command".into());
            }
            let names = credentials::env_var_names(key);
            if !names.is_empty() {
                return credentials::is_configured(key, None).then(|| names.join(", "));
            }
            let source = if self.extension_keys.contains(provider) {
                "fallback"
            } else {
                "models_json_key"
            };
            return Some(source.into());
        }
        environment_source(provider)
    }

    /// The `apiKey` that `models.json` configures for `provider`, if any.
    fn configured_key(&self, provider: &str) -> Option<&String> {
        self.config
            .providers
            .get(provider)
            .and_then(|config| config.api_key.as_ref())
            .filter(|key| !key.is_empty())
    }

    /// Whether `provider` authenticates with a stored OAuth credential; a
    /// runtime key takes its place.
    pub fn is_using_oauth(&self, provider: &str) -> bool {
        !self.runtime_keys.contains_key(provider)
            && matches!(self.credential(provider), Some(Credential::OAuth(_)))
    }

    /// Stored credentials by provider, in file order.
    pub fn stored_credentials(&self) -> Vec<(String, CredentialKind)> {
        self.store.list()
    }

    /// Models with credentials. A GitHub Copilot sign-in limits its models to
    /// those the account enables.
    pub fn available(&self) -> Vec<&Model> {
        let copilot_ids = match self.credential("github-copilot") {
            Some(Credential::OAuth(credential)) => copilot::available_model_ids(&credential),
            _ => None,
        };
        let mut models = self.configured(&self.models, |model| &model.provider);
        models.retain(|model| {
            model.provider != "github-copilot"
                || copilot_ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&model.id))
        });
        models
    }

    /// The API key a request to `provider` would use, when it is known
    /// without running a command or signing in: a runtime key, a stored or
    /// configured template value, or the environment.
    pub fn known_api_key(&self, provider: &str) -> Option<String> {
        if let Some(key) = self.runtime_keys.get(provider) {
            return Some(key.clone());
        }
        let mut env = None;
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) if key_auth::is_custom(provider) => {
                let key = credential
                    .key
                    .as_deref()
                    .and_then(|key| credentials::resolve_template(key, credential.env.as_ref()));
                let credential = ApiKeyCredential { key, ..credential };
                return key_auth::resolve(provider, Some(&credential), &Ambient::process())
                    .and_then(|resolved| resolved.api_key);
            }
            Some(Credential::ApiKey(credential)) => {
                if let Some(value) = credential
                    .key
                    .as_deref()
                    .and_then(|key| credentials::resolve_template(key, credential.env.as_ref()))
                    .filter(|value| !value.is_empty())
                {
                    return Some(value);
                }
                env = credential.env;
            }
            Some(Credential::OAuth(_)) => return None,
            None => {}
        }
        if let Some(key) = self.configured_key(provider) {
            return credentials::resolve_template(key, None).filter(|value| !value.is_empty());
        }
        if key_auth::is_custom(provider) {
            return key_auth::resolve(provider, None, &Ambient::process())
                .and_then(|resolved| resolved.api_key);
        }
        credentials::env_api_key(provider, env.as_ref()).map(|(_, value)| value)
    }

    /// Credentials for a request to `model`, with its configured headers. A
    /// stored OAuth token about to expire is refreshed first.
    pub async fn auth(&self, model: &Model) -> Auth {
        self.auth_valid_for(model, OAUTH_MINIMUM_VALIDITY_MS).await
    }

    /// [`ModelRegistry::auth`], refreshing a stored OAuth token that is valid
    /// for less than `min_validity_ms`, as pi's `minOAuthValidityMs`.
    pub async fn auth_valid_for(&self, model: &Model, min_validity_ms: u64) -> Auth {
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
        if let Some(error) = self.store_error(provider) {
            auth.error = Some(error);
            return auth;
        }
        let mut env: Option<ProviderEnv> = None;
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) if key_auth::is_custom(provider) => {
                // The stored credential owns the provider: an unresolved key
                // leaves the provider's own fallbacks, not other sources.
                let key = match &credential.key {
                    Some(key) => credentials::resolve(key, credential.env.as_ref(), true).await,
                    None => None,
                };
                let credential = ApiKeyCredential { key, ..credential };
                if let Some(resolved) =
                    key_auth::resolve(provider, Some(&credential), &Ambient::process())
                {
                    auth.use_resolved(resolved);
                }
                return auth;
            }
            Some(Credential::ApiKey(credential)) => {
                env = credential.env.clone();
                if let Some(key) = &credential.key
                    && let Some(value) = credentials::resolve(key, env.as_ref(), true)
                        .await
                        .filter(|value| !value.is_empty())
                {
                    auth.api_key = Some(value);
                    auth.source = Some("stored credential".into());
                    return auth;
                }
            }
            Some(Credential::OAuth(credential)) => {
                match self.oauth_auth(provider, credential, min_validity_ms).await {
                    Ok(Some(oauth)) => {
                        auth.api_key = oauth.api_key;
                        auth.headers.extend(oauth.headers);
                        auth.base_url = oauth.base_url;
                        auth.source = Some("OAuth".into());
                    }
                    Ok(None) => {}
                    Err(message) => auth.error = Some(message),
                }
                return auth;
            }
            None => {}
        }
        if let Some(key) = self.configured_key(provider) {
            match credentials::resolve(key, None, false).await {
                Some(value) if !value.is_empty() => {
                    auth.api_key = Some(value);
                    auth.source = Some("models.json".into());
                }
                // pi resolves the configured key or fails, without falling
                // back to the environment.
                _ => {
                    let description = format!("API key for provider \"{provider}\"");
                    auth.error = Some(format!(
                        "API key auth failed for provider {provider}: {}",
                        credentials::unresolved_message(key, &description, None)
                    ));
                }
            }
            return auth;
        }
        if key_auth::is_custom(provider) {
            if let Some(resolved) = key_auth::resolve(provider, None, &Ambient::process()) {
                auth.use_resolved(resolved);
            }
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
        } else if provider == "anthropic"
            && let Some(resolved) = key_auth::anthropic_federation(&Ambient::process())
        {
            auth.use_resolved(resolved);
        }
        auth
    }

    /// Request credentials from a stored OAuth token. A token that expires
    /// within `min_validity_ms` is refreshed under the `auth.json` lock, after
    /// checking again that no other process refreshed it. `Ok(None)` means the
    /// provider was logged out meanwhile.
    async fn oauth_auth(
        &self,
        provider: &str,
        stored: OAuthCredential,
        min_validity_ms: u64,
    ) -> Result<Option<OAuthAuth>, String> {
        let Some(flow) = self.oauth_flow(provider) else {
            // yapi has no sign-in for this provider: use the token as stored.
            return Ok(Some(OAuthAuth {
                api_key: Some(stored.access),
                ..OAuthAuth::default()
            }));
        };
        let expires_soon = |credential: &OAuthCredential| {
            crate::auth::now_ms() + min_validity_ms >= credential.expires
        };
        let mut credential = stored;
        if expires_soon(&credential) {
            let cancel = CancellationToken::new();
            let (flow_ref, cancel_ref) = (&flow, &cancel);
            let refreshed = self
                .store
                .modify(
                    provider,
                    |current| async move {
                        let Some(Credential::OAuth(current)) = current else {
                            return Ok(None);
                        };
                        if !expires_soon(&current) {
                            return Ok(None);
                        }
                        let refresh = flow_ref.refresh(&current, cancel_ref);
                        match tokio::time::timeout(OAUTH_REFRESH_TIMEOUT, refresh).await {
                            Ok(result) => result.map(|next| Some(Credential::OAuth(next))),
                            Err(_) => Err(AuthError::failed(
                                "The operation was aborted due to timeout",
                            )),
                        }
                    },
                    &cancel,
                )
                .await
                .map_err(|err| format!("OAuth refresh failed for {provider}: {err}"))?;
            match refreshed {
                Some(Credential::OAuth(next)) => credential = next,
                _ => return Ok(None),
            }
        }
        Ok(Some(flow.to_auth(&credential)))
    }

    /// The display name of a provider: its `models.json` or extension
    /// `name`, else the built-in name, else its id.
    pub fn provider_name(&self, provider: &str) -> String {
        if let Some(name) = self
            .config
            .providers
            .get(provider)
            .and_then(|config| config.name.as_ref())
        {
            return name.clone();
        }
        providers::info(provider).map_or_else(|| provider.to_owned(), |info| info.name.to_owned())
    }

    /// Runs a sign-in and stores the credential. An API key login asks for the
    /// key; an OAuth login runs the provider's flow.
    pub async fn login(
        &self,
        provider: &str,
        kind: LoginKind,
        interaction: &Interaction,
        options: &LoginOptions,
    ) -> Result<Credential, AuthError> {
        interaction.check()?;
        let credential = match kind {
            LoginKind::OAuth => {
                let flow = self.oauth_flow(provider).ok_or_else(|| {
                    AuthError::Failed(format!(
                        "{} does not support oauth login",
                        self.provider_name(provider)
                    ))
                })?;
                Credential::OAuth(flow.login(interaction, options).await?)
            }
            LoginKind::ApiKey if key_auth::is_custom(provider) => Credential::ApiKey(
                key_auth::login(provider, interaction)
                    .await
                    .unwrap_or_else(|| Err(AuthError::failed("No login for provider")))?,
            ),
            LoginKind::ApiKey => {
                let name = providers::info(provider)
                    .and_then(|info| info.api_key)
                    .map_or("API key", |method| method.name);
                let key = interaction
                    .prompt(AuthPrompt::Secret {
                        message: format!("Enter {name}"),
                    })
                    .await?;
                interaction.check()?;
                Credential::ApiKey(ApiKeyCredential {
                    key: Some(key),
                    env: None,
                })
            }
        };
        self.store
            .set(provider, credential.clone(), interaction.cancel())
            .await
            .map_err(|err| match err {
                AuthError::Cancelled => AuthError::Cancelled,
                AuthError::Failed(message) => AuthError::Failed(format!(
                    "Credential store modify failed for {provider}: {message}"
                )),
            })?;
        Ok(credential)
    }

    /// Removes `provider`'s stored credential.
    pub async fn logout(&self, provider: &str) -> Result<(), AuthError> {
        self.store
            .delete(provider, &CancellationToken::new())
            .await
            .map_err(|err| {
                AuthError::Failed(format!(
                    "Credential store delete failed for {provider}: {err}"
                ))
            })
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
        self.store.path()
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
        // JavaScript's `\s` before the bracket; an escaped quote stays in the string.
        assert_eq!(
            strip_json_comments("[1,\u{a0}\u{feff}] \"a\\\"// b\""),
            "[1\u{a0}\u{feff}] \"a\\\"// b\""
        );
    }

    /// pi's `/login` labels for each credential source, and names from
    /// `models.json`.
    #[test]
    fn login_status_follows_pi() {
        let dir = std::env::temp_dir().join(format!("yapi-login-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("models.json"),
            r#"{"providers": {
                "openai": {"apiKey": "!printf key", "name": "OpenAI Renamed"},
                "mistral": {"apiKey": "$HOME"},
                "groq": {"apiKey": "${YAPI_LOGIN_STATUS_UNSET}"},
                "xai": {"apiKey": "literal"}
            }}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"anthropic": {"type": "api_key", "key": "$YAPI_LOGIN_STATUS_UNSET"},
                "github-copilot": {"type": "oauth", "access": "a", "refresh": "r", "expires": 0}}"#,
        )
        .unwrap();
        let mut registry = ModelRegistry::load(&dir);
        let status = |registry: &ModelRegistry, id: &str| registry.login_status(id);
        assert_eq!(status(&registry, "anthropic").as_deref(), Some("stored"));
        assert_eq!(
            status(&registry, "openai").as_deref(),
            Some("models_json_command")
        );
        assert_eq!(status(&registry, "mistral").as_deref(), Some("HOME"));
        assert_eq!(status(&registry, "groq"), None);
        assert_eq!(status(&registry, "xai").as_deref(), Some("models_json_key"));
        assert_eq!(registry.provider_name("openai"), "OpenAI Renamed");
        assert_eq!(registry.provider_name("mistral"), "Mistral");

        assert!(registry.is_using_oauth("github-copilot"));
        registry.set_runtime_key("github-copilot", "key".into());
        assert_eq!(
            status(&registry, "github-copilot").as_deref(),
            Some("runtime")
        );
        assert!(!registry.is_using_oauth("github-copilot"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
