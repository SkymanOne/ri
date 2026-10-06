//! Requests and operations the runtime itself answers: modules, the process
//! environment, randomness, hashing, processes, HTTP, yapi's wire APIs and
//! JSON helpers. Grants
//! gate the ones that reach outside the instance; everything else goes to the
//! [`Bridge`](crate::Bridge).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sha2::Digest as _;

use tokio_util::sync::CancellationToken;
use yapi_ai::api::Apis;
use yapi_ai::stream::{EventStream, Request, StreamEvent};
use yapi_core::tools::{RegisteredTool, ToolEnv};
use yapi_types::message::Message;
use yapi_types::model::Model;
use yapi_types::sync::lock;

use crate::instance::Options;
use crate::loader::Loader;
use crate::{Bridge, ops};

/// What an instance's requests and operations reach.
pub(crate) struct Host {
    pub(crate) loader: Loader,
    pub(crate) bridge: Arc<dyn Bridge>,
    pub(crate) options: Options,
    pub(crate) ai_streams: AiStreams,
}

/// A running stream of one of yapi's wire APIs, with its cancellation. The
/// events are out of the map while an `ai.next` call reads them.
type AiStream = (Option<EventStream>, CancellationToken);

/// Streams of yapi's wire APIs that extensions read through pi-ai, by id.
#[derive(Clone, Default)]
pub(crate) struct AiStreams {
    running: Arc<Mutex<HashMap<u64, AiStream>>>,
    next_id: Arc<AtomicU64>,
}

impl AiStreams {
    /// Starts streaming `{api, model, context, options}` with yapi's
    /// implementation of `api`, or with the model's provider and API as
    /// pi-ai's `streamSimple` does without one, and returns the stream's id.
    /// A request without a key takes the provider's key variable when
    /// `env_keys` allows it.
    fn start(&self, payload: &Value, env_keys: bool) -> Result<Value, String> {
        let model: Model = serde_json::from_value(payload["model"].clone())
            .map_err(|err| format!("Invalid model: {err}"))?;
        let messages: Vec<Message> = serde_json::from_value(payload["context"]["messages"].clone())
            .map_err(|err| format!("Invalid context: {err}"))?;
        let cancel = CancellationToken::new();
        let mut options = crate::streams::options_from_json(&payload["options"], cancel.clone());
        if env_keys
            && options
                .api_key
                .as_deref()
                .is_none_or(|key| key.trim().is_empty())
        {
            let key = yapi_ai::credentials::env_api_key(&model.provider, options.env.as_ref())
                .map(|(_, key)| key)
                .filter(|key| key != yapi_ai::credentials::AMBIENT_CREDENTIALS);
            options.api_key = key.or(options.api_key);
        }
        let request = Request {
            model,
            messages,
            options,
        };
        let events = match payload["api"].as_str() {
            Some(api) => {
                let provider = yapi_ai::api::builtin(api)
                    .ok_or_else(|| format!("No API provider registered for api: {api}"))?;
                provider.stream(request)
            }
            None => Apis::default().stream(request),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.running).insert(id, (Some(events), cancel));
        Ok(json!(id))
    }

    /// The events of stream `id` that have arrived, waiting for at least
    /// one; none once the stream has ended.
    async fn next(&self, id: u64) -> Value {
        let Some(mut events) = lock(&self.running)
            .get_mut(&id)
            .and_then(|(events, _)| events.take())
        else {
            return json!([]);
        };
        let mut batch = Vec::new();
        let mut next = events.next().await;
        let mut ended = next.is_none();
        while let Some(event) = next {
            ended = matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_));
            batch.push(crate::streams::event_json(&event));
            if ended {
                break;
            }
            next = events.next().now_or_never().flatten();
        }
        let mut running = lock(&self.running);
        if ended {
            running.remove(&id);
        } else if let Some((slot, _)) = running.get_mut(&id) {
            *slot = Some(events);
        }
        Value::Array(batch)
    }

    /// Cancels stream `id`.
    fn abort(&self, id: u64) {
        if let Some((_, cancel)) = lock(&self.running).get(&id) {
            cancel.cancel();
        }
    }
}

