//! llama.cpp: a local `llama-server` in router mode as a provider. Its
//! loaded models are chat models over the OpenAI-compatible API and
//! classifiers over `llama-cpp-classify`.
//!
//! Port of `extensions/llama/client.ts` and `provider.ts` in pi-coding-agent
//! `v1.0.0`, where llama.cpp is the built-in extension `builtin:llama.cpp`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use yapi_types::model::{ClassifierModel, Model};

use crate::model_catalog::ProviderModels;

/// The provider id.
pub const PROVIDER_ID: &str = "llama.cpp";
/// Where the server runs unless configured.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";
/// The credential setting with the server URL.
pub const BASE_URL_ENV: &str = "LLAMA_BASE_URL";
/// pi gives each server request 15 seconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A model's state on the server.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Status {
    /// `unloaded`, `loading`, `loaded`, `downloading` or `sleeping`.
    pub value: String,
    /// The server's arguments for the model.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Whether its last load failed.
    #[serde(default)]
    pub failed: Option<bool>,
    /// The exit code of a failed load.
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// Download progress by file: `{done, total}`.
    #[serde(default)]
    pub progress: Option<Value>,
}

/// What the model takes and produces.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Architecture {
    /// Input modalities, such as `image`.
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
}

/// Facts about the model's weights and runtime.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Meta {
    /// The context size it runs with.
    #[serde(default)]
    pub n_ctx: Option<u64>,
    /// The context size it was trained with.
    #[serde(default)]
    pub n_ctx_train: Option<u64>,
}

/// One entry of the server's `/models`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// The model id requests name.
    pub id: String,
    /// Its state.
    pub status: Status,
    /// Its modalities.
    #[serde(default)]
    pub architecture: Option<Architecture>,
    /// Where it comes from, such as `preset`.
    #[serde(default)]
    pub source: Option<String>,
    /// Its weights and runtime.
    #[serde(default)]
    pub meta: Option<Meta>,
}

impl ModelInfo {
    /// Loaded, or asleep and woken by the next request.
    pub fn is_loaded(&self) -> bool {
        matches!(self.status.value.as_str(), "loaded" | "sleeping")
    }
}

/// What `/props` reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props {
    /// Whether the router loads unloaded presets on first use.
    pub models_autoload: Option<bool>,
    /// The model's chat template.
    pub chat_template: Option<String>,
}

/// pi's `normalizeLlamaServerUrl`: an http(s) URL without query, fragment,
/// trailing slash or `/v1`.
pub fn normalize_server_url(value: &str) -> Result<String, String> {
    let mut url = url::Url::parse(value.trim()).map_err(|_| "Invalid URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Server URL must use http or https".into());
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path).to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

/// The OpenAI-compatible base URL of a server.
pub fn inference_url(server_url: &str) -> String {
    let normalized = normalize_server_url(server_url)
        .unwrap_or_else(|_| server_url.trim_end_matches('/').to_owned());
    format!("{normalized}/v1")
}

/// A client of one server.
#[derive(Clone, Debug)]
pub struct Client {
    /// The normalized server URL.
    pub server_url: String,
    api_key: Option<String>,
}

