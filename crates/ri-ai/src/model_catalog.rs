//! Model catalogs that change after a release: pi.dev's catalog overlay for
//! built-in providers, and the catalogs of Radius gateways. Refreshed
//! catalogs persist in `models-store.json`, so later startups have them
//! offline.
//!
//! Ports of `remote-catalog-provider.ts` and `models-store.ts` in
//! `pi-coding-agent`, and of the Radius provider's `refreshModels` in pi-ai
//! `v1.0.0`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use ri_types::model::{ClassifierModel, ImageModel, Model};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::auth::lock::FileLock;
use crate::auth::now_ms;

/// pi.dev, where pi publishes catalog updates between releases.
pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";
/// A catalog checked this recently is not fetched again unless forced.
pub const REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;
/// Model types this client asks the catalog for.
const MODEL_TYPES: &str = "chat,image,classifier";
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
const RETRYABLE: [u16; 6] = [408, 429, 500, 502, 503, 504];

/// pi's `FileModelsStore`: catalogs by provider id in `models-store.json`.
#[derive(Clone, Debug)]
pub struct ModelsStore {
    path: PathBuf,
}

impl ModelsStore {
    /// The store at `path`.
    pub fn new(path: impl Into<PathBuf>) -> ModelsStore {
        ModelsStore { path: path.into() }
    }

    /// The store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every stored entry; an unreadable file reads as empty.
    pub fn read_all(&self) -> Map<String, Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| {
                serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')).ok()
            })
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }

    /// One provider's entry.
    pub fn read(&self, provider: &str) -> Option<Value> {
        self.read_all().remove(provider)
    }

    /// Replaces `provider`'s entry under the file lock, as pi writes it.
    pub async fn write(
        &self,
        provider: &str,
        entry: Value,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        }
        if !self.path.exists() {
            std::fs::write(&self.path, "").map_err(|err| err.to_string())?;
        }
        let _lock = FileLock::acquire(&self.path, cancel)
            .await
            .map_err(|err| err.to_string())?;
        let mut all = self.read_all();
        all.insert(provider.to_owned(), entry);
        let text = ri_types::json::to_string_pretty(&Value::Object(all), "  ")
            .map_err(|err| err.to_string())?;
        std::fs::write(&self.path, text).map_err(|err| err.to_string())
    }
}

/// How a provider's catalog refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// pi.dev's overlay on a built-in catalog.
    Remote,
    /// A Radius gateway's own catalog.
    Radius {
        /// The gateway origin.
        gateway: String,
    },
    /// The models a llama.cpp server serves.
    Llama {
        /// The server URL.
        server: String,
    },
}

/// A provider's models of every type.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderModels {
    /// Chat models.
    pub chat: Vec<Model>,
    /// Classifier models.
    pub classifiers: Vec<ClassifierModel>,
    /// Image-generation models.
    pub images: Vec<ImageModel>,
}

impl ProviderModels {
    /// Only chat models.
    pub fn chat(chat: Vec<Model>) -> ProviderModels {
        ProviderModels {
            chat,
            ..ProviderModels::default()
        }
    }

    /// Whether there are no models of any type.
    pub fn is_empty(&self) -> bool {
        self.chat.is_empty() && self.classifiers.is_empty() && self.images.is_empty()
    }
}

/// The models of a stored or fetched catalog, for `provider`, by type.
/// Models of unknown types, and invalid ones, are dropped.
fn typed_models(provider: &str, models: &Value) -> ProviderModels {
    let mut typed = ProviderModels::default();
    for model in models.as_array().into_iter().flatten() {
        if model["provider"].as_str().is_some_and(|p| p != provider) {
            continue;
        }
        let mut model = model.clone();
        model["provider"] = json!(provider);
        match model["type"].as_str().unwrap_or("chat") {
            "chat" => typed.chat.extend(serde_json::from_value(model).ok()),
            "classifier" => typed.classifiers.extend(serde_json::from_value(model).ok()),
            "image" => typed.images.extend(serde_json::from_value(model).ok()),
            _ => {}
        }
    }
    typed
}

