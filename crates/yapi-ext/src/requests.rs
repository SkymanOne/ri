//! Requests and operations the runtime itself answers: modules, the process
//! environment, randomness, hashing, processes, HTTP and JSON helpers. Grants
//! gate the ones that reach outside the instance; everything else goes to the
//! [`Bridge`](crate::Bridge).

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sha2::Digest as _;

use tokio_util::sync::CancellationToken;
use yapi_core::tools::{RegisteredTool, ToolEnv};

use crate::loader::Loader;
use crate::{Bridge, Grants, ops};

/// What an instance's requests and operations reach.
pub(crate) struct Host {
    pub(crate) loader: Loader,
    pub(crate) bridge: Arc<dyn Bridge>,
    pub(crate) grants: Grants,
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) temp_dir: PathBuf,
    pub(crate) environment: Option<std::collections::BTreeMap<String, String>>,
}

fn denied(what: &str) -> String {
    format!("{what} is not permitted for this extension")
}

fn text<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().unwrap_or_default()
}

impl Host {
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
            "cwd" => Ok(Value::String(self.cwd.to_string_lossy().into_owned())),
            "home" => Ok(Value::String(self.home_dir.to_string_lossy().into_owned())),
            "agentDir" => Ok(Value::String(self.agent_dir.to_string_lossy().into_owned())),
            "tmpdir" => Ok(Value::String(self.temp_dir.to_string_lossy().into_owned())),
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
            "models.envApiKey" if self.grants.environment && self.environment.is_none() => Ok(
                yapi_ai::credentials::env_api_key(text(payload, "provider"), None)
                    .map_or(Value::Null, |(_, key)| Value::String(key)),
            ),
            "models.envApiKey" => Ok(Value::Null),
            "platform" => Ok(Value::String(platform().into())),
            "env" => Ok(Value::Object(
                if let (true, Some(environment)) = (self.grants.environment, &self.environment) {
                    environment
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                        .collect()
                } else if self.grants.environment {
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
            "exec.sync" if self.grants.process => ops::exec_sync(payload),
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
            "exec" if self.grants.process => Box::pin(ops::exec(payload)),
            "exec" => Box::pin(async { Err(denied("Running processes")) }),
            "fetch" if self.grants.network => Box::pin(ops::fetch(payload)),
            "fetch" => Box::pin(async { Err(denied("Network access")) }),
            "dns.lookup" if self.grants.network => Box::pin(ops::dns_lookup(payload)),
            "dns.lookup" => Box::pin(async { Err(denied("Network access")) }),
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
}

impl Host {
    /// Built-in tool `{name}` for `{cwd}`, if the grants cover what it does.
    /// Built-in tool `name`; with `run`, only when the grants allow running it.
    fn builtin_tool(&self, payload: &Value, run: bool) -> Result<RegisteredTool, String> {
        let name = text(payload, "name");
        let granted = if name == "bash" {
            self.grants.process
        } else {
            self.grants.filesystem
        };
        if run && !granted {
            return Err(denied(&format!("The {name} tool")));
        }
        let cwd = payload["cwd"]
            .as_str()
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        let env = ToolEnv {
            cwd,
            runtime: Arc::default(),
            bin_dir: yapi_core::config::bin_dir(&self.agent_dir),
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