fn error_message(payload: &Value, fallback: String) -> String {
    payload["error"]["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .map_or(fallback, str::to_owned)
}

impl Client {
    /// A client of `server_url`, sending `api_key` when there is one.
    pub fn new(server_url: &str, api_key: Option<String>) -> Result<Client, String> {
        Ok(Client {
            server_url: normalize_server_url(server_url)?,
            api_key: api_key.filter(|key| !key.is_empty()),
        })
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        let mut request = crate::http::client()
            .request(method, format!("{}{path}", self.server_url))
            .timeout(REQUEST_TIMEOUT);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(yapi_types::json::to_string(&body).unwrap_or_default());
        }
        if let Some(key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let response = tokio::select! {
            () = cancel.cancelled() => return Err("This operation was aborted".into()),
            response = request.send() => response.map_err(|err| {
                if err.is_timeout() {
                    "The operation was aborted due to timeout".to_owned()
                } else {
                    "fetch failed".to_owned()
                }
            })?,
        };
        let status = response.status();
        let payload: Value = response
            .bytes()
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(error_message(
                &payload,
                format!("llama.cpp returned HTTP {}", status.as_u16()),
            ));
        }
        Ok(payload)
    }

    /// The server's models; `reload` rescans its model directories.
    pub async fn list(
        &self,
        reload: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<ModelInfo>, String> {
        let path = if reload {
            "/models?reload=1"
        } else {
            "/models"
        };
        let payload = self
            .request(reqwest::Method::GET, path, None, cancel)
            .await?;
        let Some(data) = payload["data"].as_array() else {
            return Err("llama.cpp returned an invalid model catalog".into());
        };
        let valid = |entry: &Value| entry["id"].is_string() && entry["status"]["value"].is_string();
        if !data.iter().all(valid) {
            return Err("Server is not running in llama.cpp router mode".into());
        }
        Ok(data
            .iter()
            .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect())
    }

    /// `/props`, for `model` without loading it, or for the server.
    pub async fn props(
        &self,
        model: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Props, String> {
        let path = match model {
            Some(model) => format!(
                "/props?{}",
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("model", model)
                    .append_pair("autoload", "false")
                    .finish()
            ),
            None => "/props".into(),
        };
        let payload = self
            .request(reqwest::Method::GET, &path, None, cancel)
            .await?;
        Ok(Props {
            models_autoload: payload["models_autoload"].as_bool(),
            chat_template: payload["chat_template"].as_str().map(str::to_owned),
        })
    }

    /// POSTs `{"model": model}` to `path`.
    async fn post_model(
        &self,
        path: &str,
        model: &str,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let body = Some(json!({ "model": model }));
        self.request(reqwest::Method::POST, path, body, cancel)
            .await
            .map(drop)
    }

    /// Asks the router to load `model`.
    pub async fn load(&self, model: &str, cancel: &CancellationToken) -> Result<(), String> {
        self.post_model("/models/load", model, cancel).await
    }

    /// Asks the router to unload `model`, or to stop loading or downloading it.
    pub async fn unload(&self, model: &str, cancel: &CancellationToken) -> Result<(), String> {
        self.post_model("/models/unload", model, cancel).await
    }

    /// Unloads `model` and waits until it is unloaded.
    pub async fn unload_and_wait(
        &self,
        model: &str,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        self.unload(model, cancel).await?;
        loop {
            let models = self.list(false, cancel).await?;
            match models.iter().find(|entry| entry.id == model) {
                Some(entry) if entry.status.value != "unloaded" => {}
                _ => return Ok(()),
            }
            sleep(Duration::from_millis(100), cancel).await?;
        }
    }

    /// Asks the router to download `model`, a Hugging Face `repo[:quant]`.
    pub async fn download(&self, model: &str, cancel: &CancellationToken) -> Result<(), String> {
        self.post_model("/models", model, cancel).await
    }

    /// Loads `model` and waits until it is loaded, reporting progress.
    pub async fn load_and_wait(
        &self,
        model: &str,
        progress: &(dyn Fn(Progress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<ModelInfo, String> {
        self.load(model, cancel).await?;
        progress(Progress::message("Loading model"));
        loop {
            let models = self.list(false, cancel).await?;
            match models.into_iter().find(|entry| entry.id == model) {
                Some(entry) if entry.status.value == "loaded" => return Ok(entry),
                Some(entry) if entry.status.failed == Some(true) => {
                    return Err(match entry.status.exit_code {
                        Some(code) => format!("Model exited with code {code}"),
                        None => "Model failed to load".into(),
                    });
                }
                _ => {}
            }
            sleep(Duration::from_millis(250), cancel).await?;
        }
    }

    /// Downloads `model` and waits until the router lists it; the catalog
    /// after a rescan.
    pub async fn download_and_wait(
        &self,
        model: &str,
        progress: &(dyn Fn(Progress) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<Vec<ModelInfo>, String> {
        self.download(model, cancel).await?;
        progress(Progress::message("Downloading model"));
        let mut downloading = false;
        let mut polls = 0;
        loop {
            let models = self.list(false, cancel).await?;
            polls += 1;
            match models.iter().find(|entry| entry.id == model) {
                Some(entry) if entry.status.value == "downloading" => {
                    downloading = true;
                    if let Some(report) = entry.status.progress.as_ref().and_then(download_progress)
                    {
                        progress(report);
                    }
                }
                Some(_) if downloading || polls >= 2 => return self.list(true, cancel).await,
                _ => {}
            }
            sleep(Duration::from_millis(500), cancel).await?;
        }
    }
}

async fn sleep(duration: Duration, cancel: &CancellationToken) -> Result<(), String> {
    tokio::select! {
        () = cancel.cancelled() => Err("Cancelled".into()),
        () = tokio::time::sleep(duration) => Ok(()),
    }
}

/// What a long operation reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// What happens now.
    pub message: String,
    /// How far along, from 0 to 1, when known.
    pub ratio: Option<f64>,
    /// Detail such as bytes downloaded.
    pub detail: Option<String>,
}

impl Progress {
    fn message(message: &str) -> Progress {
        Progress {
            message: message.into(),
            ratio: None,
            detail: None,
        }
    }
}

/// pi's `formatBytes`.
pub fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes / 1024.0;
    let mut unit = units[0];
    for next in &units[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    let digits = if value >= 10.0 { 1 } else { 2 };
    format!("{} {unit}", yapi_types::js::to_fixed(value, digits))
}

/// pi's `parseDownloadProgress` over a model's per-file `{done, total}`.
fn download_progress(data: &Value) -> Option<Progress> {
    let files = data
        .get("progress")
        .filter(|nested| nested.is_object())
        .unwrap_or(data);
    let (mut done, mut total) = (0.0, 0.0);
    for entry in files.as_object()?.values() {
        if let (Some(d), Some(t)) = (entry["done"].as_f64(), entry["total"].as_f64()) {
            done += d;
            total += t;
        }
    }
    (total > 0.0).then(|| Progress {
        message: "Downloading model".into(),
        ratio: Some(done / total),
        detail: Some(format!("{} / {}", format_bytes(done), format_bytes(total))),
    })
}

/// Whether a model can serve requests: loaded or asleep, or an unloaded
/// preset the router loads on first use.
pub fn is_selectable(model: &ModelInfo, router_autoload: bool) -> bool {
    match model.status.value.as_str() {
        "loaded" | "sleeping" => true,
        "unloaded" => {
            router_autoload
                && model.status.failed != Some(true)
                && model.source.as_deref() == Some("preset")
        }
        _ => false,
    }
}

/// The `--ctx-size`, `-c` or `-ctx` argument the server runs the model with.
fn configured_context(model: &ModelInfo) -> Option<u64> {
    let args = model.status.args.as_deref().unwrap_or_default();
    args.windows(2).find_map(|pair| {
        matches!(pair[0].as_str(), "--ctx-size" | "-c" | "-ctx")
            .then(|| pair[1].parse::<u64>().ok().filter(|value| *value > 0))
            .flatten()
    })
}

/// pi's `contextWindowOf`: the runtime context, the configured one, the
/// last known one, the training context, or 128000.
pub fn context_window(model: &ModelInfo, cached: Option<u64>) -> u64 {
    let meta = model.meta.clone().unwrap_or_default();
    meta.n_ctx
        .filter(|value| *value > 0)
        .or_else(|| configured_context(model))
        .or(cached.filter(|value| *value > 0))
        .or(meta.n_ctx_train.filter(|value| *value > 0))
        .unwrap_or(128_000)
}

const ZERO_COST: &str = r#"{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}"#;

/// pi's `toPiModel`: the chat model of a server model.
pub fn chat_model(
    model: &ModelInfo,
    server_url: &str,
    props: Option<&Props>,
    cached: Option<u64>,
) -> Option<Model> {
    let window = context_window(model, cached);
    let reasoning = props
        .and_then(|props| props.chat_template.as_deref())
        .is_some_and(|template| template.contains("enable_thinking"));
    let image = model
        .architecture
        .as_ref()
        .and_then(|architecture| architecture.input_modalities.as_ref())
        .is_some_and(|modalities| modalities.iter().any(|kind| kind == "image"));
    let mut value = json!({
        "id": model.id,
        "name": model.id,
        "api": "openai-completions",
        "provider": PROVIDER_ID,
        "baseUrl": inference_url(server_url),
        "reasoning": reasoning,
    });
    if reasoning {
        value["thinkingLevelMap"] = json!({
            "off": "off", "minimal": null, "low": null, "medium": "medium", "high": null, "xhigh": null
        });
    }
    value["input"] = if image {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    value["cost"] = serde_json::from_str(ZERO_COST).unwrap_or(Value::Null);
    value["contextWindow"] = json!(window);
    value["maxTokens"] = json!(window);
    let mut compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "supportsUsageInStreaming": true,
        "supportsStrictMode": false,
        "maxTokensField": "max_tokens",
    });
    if reasoning {
        compat["thinkingFormat"] = json!("qwen-chat-template");
    }
    value["compat"] = compat;
    serde_json::from_value(value).ok()
}

/// pi's `toPiClassifierModel`: the same model as a classifier.
pub fn classifier_model(
    model: &ModelInfo,
    server_url: &str,
    cached: Option<u64>,
) -> Option<ClassifierModel> {
    serde_json::from_value(json!({
        "type": "classifier",
        "id": model.id,
        "name": model.id,
        "api": "llama-cpp-classify",
        "provider": PROVIDER_ID,
        "baseUrl": server_url,
        "input": ["text"],
        "cost": serde_json::from_str::<Value>(ZERO_COST).unwrap_or(Value::Null),
        "contextWindow": context_window(model, cached),
    }))
    .ok()
}

/// The models a server serves now: loaded ones with their templates, then
/// the same models as classifiers. `cached` gives earlier context windows.
pub async fn fetch_models(
    client: &Client,
    cached: &Map<String, Value>,
    cancel: &CancellationToken,
) -> Result<ProviderModels, String> {
    let catalog = client.list(false, cancel).await?;
    let autoload = catalog
        .iter()
        .any(|model| model.status.value == "unloaded" && model.source.as_deref() == Some("preset"))
        && client
            .props(None, cancel)
            .await
            .ok()
            .and_then(|props| props.models_autoload)
            == Some(true);
    let selectable: Vec<&ModelInfo> = catalog
        .iter()
        .filter(|model| is_selectable(model, autoload))
        .collect();
    let window = |id: &str| cached.get(id).and_then(Value::as_u64);
    let mut models = ProviderModels::default();
    for model in &selectable {
        // Only loaded models give their template without side effects.
        let props = if model.status.value == "loaded" {
            Some(client.props(Some(&model.id), cancel).await?)
        } else {
            None
        };
        models.chat.extend(chat_model(
            model,
            &client.server_url,
            props.as_ref(),
            window(&model.id),
        ));
    }
    for model in &selectable {
        models.classifiers.extend(classifier_model(
            model,
            &client.server_url,
            window(&model.id),
        ));
    }
    Ok(models)
}

/// Hugging Face, where the router downloads models from.
pub mod huggingface {
    use std::path::PathBuf;

    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    const DEFAULT_URL: &str = "https://huggingface.co";

    /// A search result.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Model {
        /// `owner/repository`.
        pub id: String,
        /// Downloads in the last month.
        pub downloads: u64,
    }

    /// A GGUF quantization of a repository.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Quantization {
        /// Such as `Q4_K_M`.
        pub name: String,
        /// Bytes over all shards, when every shard reports its size.
        pub size: Option<u64>,
    }

    /// A repository's access and quantizations.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Details {
        /// The repository id.
        pub id: String,
        /// `auto` or `manual` approval, when gated.
        pub gated: Option<String>,
        /// Quantizations, Q4_K_M first, then by size.
        pub quantizations: Vec<Quantization>,
    }

    /// pi's `findHuggingFaceToken`: `HF_TOKEN`, then the token files of the
    /// Hugging Face CLI. `env` reads the environment.
    pub fn find_token(
        env: &dyn Fn(&str) -> Option<String>,
        home: Option<PathBuf>,
    ) -> Option<String> {
        if let Some(token) = env("HF_TOKEN")
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
        {
            return Some(token);
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        paths.extend(env("HF_TOKEN_PATH").map(PathBuf::from));
        paths.extend(env("HF_HOME").map(|home| PathBuf::from(home).join("token")));
        paths.extend(
            env("XDG_CACHE_HOME")
                .map(|cache| PathBuf::from(cache).join("huggingface").join("token")),
        );
        paths.extend(home.map(|home| home.join(".cache").join("huggingface").join("token")));
        let mut seen = Vec::new();
        for path in paths {
            if seen.contains(&path) {
                continue;
            }
            if let Ok(token) = std::fs::read_to_string(&path) {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_owned());
                }
            }
            seen.push(path);
        }
        None
    }

    /// A Hugging Face API client.
    #[derive(Clone, Debug)]
    pub struct Client {
        token: Option<String>,
        base_url: String,
    }

    impl Client {
        /// A client of `base_url`, or huggingface.co, sending `token`.
        pub fn new(token: Option<String>, base_url: Option<&str>) -> Client {
            Client {
                token,
                base_url: base_url
                    .unwrap_or(DEFAULT_URL)
                    .trim_end_matches('/')
                    .to_owned(),
            }
        }

        async fn request(&self, path: &str, cancel: &CancellationToken) -> Result<Value, String> {
            let mut request = crate::http::client()
                .get(format!("{}{path}", self.base_url))
                .timeout(super::REQUEST_TIMEOUT);
            if let Some(token) = &self.token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = tokio::select! {
                () = cancel.cancelled() => return Err("This operation was aborted".into()),
                response = request.send() => response.map_err(|_| "fetch failed".to_owned())?,
            };
            let status = response.status().as_u16();
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let (retry_after, rate_limit) = (header("retry-after"), header("ratelimit"));
            let payload: Value = response
                .bytes()
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null);
            if (200..300).contains(&status) {
                return Ok(payload);
            }
            if status == 429 {
                let delay = retry_after
                    .and_then(|value| value.trim().parse::<f64>().ok())
                    .filter(|delay| *delay != 0.0)
                    .map(|delay| delay.to_string())
                    .or_else(|| {
                        rate_limit.and_then(|value| {
                            value
                                .split(';')
                                .find_map(|part| part.trim().strip_prefix("t="))
                                .filter(|digits| {
                                    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                                })
                                .map(str::to_owned)
                        })
                    });
                return Err(match delay {
                    Some(delay) => format!("Hugging Face rate limit reached; retry in {delay}s"),
                    None => "Hugging Face rate limit reached".into(),
                });
            }
            Err(payload["error"]
                .as_str()
                .filter(|error| !error.is_empty())
                .map_or_else(
                    || format!("Hugging Face returned HTTP {status}"),
                    str::to_owned,
                ))
        }

        /// GGUF repositories matching `query`, most downloaded first.
        pub async fn search(
            &self,
            query: &str,
            cancel: &CancellationToken,
        ) -> Result<Vec<Model>, String> {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("search", query)
                .append_pair("filter", "gguf")
                .append_pair("sort", "downloads")
                .append_pair("direction", "-1")
                .append_pair("limit", "20")
                .finish();
            let payload = self
                .request(&format!("/api/models?{query}"), cancel)
                .await?;
            let Some(entries) = payload.as_array() else {
                return Err("Hugging Face returned invalid search results".into());
            };
            Ok(entries
                .iter()
                .filter_map(|entry| {
                    Some(Model {
                        id: entry["id"].as_str()?.to_owned(),
                        downloads: entry["downloads"].as_f64().map_or(0, |count| count as u64),
                    })
                })
                .collect())
        }

        /// A repository's access and quantizations.
        pub async fn details(
            &self,
            id: &str,
            cancel: &CancellationToken,
        ) -> Result<Details, String> {
            let encoded: Vec<String> = id
                .split('/')
                .map(|part| url::form_urlencoded::byte_serialize(part.as_bytes()).collect())
                .collect();
            let payload = self
                .request(
                    &format!("/api/models/{}?blobs=true", encoded.join("/")),
                    cancel,
                )
                .await?;
            if !payload.is_object() {
                return Err("Hugging Face returned invalid model details".into());
            }
            let mut sizes: Vec<(String, u64, bool)> = Vec::new();
            for file in payload["siblings"].as_array().into_iter().flatten() {
                let Some(path) = file["rfilename"].as_str() else {
                    continue;
                };
                if !path.to_lowercase().ends_with(".gguf") {
                    continue;
                }
                let name = path.rsplit('/').next().unwrap_or(path);
                if name.to_lowercase().starts_with("mmproj") {
                    continue;
                }
                let Some(quantization) = quantization(&name[..name.len() - 5]) else {
                    continue;
                };
                let index = match sizes.iter().position(|(name, _, _)| *name == quantization) {
                    Some(index) => index,
                    None => {
                        sizes.push((quantization, 0, true));
                        sizes.len() - 1
                    }
                };
                match file["size"].as_f64() {
                    Some(size) => sizes[index].1 += size as u64,
                    None => sizes[index].2 = false,
                }
            }
            let mut quantizations: Vec<Quantization> = sizes
                .into_iter()
                .map(|(name, total, complete)| Quantization {
                    name,
                    size: complete.then_some(total),
                })
                .collect();
            quantizations.sort_by(|left, right| {
                if left.name == "Q4_K_M" {
                    return std::cmp::Ordering::Less;
                }
                if right.name == "Q4_K_M" {
                    return std::cmp::Ordering::Greater;
                }
                left.size
                    .unwrap_or(u64::MAX)
                    .cmp(&right.size.unwrap_or(u64::MAX))
                    .then_with(|| yapi_types::collate::locale_compare(&left.name, &right.name))
            });
            let gated = payload["gated"]
                .as_str()
                .filter(|gated| matches!(*gated, "auto" | "manual"))
                .map(str::to_owned);
            Ok(Details {
                id: payload["id"].as_str().unwrap_or(id).to_owned(),
                gated,
                quantizations,
            })
        }
    }

    /// The quantization a GGUF file stem names, upper-cased; pi's
    /// `QUANTIZATION_PATTERN` after the shard suffix is removed.
    pub fn quantization(stem: &str) -> Option<String> {
        let stem = match stem.rfind("-of-") {
            Some(of)
                if stem.len() == of + 9
                    && stem[of + 4..].chars().all(|c| c.is_ascii_digit())
                    && of >= 6
                    && stem.as_bytes()[of - 6] == b'-'
                    && stem[of - 5..of].chars().all(|c| c.is_ascii_digit()) =>
            {
                &stem[..of - 6]
            }
            _ => stem,
        };
        let pattern = regex_lite::Regex::new(
            r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d(?:_[A-Z0-9]+)+|Q\d(?:_[A-Z0-9]+)+|BF16|F16|F32|MXFP\d(?:_[A-Z0-9]+)*))$",
        )
        .ok()?;
        pattern
            .captures(stem)
            .and_then(|captures| captures.get(1))
            .map(|found| found.as_str().to_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_urls_like_pi() {
        assert_eq!(
            normalize_server_url(" http://127.0.0.1:8080/v1/?x=1#y ").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_server_url("https://host/llama/").unwrap(),
            "https://host/llama"
        );
        assert_eq!(
            normalize_server_url("ftp://host").unwrap_err(),
            "Server URL must use http or https"
        );
        assert_eq!(inference_url("http://h:1"), "http://h:1/v1");
        assert_eq!(format_bytes(1536.0), "1.50 KiB");
        assert_eq!(format_bytes(12.0 * 1024.0 * 1024.0), "12.0 MiB");
        assert_eq!(
            huggingface::quantization("Qwen3-8B-Q4_K_M-00001-of-00002").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            huggingface::quantization("model.ud-iq2_xxs").as_deref(),
            Some("UD-IQ2_XXS")
        );
        assert_eq!(huggingface::quantization("readme"), None);
    }

    #[test]
    fn builds_models_like_pi() {
        let info: ModelInfo = serde_json::from_value(json!({
            "id": "qwen", "status": {"value": "loaded", "args": ["-c", "8192"]},
            "architecture": {"input_modalities": ["text", "image"]}
        }))
        .unwrap();
        let props = Props {
            models_autoload: None,
            chat_template: Some("{% if enable_thinking %}".into()),
        };
        let model = chat_model(&info, "http://h:8080", Some(&props), None).unwrap();
        assert_eq!(model.context_window, 8192);
        assert!(model.reasoning);
        assert_eq!(model.base_url, "http://h:8080/v1");
        assert!(model.accepts_images());
        let classifier = classifier_model(&info, "http://h:8080", Some(4096)).unwrap();
        assert_eq!(classifier.api, "llama-cpp-classify");
        assert_eq!(classifier.base_url, "http://h:8080");
        let preset: ModelInfo = serde_json::from_value(json!({
            "id": "p", "status": {"value": "unloaded"}, "source": "preset"
        }))
        .unwrap();
        assert!(is_selectable(&preset, true));
        assert!(!is_selectable(&preset, false));
        assert_eq!(context_window(&preset, None), 128_000);
    }
}