fn denied(what: &str) -> String {
    format!("{what} is not permitted for this extension")
}

fn text<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().unwrap_or_default()
}

impl Host {
    /// Whether the extension reads the process environment, which provider
    /// key variables come from.
    fn reads_process_env(&self) -> bool {
        self.options.grants.environment && self.options.environment.is_none()
    }

    /// Answers request `kind` at once.
    pub(crate) fn request(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        match kind {
            "module.resolve" => self
                .loader
                .resolve(
                    text(payload, "specifier"),
                    text(payload, "referrer"),
                    payload["kind"] == "require",
                )
                .map(Value::String),
            "module.load" => self.loader.load(text(payload, "path")),
            "module.source" => self.loader.source(text(payload, "path")),
            "log" => {
                self.bridge
                    .log(text(payload, "level"), text(payload, "message"));
                Ok(Value::Null)
            }
            "cwd" => Ok(Value::String(
                self.options.cwd.to_string_lossy().into_owned(),
            )),
            "home" => Ok(Value::String(
                self.options.home_dir.to_string_lossy().into_owned(),
            )),
            "agentDir" => Ok(Value::String(
                self.options.agent_dir.to_string_lossy().into_owned(),
            )),
            "tmpdir" => Ok(Value::String(
                self.options.temp_dir.to_string_lossy().into_owned(),
            )),
            // pi-ai's built-in catalog, which needs no session.
            "models.providers" => Ok(json!(
                yapi_ai::catalog::builtin_providers().collect::<Vec<_>>()
            )),
            "models.list" => Ok(serde_json::to_value(yapi_ai::catalog::builtin_models(text(
                payload, "provider",
            )))
            .unwrap_or_default()),
            "models.builtin" => Ok(yapi_ai::catalog::builtin_models(text(payload, "provider"))
                .into_iter()
                .find(|model| model.id == text(payload, "id"))
                .and_then(|model| serde_json::to_value(model).ok())
                .unwrap_or_default()),
            "models.envApiKey" if self.reads_process_env() => Ok(
                yapi_ai::credentials::env_api_key(text(payload, "provider"), None)
                    .map_or(Value::Null, |(_, key)| Value::String(key)),
            ),
            "models.envApiKey" => Ok(Value::Null),
            "platform" => Ok(Value::String(platform().into())),
            "env" => Ok(Value::Object(
                if let (true, Some(environment)) =
                    (self.options.grants.environment, &self.options.environment)
                {
                    environment
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                        .collect()
                } else if self.options.grants.environment {
                    std::env::vars()
                        .chain(
                            yapi_core::config::child_env()
                                .into_iter()
                                .map(|(key, value)| (key.to_owned(), value)),
                        )
                        .map(|(key, value)| (key, Value::String(value)))
                        .collect()
                } else {
                    Map::new()
                },
            )),
            "random" => {
                let count = payload["bytes"].as_u64().unwrap_or(0).min(65536) as usize;
                let mut bytes = vec![0; count];
                getrandom::fill(&mut bytes).map_err(|err| err.to_string())?;
                Ok(Value::String(STANDARD.encode(bytes)))
            }
            "hash" => hash(text(payload, "algorithm"), text(payload, "data")),
            "exec.sync" if self.options.grants.process => ops::exec_sync(payload),
            "exec.sync" => Err(denied("Running processes")),
            "json.repair" => Ok(Value::String(yapi_ai::json_parse::repair_json(text(
                payload, "text",
            )))),
            "json.parseWithRepair" => {
                yapi_ai::json_parse::parse_json_with_repair(text(payload, "text"))
                    .map_err(|err| err.to_string())
            }
            "json.partial" => Ok(Value::Object(yapi_ai::json_parse::parse_streaming_json(
                text(payload, "text"),
            ))),
            "codemode.definition" => Ok(crate::codemode::definition()),
            "builtin.tool" => {
                // Declaring a tool needs no grant; running it does.
                let tool = self.builtin_tool(payload, false)?;
                Ok(json!({
                    "name": tool.name(),
                    "label": tool.tool.label(),
                    "description": tool.tool.declaration().description,
                    "parameters": tool.tool.declaration().parameters,
                    "promptSnippet": tool.snippet,
                    "promptGuidelines": tool.guidelines,
                }))
            }
            "ai.abort" => {
                self.ai_streams
                    .abort(payload["id"].as_u64().unwrap_or_default());
                Ok(Value::Null)
            }
            "frontmatter.parse" => {
                let (frontmatter, body) =
                    yapi_core::resources::parse_frontmatter(text(payload, "content"))?;
                Ok(json!({ "frontmatter": frontmatter, "body": body }))
            }
            _ => self.bridge.request(kind, payload),
        }
    }