/// The models a stored entry contributes: a Radius catalog as stored; pi.dev's
/// overlay only when it is newer than the built-in catalog.
pub fn restore(provider: &str, source: &Source, entry: &Value) -> ProviderModels {
    match source {
        Source::Radius { .. } | Source::Llama { .. } => typed_models(provider, &entry["models"]),
        Source::Remote => {
            let newer = entry["lastModified"]
                .as_u64()
                .is_some_and(|modified| modified > generated_at_ms());
            if newer {
                typed_models(provider, &entry["models"])
            } else {
                ProviderModels::default()
            }
        }
    }
}

/// When the built-in catalog was generated.
pub fn generated_at_ms() -> u64 {
    crate::auth::rfc3339_ms(crate::catalog::GENERATED_AT).unwrap_or(0)
}

/// What a refresh does.
#[derive(Clone, Debug)]
pub struct RefreshOptions {
    /// Only these providers; all refreshable ones when `None`.
    pub providers: Option<Vec<String>>,
    /// Whether to fetch; stored catalogs are restored either way.
    pub allow_network: bool,
    /// Fetch even when the stored catalog is fresh.
    pub force: bool,
    /// Where pi.dev's catalogs live.
    pub catalog_base_url: String,
    /// Stops the refresh.
    pub cancel: CancellationToken,
}

impl Default for RefreshOptions {
    fn default() -> RefreshOptions {
        RefreshOptions {
            providers: None,
            allow_network: true,
            force: false,
            catalog_base_url: DEFAULT_CATALOG_BASE_URL.to_owned(),
            cancel: CancellationToken::new(),
        }
    }
}

/// One provider to refresh, with what the refresh needs from the registry.
#[derive(Clone, Debug)]
pub struct Target {
    /// Provider id.
    pub provider: String,
    /// Where its catalog comes from.
    pub source: Source,
    /// Its credential for the gateway, when it has one: an OAuth access token
    /// or an API key. pi.dev's catalogs need none but refresh only for
    /// configured providers.
    pub token: Option<String>,
    /// Whether the provider is configured.
    pub configured: bool,
}

/// What a refresh found.
#[derive(Debug, Default)]
pub struct Refreshed {
    /// Models each provider's catalog contributes now.
    pub models: IndexMap<String, ProviderModels>,
    /// Providers whose refresh failed, with why, in target order.
    pub errors: Vec<(String, String)>,
    /// Whether the refresh was cancelled before it finished.
    pub aborted: bool,
}

