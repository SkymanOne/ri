//! The models ri can use and the credentials to use them.
//!
//! Layers, as in pi's provider composer: the built-in catalog, then `models.json`
//! (provider base URLs, headers and compat, added or replaced models, overrides),
//! then extension providers. Credentials resolve in pi's order: a runtime key
//! (`--api-key`), `auth.json`, `models.json` `apiKey`, then environment variables.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use ri_types::auth::{ApiKeyCredential, Credential, OAuthCredential};
use ri_types::model::{Model, Pricing};
use ri_types::models::{ModelDefinition, ModelOverride, ModelsConfig, ProviderConfig};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::{
    AuthError, AuthPrompt, CredentialKind, CredentialStore, Interaction, LoginOptions, OAuthAuth,
    OAuthProvider, builtin_oauth, copilot,
};
use crate::catalog;
use crate::credentials::{self, BEARER_TOKEN_ENV, ProviderEnv};
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
}

impl Auth {
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
        Ok(())
    }
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
#[derive(Default)]
pub struct ModelRegistry {
    models: Vec<Model>,
    config: ModelsConfig,
    store: Arc<CredentialStore>,
    oauth: IndexMap<String, Arc<dyn OAuthProvider>>,
    runtime_keys: IndexMap<String, String>,
    error: Option<String>,
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
            store: Arc::new(CredentialStore::open(agent_dir.join("auth.json"))),
            ..ModelRegistry::default()
        };
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

    /// Uses `flow` for `provider`'s sign-in and refresh, replacing the built-in.
    pub fn register_oauth(&mut self, provider: &str, flow: Arc<dyn OAuthProvider>) {
        self.oauth.insert(provider.to_owned(), flow);
    }

    /// The sign-in for `provider`: a registered one, else ri's built-in.
    pub fn oauth_flow(&self, provider: &str) -> Option<Arc<dyn OAuthProvider>> {
        self.oauth
            .get(provider)
            .cloned()
            .or_else(|| builtin_oauth(provider))
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

    /// Where `provider`'s credential comes from, as pi labels it in `/login`:
    /// `stored credential`, `OAuth`, `configured API key` or an environment
    /// variable. `None` when it has none. Commands are not run.
    pub fn auth_source(&self, provider: &str) -> Option<String> {
        if self.runtime_keys.contains_key(provider) {
            return Some("--api-key".into());
        }
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) => {
                if let Some(key) = &credential.key
                    && (credentials::is_command(key)
                        || credentials::is_configured(key, credential.env.as_ref()))
                {
                    return Some("stored credential".into());
                }
            }
            Some(Credential::OAuth(_)) => return Some("OAuth".into()),
            None => {}
        }
        if let Some(key) = self
            .config
            .providers
            .get(provider)
            .and_then(|c| c.api_key.as_ref())
            && (credentials::is_command(key) || credentials::is_configured(key, None))
        {
            return Some("configured API key".into());
        }
        credentials::env_api_key(provider, None).map(|(name, _)| name.to_owned())
    }

    /// Whether `provider` authenticates with a stored OAuth credential.
    pub fn is_using_oauth(&self, provider: &str) -> bool {
        matches!(self.credential(provider), Some(Credential::OAuth(_)))
    }

    /// Stored credentials by provider, in file order.
    pub fn stored_credentials(&self) -> Vec<(String, CredentialKind)> {
        self.store.list()
    }

    /// Models with credentials. A GitHub Copilot sign-in limits its models to
    /// those the account enables.
    pub fn available(&self) -> Vec<&Model> {
        let mut configured: HashMap<&str, bool> = HashMap::new();
        let copilot_ids = match self.credential("github-copilot") {
            Some(Credential::OAuth(credential)) => copilot::available_model_ids(&credential),
            _ => None,
        };
        self.models
            .iter()
            .filter(|model| {
                *configured
                    .entry(model.provider.as_str())
                    .or_insert_with(|| self.has_auth(&model.provider))
            })
            .filter(|model| {
                model.provider != "github-copilot"
                    || copilot_ids
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&model.id))
            })
            .collect()
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
        let mut env: Option<ProviderEnv> = None;
        match self.credential(provider) {
            Some(Credential::ApiKey(credential)) => {
                env = credential.env.clone();
                if let Some(key) = &credential.key
                    && let Some(value) = credentials::resolve(key, env.as_ref(), true).await
                {
                    auth.api_key = Some(value);
                    auth.source = Some("stored credential".into());
                    return auth;
                }
            }
            Some(Credential::OAuth(credential)) => {
                match self.oauth_auth(provider, credential, min_validity_ms).await {
                    Ok(Some(oauth)) => {
                        auth.api_key = Some(oauth.api_key);
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
            // ri has no sign-in for this provider: use the token as stored.
            return Ok(Some(OAuthAuth {
                api_key: stored.access,
                base_url: None,
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

    /// The display name of a provider: the built-in name, else its id.
    pub fn provider_name(&self, provider: &str) -> String {
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
    }
}