    /// Starts operation `kind`.
    pub(crate) fn start(
        &self,
        kind: &str,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, String>> {
        match kind {
            "timer" => Box::pin(ops::timer(payload)),
            "exec" if self.options.grants.process => Box::pin(ops::exec(payload)),
            "exec" => Box::pin(async { Err(denied("Running processes")) }),
            "fetch" if self.options.grants.network => Box::pin(ops::fetch(payload)),
            "fetch" => Box::pin(async { Err(denied("Network access")) }),
            "dns.lookup" if self.options.grants.network => Box::pin(ops::dns_lookup(payload)),
            "dns.lookup" => Box::pin(async { Err(denied("Network access")) }),
            "ai.stream" if self.options.grants.network => {
                let streams = self.ai_streams.clone();
                let env_keys = self.reads_process_env();
                // The stream starts on the runtime.
                Box::pin(async move { streams.start(&payload, env_keys) })
            }
            "ai.stream" => Box::pin(async { Err(denied("Network access")) }),
            "ai.next" => {
                let streams = self.ai_streams.clone();
                let id = payload["id"].as_u64().unwrap_or_default();
                Box::pin(async move { Ok(streams.next(id).await) })
            }
            "builtin.execute" => {
                let tool = match self.builtin_tool(&payload, true) {
                    Ok(tool) => tool.tool,
                    Err(message) => return Box::pin(async move { Err(message) }),
                };
                let call_id = text(&payload, "toolCallId").to_owned();
                let params = payload["params"].clone();
                Box::pin(async move {
                    let result = tool
                        .execute(call_id, params, CancellationToken::new(), Arc::new(|_| {}))
                        .await?;
                    serde_json::to_value(result).map_err(|err| err.to_string())
                })
            }
            _ => self.bridge.start(kind, payload),
        }
    }

    /// Built-in tool `{name}` for `{cwd}`; with `run`, only when the grants
    /// allow running it.
    fn builtin_tool(&self, payload: &Value, run: bool) -> Result<RegisteredTool, String> {
        let name = text(payload, "name");
        let granted = if name == "bash" {
            self.options.grants.process
        } else {
            self.options.grants.filesystem
        };
        if run && !granted {
            return Err(denied(&format!("The {name} tool")));
        }
        let cwd = payload["cwd"]
            .as_str()
            .map_or_else(|| self.options.cwd.clone(), PathBuf::from);
        let env = ToolEnv {
            cwd,
            runtime: Arc::default(),
            bin_dir: yapi_core::config::bin_dir(&self.options.agent_dir),
        };
        yapi_core::tools::builtin(name, &env)
            .ok_or_else(|| format!("Unknown built-in tool: {name}"))
    }
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn hash(algorithm: &str, data: &str) -> Result<Value, String> {
    let data = STANDARD.decode(data).map_err(|err| err.to_string())?;
    let digest = match algorithm.to_ascii_lowercase().as_str() {
        "sha256" => sha2::Sha256::digest(&data).to_vec(),
        "sha512" => sha2::Sha512::digest(&data).to_vec(),
        "sha384" => sha2::Sha384::digest(&data).to_vec(),
        "sha224" => sha2::Sha224::digest(&data).to_vec(),
        "sha1" => sha1::Sha1::digest(&data).to_vec(),
        "md5" => md5::Md5::digest(&data).to_vec(),
        other => return Err(format!("Digest method not supported: {other}")),
    };
    Ok(Value::String(STANDARD.encode(digest)))
}