async fn get(
    url: &str,
    headers: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<reqwest::Response, String> {
    let mut attempt = 0;
    loop {
        let mut request = crate::http::client().get(url).timeout(ATTEMPT_TIMEOUT);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        let result = tokio::select! {
            () = cancel.cancelled() => return Err(crate::http::ABORTED_BEFORE_RESPONSE.into()),
            result = request.send() => result,
        };
        match result {
            Ok(response) if RETRYABLE.contains(&response.status().as_u16()) && attempt < 2 => {}
            Ok(response) => return Ok(response),
            Err(_) if attempt < 2 => {}
            Err(err) => return Err(crate::auth::network_message(&err)),
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
    }
}

/// pi's `parseCatalog`: an array, `{models: [...]}`, or an object of models.
fn parse_catalog(provider: &str, value: Value) -> Result<Vec<Value>, String> {
    let entries: Vec<Value> = match value {
        Value::Array(items) => items,
        Value::Object(mut object) => match object.remove("models") {
            Some(Value::Array(items)) => items,
            Some(models) => {
                object.insert("models".into(), models);
                object.into_iter().map(|(_, value)| value).collect()
            }
            None => object.into_iter().map(|(_, value)| value).collect(),
        },
        _ => return Err(format!("Invalid model catalog for provider \"{provider}\"")),
    };
    Ok(entries
        .into_iter()
        .filter(|entry| entry.is_object() && entry.get("id").is_some())
        .filter(|entry| {
            entry["type"]
                .as_str()
                .is_none_or(|kind| matches!(kind, "chat" | "image" | "classifier"))
                && (entry["type"].is_null() || entry["type"].is_string())
        })
        .map(|mut entry| {
            entry["provider"] = json!(provider);
            entry
        })
        .collect())
}

fn http_date_ms(text: &str) -> Option<u64> {
    // RFC 7231 IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`.
    let mut parts = text.split_whitespace().skip(1);
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    crate::auth::rfc3339_ms(&format!("{year:04}-{month:02}-{day:02}T{time}Z"))
}

async fn refresh_remote(
    target: &Target,
    stored: Option<&Value>,
    store: &ModelsStore,
    options: &RefreshOptions,
) -> Result<Option<ProviderModels>, String> {
    let provider = &target.provider;
    let checked = stored.and_then(|entry| entry["checkedAt"].as_u64());
    let modified = stored.and_then(|entry| entry["lastModified"].as_u64());
    if !options.force
        && let (Some(checked), Some(_)) = (checked, modified)
        && now_ms().saturating_sub(checked) < REFRESH_INTERVAL_MS
    {
        return Ok(None);
    }
    // Revalidate only when a cached body backs the validator.
    let validator = stored
        .filter(|entry| {
            entry["models"]
                .as_array()
                .is_some_and(|models| !models.is_empty())
        })
        .and_then(|entry| entry["etag"].as_str().map(str::to_owned));
    let mut url = url::Url::parse(&options.catalog_base_url).map_err(|err| err.to_string())?;
    url.set_path(&format!(
        "/api/models/providers/{}",
        crate::aws::sigv4::escape(provider)
    ));
    url.query_pairs_mut().append_pair("types", MODEL_TYPES);
    let mut headers = vec![("accept", "application/json".to_owned())];
    if let Some(validator) = validator {
        headers.push(("if-none-match", validator));
    }
    let response = get(url.as_str(), &headers, &options.cancel).await?;
    let checked_at = now_ms();
    let status = response.status().as_u16();
    let mut entry = stored.cloned().unwrap_or_else(|| json!({ "models": [] }));
    if status == 304 && stored.is_some() {
        entry["checkedAt"] = json!(checked_at);
        store.write(provider, entry, &options.cancel).await?;
        return Ok(None);
    }
    if status == 404 || status == 501 {
        entry["checkedAt"] = json!(checked_at);
        entry["lastModified"] = json!(0);
        if let Some(object) = entry.as_object_mut() {
            object.remove("etag");
        }
        store.write(provider, entry, &options.cancel).await?;
        return Ok(Some(ProviderModels::default()));
    }
    if !(200..300).contains(&status) {
        entry["checkedAt"] = json!(checked_at);
        store.write(provider, entry, &options.cancel).await?;
        return Err(format!(
            "Model catalog request failed for {provider}: {status}"
        ));
    }
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|value| value.to_str().ok())
        .and_then(http_date_ms)
        .unwrap_or(0);
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.map_err(|err| err.to_string())?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|err| format!("Invalid model catalog for provider \"{provider}\": {err}"))?;
    let models = parse_catalog(provider, body)?;
    let mut entry = json!({
        "models": models,
        "checkedAt": checked_at,
        "lastModified": last_modified,
    });
    if let Some(etag) = etag {
        entry["etag"] = json!(etag);
    }
    store
        .write(provider, entry.clone(), &options.cancel)
        .await?;
    Ok(Some(restore(provider, &Source::Remote, &entry)))
}

/// pi's llama.cpp `refreshModels`: the server's loaded models, keeping the
/// context windows learned before for models that no longer report one.
async fn refresh_llama(
    target: &Target,
    server: &str,
    stored: Option<&Value>,
    store: &ModelsStore,
    options: &RefreshOptions,
) -> Result<Option<ProviderModels>, String> {
    let client = crate::llama::Client::new(server, target.token.clone())?;
    let cached: Map<String, Value> = stored
        .and_then(|entry| entry["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            Some((
                model["id"].as_str()?.to_owned(),
                model["contextWindow"].clone(),
            ))
        })
        .collect();
    let models = crate::llama::fetch_models(&client, &cached, &options.cancel).await?;
    if options.cancel.is_cancelled() {
        return Ok(None);
    }
    let mut list: Vec<Value> = models
        .chat
        .iter()
        .filter_map(|model| serde_json::to_value(model).ok())
        .collect();
    list.extend(
        models
            .classifiers
            .iter()
            .filter_map(|model| serde_json::to_value(model).ok()),
    );
    let entry = json!({ "models": list, "checkedAt": now_ms() });
    store
        .write(&target.provider, entry, &options.cancel)
        .await?;
    Ok(Some(models))
}

/// pi's `loadRadiusGatewayConfig` and `getRadiusModelsFromConfig`.
async fn refresh_radius(
    target: &Target,
    gateway: &str,
    store: &ModelsStore,
    options: &RefreshOptions,
) -> Result<Option<ProviderModels>, String> {
    let mut headers = vec![("accept", "application/json".to_owned())];
    if let Some(token) = &target.token {
        headers.push(("authorization", format!("Bearer {token}")));
    }
    let response = get(&format!("{gateway}/v1/config"), &headers, &options.cancel).await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        let trimmed = text.trim();
        let body = if trimmed.chars().count() > 512 {
            format!("{}…", trimmed.chars().take(512).collect::<String>())
        } else {
            trimmed.to_owned()
        };
        return Err(format!(
            "Could not load Radius config from {gateway}: {status}: {body}"
        ));
    }
    let config = crate::auth::json_body(response).await;
    let (Some(base_url), Some(models)) = (config["baseUrl"].as_str(), config["models"].as_array())
    else {
        return Err(format!("Invalid Radius config from {gateway}"));
    };
    let valid = |model: &&Value| {
        model["id"].is_string()
            && model["name"].is_string()
            && model["reasoning"].is_boolean()
            && model["input"].is_array()
            && model["cost"].is_object()
            && model["contextWindow"].is_number()
            && model["maxTokens"].is_number()
    };
    let models: Vec<Value> = models
        .iter()
        .filter(valid)
        .map(|model| {
            let mut model = model.clone();
            model["api"] = json!("pi-messages");
            model["provider"] = json!(target.provider);
            model["baseUrl"] = json!(base_url);
            model
        })
        .collect();
    let entry = json!({ "models": models, "checkedAt": now_ms() });
    store
        .write(&target.provider, entry.clone(), &options.cancel)
        .await?;
    Ok(Some(typed_models(&target.provider, &entry["models"])))
}

/// One provider's refresh: its models, if it has a catalog, and why fetching
/// failed, if it did.
async fn refresh_one(
    target: &Target,
    entry: Option<&Value>,
    store: &ModelsStore,
    options: &RefreshOptions,
) -> (Option<ProviderModels>, Option<String>) {
    let restored = entry.map(|entry| restore(&target.provider, &target.source, entry));
    if !options.allow_network || options.cancel.is_cancelled() || !target.configured {
        return (restored, None);
    }
    let result = match &target.source {
        Source::Remote => refresh_remote(target, entry, store, options).await,
        Source::Radius { gateway } => refresh_radius(target, gateway, store, options).await,
        Source::Llama { server } => refresh_llama(target, server, entry, store, options).await,
    };
    match result {
        Ok(Some(models)) => (Some(models), None),
        Ok(None) => (restored, None),
        Err(_) if options.cancel.is_cancelled() => (restored, None),
        Err(error) => (restored, Some(error)),
    }
}

/// Refreshes `targets` concurrently: restores each stored catalog, then
/// fetches the configured providers' catalogs when the network is allowed.
pub async fn refresh(
    targets: &[Target],
    store: &ModelsStore,
    options: &RefreshOptions,
) -> Refreshed {
    let selected = |provider: &str| {
        options
            .providers
            .as_ref()
            .is_none_or(|providers| providers.iter().any(|p| p == provider))
    };
    let stored = store.read_all();
    let chosen: Vec<&Target> = targets
        .iter()
        .filter(|target| selected(&target.provider))
        .collect();
    let results = futures_util::future::join_all(
        chosen
            .iter()
            .map(|target| refresh_one(target, stored.get(&target.provider), store, options)),
    )
    .await;
    let mut refreshed = Refreshed {
        aborted: options.cancel.is_cancelled(),
        ..Refreshed::default()
    };
    for (target, (models, error)) in chosen.into_iter().zip(results) {
        if let Some(models) = models {
            refreshed.models.insert(target.provider.clone(), models);
        }
        if let Some(error) = error {
            refreshed.errors.push((target.provider.clone(), error));
        }
    }
    refreshed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_catalog_shapes_and_dates() {
        let models = parse_catalog(
            "xai",
            json!({"models": [{"id": "a"}, {"id": "b", "type": "video"}, {"name": "no id"}]}),
        )
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["provider"], "xai");
        assert_eq!(
            parse_catalog("xai", json!({"a": {"id": "a"}}))
                .unwrap()
                .len(),
            1
        );
        assert!(parse_catalog("xai", json!(1)).is_err());
        assert_eq!(http_date_ms("Thu, 01 Jan 1970 00:00:10 GMT"), Some(10_000));
        assert!(generated_at_ms() > 0);
    }
}
