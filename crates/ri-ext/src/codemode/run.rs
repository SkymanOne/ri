//! Running one script: port of `extensions/codemode/execute.ts` in
//! pi-coding-agent and of the host side of `pi-codemode`'s sandbox, `v1.0.0`.
//!
//! Each script runs in a fresh `ri-js` instance without grants or file
//! access. The guest compiles the script next to pi's prelude in an empty
//! QuickJS context; nested tool calls and discovery globals are operations
//! this module answers, so the script reaches nothing else.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use ri_agent::UpdateSink;
use ri_core::agent_session::AgentSession;
use ri_core::extensions::ToolInfo;
use ri_core::extensions::tool_search;
use ri_core::tools::RegisteredTool;
use ri_types::event::ToolResult;
use ri_types::message::{ContentBlock, Usage};
use ri_types::session::FileEntry;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::declarations::{Declaration, identifier, sample};
use super::models;
use crate::{Bridge, Engine, Grants, Instance, Options};

/// The custom entry type of `store()` writes.
pub(super) const STORE_ENTRY_TYPE: &str = "codemode-store";
const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
const CHARS_PER_TOKEN: usize = 4;
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// pi's `DEFAULT_TOOL_SEARCH_LIMIT`.
const DEFAULT_SEARCH_LIMIT: usize = 8;
/// Above the script's 256 MiB heap, so QuickJS reports running out first.
const INSTANCE_MEMORY_LIMIT: usize = 384 * 1024 * 1024;
const SUPPORTED_FIELDS: &str = "`max_output_tokens` and `timeout_ms`";

/// The `// @options:` line of a script.
#[derive(Debug, Default, PartialEq)]
pub(super) struct SourceOptions {
    max_output_tokens: Option<u64>,
    timeout_ms: Option<u64>,
}

fn options_error(detail: &str) -> String {
    format!("@options must be a JSON object with supported fields {SUPPORTED_FIELDS}{detail}")
}

fn parse_options(directive: &str) -> Result<SourceOptions, String> {
    if directive.is_empty() {
        return Err(options_error(""));
    }
    let value: Value = serde_json::from_str(directive).map_err(|err| {
        format!("@options must be valid JSON with supported fields {SUPPORTED_FIELDS}: {err}")
    })?;
    let Value::Object(fields) = value else {
        return Err(options_error(""));
    };
    if let Some(key) = fields
        .keys()
        .find(|key| *key != "max_output_tokens" && *key != "timeout_ms")
    {
        return Err(format!(
            "@options only supports {SUPPORTED_FIELDS}; got `{key}`"
        ));
    }
    // A safe integer: whole, and at most 2^53 - 1.
    let safe = |value: &Value| {
        value
            .as_f64()
            .filter(|number| {
                number.fract() == 0.0 && *number >= 0.0 && *number <= 9_007_199_254_740_991.0
            })
            .map(|number| number as u64)
    };
    let mut options = SourceOptions::default();
    if let Some(value) = fields.get("max_output_tokens") {
        options.max_output_tokens = Some(
            safe(value)
                .ok_or("@options field `max_output_tokens` must be a non-negative safe integer")?,
        );
    }
    if let Some(value) = fields.get("timeout_ms") {
        options.timeout_ms = Some(
            safe(value)
                .filter(|ms| *ms > 0 && *ms <= MAX_TIMEOUT_MS)
                .ok_or_else(|| {
                    format!("@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}")
                })?,
        );
    }
    Ok(options)
}

/// Splits an optional first-line `// @options: {...}` from the script; pi's
/// `parseCodemodeSource`. The options line becomes an empty line, so line
/// numbers are unchanged.
pub(super) fn parse_source(input: &str) -> Result<(String, SourceOptions), String> {
    if input.trim().is_empty() {
        return Err("Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.".into());
    }
    let newline = input.find('\n');
    let first = newline.map_or(input, |index| &input[..index]);
    let first = first.strip_suffix('\r').unwrap_or(first).trim_start();
    let Some(directive) = first.strip_prefix("// @options:") else {
        return Ok((input.to_owned(), SourceOptions::default()));
    };
    let code = newline.map_or("", |index| &input[index..]);
    if code.trim().is_empty() {
        return Err(
            "The @options line must be followed by JavaScript source on subsequent lines".into(),
        );
    }
    Ok((code.to_owned(), parse_options(directive.trim())?))
}

/// `load()` values: the `codemode-store` entries of the branch, applied from
/// the root, as JSON texts; pi's `readCodemodeStore`.
fn read_store(session: Option<&AgentSession>) -> Map<String, Value> {
    let mut store = Map::new();
    let Some(session) = session else {
        return store;
    };
    session.with_session(|file| {
        for entry in file.branch_path(None) {
            let FileEntry::Custom(entry) = entry else {
                continue;
            };
            let Some(data) = entry
                .data
                .as_ref()
                .filter(|_| entry.custom_type == STORE_ENTRY_TYPE)
            else {
                continue;
            };
            let (Some(set), Some(deleted)) = (data["set"].as_object(), data["delete"].as_array())
            else {
                continue;
            };
            if !deleted.iter().all(Value::is_string) {
                continue;
            }
            for key in deleted.iter().filter_map(Value::as_str) {
                store.shift_remove(key);
            }
            for (key, value) in set {
                store.shift_remove(key);
                let text = ri_types::json::to_string(value).unwrap_or_default();
                store.insert(key.clone(), Value::String(text));
            }
        }
    });
    store
}

/// `text.slice(0, max - 3) + "..."` when longer than `max`.
fn truncate_text(text: &str, max: usize) -> String {
    if ri_types::js::len(text) > max {
        format!("{}...", ri_types::js::slice(text, 0, max - 3))
    } else {
        text.to_owned()
    }
}

fn text_of(result: &ToolResult) -> String {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    texts.join("\n")
}

/// A nested call as `details.calls` reports it.
#[derive(Clone, Debug)]
struct CallRecord {
    id: String,
    name: String,
    args: String,
    status: &'static str,
    duration_ms: Option<f64>,
    error: Option<String>,
    /// Cost in USD of a `models.*` call that reported usage.
    cost: Option<f64>,
}

impl CallRecord {
    fn json(&self) -> Value {
        let mut out =
            json!({"id": self.id, "name": self.name, "args": self.args, "status": self.status});
        if let Some(duration) = self.duration_ms {
            out["durationMs"] = json!(duration);
        }
        if let Some(error) = &self.error {
            out["error"] = json!(error);
        }
        if let Some(cost) = self.cost {
            out["cost"] = json!(cost);
        }
        out
    }
}

/// What the script produced so far.
#[derive(Default)]
struct Progress {
    output: Vec<Value>,
    calls: Vec<CallRecord>,
    finished: bool,
}

/// A tool the script may call.
struct Callable {
    registered: RegisteredTool,
    declaration: Declaration,
    sample: String,
}

/// Answers the script's operations.
struct ScriptBridge {
    session: Option<AgentSession>,
    call_id: String,
    tools: Vec<Callable>,
    progress: Mutex<Progress>,
    updates: UpdateSink,
    /// Cancelled when the script ends, cancelling the calls still running.
    calls: CancellationToken,
    /// The codemode reference, when scripts reach `models`.
    docs: Option<String>,
    /// Slots for `models.classify()` and `models.generateImages()` calls.
    model_slots: tokio::sync::Semaphore,
    /// `models.*` calls so far, numbering their rows.
    model_calls: AtomicU64,
    /// Usage of the script's `models.*` calls.
    model_usage: Mutex<Option<Usage>>,
    /// Images `models.generateImages()` returned.
    generated_images: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The successful result of an operation: `undefined` or a JSON value.
fn script_value(value: Option<&Value>) -> Value {
    json!({"json": value.map(|value| ri_types::json::to_string(value).unwrap_or_default())})
}

/// Whether `query` names namespace `namespace`; pi's `isNamespaceName`.
fn is_namespace_name(namespace: &str, query: &str) -> bool {
    let (id, query_id) = (identifier(namespace), identifier(query));
    let suffix = |name: &str| name.rfind("__").map(|index| name[index + 2..].to_owned());
    namespace == query
        || id == query_id
        || suffix(namespace).as_deref() == Some(query)
        || suffix(&id).as_deref() == Some(query_id.as_str())
}

impl ScriptBridge {
    fn publish(&self) {
        let calls: Vec<Value> = lock(&self.progress)
            .calls
            .iter()
            .map(CallRecord::json)
            .collect();
        (self.updates)(ToolResult {
            details: Some(json!({"calls": calls})),
            ..ToolResult::default()
        });
    }

    fn entry(&self, tool: &Callable) -> Value {
        json!({"name": identifier(tool.registered.name()), "description": tool.sample})
    }

    /// `searchTools()`, `describeTool()` and `describeNamespace()`.
    fn global(&self, name: &str, args: &Value) -> Result<Value, String> {
        let arg = |index: usize| args.get(index).unwrap_or(&Value::Null);
        match name {
            "searchTools" => {
                let query = arg(0)
                    .as_str()
                    .ok_or("searchTools() expects a query string")?;
                let options = arg(1);
                let limit = match options.get("limit") {
                    None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
                    Some(limit) => limit
                        .as_f64()
                        .filter(|limit| limit.fract() == 0.0 && *limit > 0.0)
                        .ok_or("searchTools() limit must be a positive integer")?
                        as usize,
                };
                let namespace = match options.get("namespace") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(namespace)) => Some(namespace.as_str()),
                    Some(_) => return Err("searchTools() namespace must be a string".into()),
                };
                let documents: Vec<(String, String)> = self
                    .tools
                    .iter()
                    .filter(|tool| match (namespace, &tool.registered.namespace) {
                        (Some(query), Some(own)) => {
                            query.is_empty() || is_namespace_name(&own.name, query)
                        }
                        (Some(query), None) => query.is_empty(),
                        (None, _) => true,
                    })
                    .map(|tool| {
                        let info = ToolInfo {
                            name: tool.registered.name().to_owned(),
                            description: tool.declaration.description.clone(),
                            parameters: tool.declaration.input.clone(),
                            exposure: tool.registered.exposure,
                            namespace: tool.registered.namespace.clone(),
                        };
                        (info.name.clone(), tool_search::document(&info))
                    })
                    .collect();
                let found: Vec<Value> = tool_search::rank(query, &documents, limit)
                    .into_iter()
                    .filter_map(|(name, _)| {
                        let tool = self
                            .tools
                            .iter()
                            .find(|tool| tool.registered.name() == name)?;
                        Some(self.entry(tool))
                    })
                    .collect();
                Ok(script_value(Some(&Value::Array(found))))
            }
            "describeTool" => {
                let name = arg(0)
                    .as_str()
                    .ok_or("describeTool() expects a tool name")?;
                let tool = self.tools.iter().find(|tool| {
                    tool.registered.name() == name || identifier(tool.registered.name()) == name
                });
                Ok(script_value(
                    tool.map(|tool| Value::String(tool.sample.clone())).as_ref(),
                ))
            }
            "describeNamespace" => {
                let name = arg(0)
                    .as_str()
                    .ok_or("describeNamespace() expects a namespace name")?;
                let mut found = None;
                let mut names = Vec::new();
                for tool in &self.tools {
                    let Some(namespace) = &tool.registered.namespace else {
                        continue;
                    };
                    if !is_namespace_name(&namespace.name, name) {
                        continue;
                    }
                    found.get_or_insert(namespace);
                    names.push(Value::String(identifier(tool.registered.name())));
                }
                let described = found.map(|namespace| {
                    let mut out = json!({"name": namespace.name});
                    if let Some(description) = namespace
                        .description
                        .as_ref()
                        .filter(|text| !text.is_empty())
                    {
                        out["description"] = json!(description);
                    }
                    if let Some(instructions) = namespace
                        .instructions
                        .as_ref()
                        .filter(|text| !text.is_empty())
                    {
                        out["instructions"] = json!(instructions);
                    }
                    out["tools"] = Value::Array(names);
                    out
                });
                Ok(script_value(described.as_ref()))
            }
            other => Err(format!("Unknown global \"{other}\"")),
        }
    }

    /// A nested tool call: recorded, run through the session, and resolved
    /// to what the script receives.
    async fn call(self: Arc<Self>, name: String, args: Option<String>) -> Result<Value, String> {
        let Some(tool) = self
            .tools
            .iter()
            .position(|tool| tool.registered.name() == name)
        else {
            return Err(format!("Unknown tool \"{name}\""));
        };
        let Some(session) = self.session.clone() else {
            return Err("Tool calls need a session".into());
        };
        let preview = args
            .as_deref()
            .map(|args| truncate_text(args, ARGS_PREVIEW_CHARS))
            .unwrap_or_default();
        let index = {
            let mut progress = lock(&self.progress);
            progress.calls.push(CallRecord {
                id: format!("{}/?", self.call_id),
                name: name.clone(),
                args: preview,
                status: "running",
                duration_ms: None,
                error: None,
                cost: None,
            });
            progress.calls.len() - 1
        };
        self.publish();
        let started = Instant::now();
        let cancel = self.calls.child_token();
        let args: Value = args
            .as_deref()
            .and_then(|args| serde_json::from_str(args).ok())
            .unwrap_or(Value::Null);
        let outcome = session
            .execute_tool(&self.call_id, &name, args, cancel.clone(), None)
            .await;
        let text = text_of(&outcome.result);
        {
            let mut progress = lock(&self.progress);
            let record = &mut progress.calls[index];
            record.id = outcome.call.id.clone();
            record.duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            if outcome.is_error {
                record.status = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "error"
                };
                let error = if text.is_empty() {
                    format!("Tool \"{name}\" failed")
                } else {
                    text.clone()
                };
                record.error = Some(truncate_text(&error, ERROR_PREVIEW_CHARS));
            } else {
                record.status = "ok";
            }
        }
        self.publish();
        let tool = &self.tools[tool];
        if tool.registered.tool.output_schema().is_some()
            && let Some(structured) = &outcome.result.structured_content
        {
            return Ok(script_value(Some(structured)));
        }
        if outcome.is_error {
            return Err(if text.is_empty() {
                format!("Tool \"{name}\" failed")
            } else {
                text
            });
        }
        Ok(script_value(Some(&Value::String(text))))
    }
}

impl ScriptBridge {
    /// `models.*`: pi's `createModelGlobals`.
    async fn model_global(self: Arc<Self>, name: String, args: Value) -> Result<Value, String> {
        let docs = self.docs.clone().ok_or("models is not available")?;
        let session = self.session.clone().ok_or("models needs a session")?;
        let registry = session.registry();
        let items = models::args(&args);
        let arg = |index: usize| items.get(index).unwrap_or(&Value::Null);
        match name.as_str() {
            "models.getModelsOfType" | "models.getAvailableOfType" => {
                let kind = models::model_type(arg(0))?;
                let provider = models::provider(arg(1))?;
                let available = name == "models.getAvailableOfType";
                let list = models::models_of_type(&registry, kind, provider, available);
                Ok(script_value(Some(&Value::Array(list))))
            }
            "models.getModelOfType" => {
                let (provider, id) = models::model_of_type_args(&items)?;
                let kind = models::model_type(arg(0))?;
                let found = models::model_of_type(&registry, kind, provider, id);
                Ok(script_value(found.as_ref()))
            }
            "models.classify" => {
                self.model_call(&registry, &name, "classifier", arg(0), arg(1), &docs)
                    .await
            }
            "models.generateImages" => {
                self.model_call(&registry, &name, "image", arg(0), arg(1), &docs)
                    .await
            }
            other => Err(format!("Unknown global \"{other}\"")),
        }
    }

    /// A classifier or image call: resolved by provider and id only, its
    /// context checked, and run as a nested call row that shows the model,
    /// never the prompt or image data.
    async fn model_call(
        &self,
        registry: &ri_ai::registry::ModelRegistry,
        name: &str,
        kind: &str,
        model: &Value,
        context: &Value,
        docs: &str,
    ) -> Result<Value, String> {
        let (provider, id) = models::resolve_model(registry, name, kind, model)?;
        if kind == "classifier" {
            models::check_classifier_context(context, docs)?;
        } else {
            models::check_images_context(context, docs)?;
        }
        let number = self.model_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let index = {
            let mut progress = lock(&self.progress);
            progress.calls.push(CallRecord {
                id: format!("{}/{name}/{number}", self.call_id),
                name: name.to_owned(),
                args: format!("{provider}/{id}"),
                status: "running",
                duration_ms: None,
                error: None,
                cost: None,
            });
            progress.calls.len() - 1
        };
        self.publish();
        let started = Instant::now();
        let result = {
            let _slot = self
                .model_slots
                .acquire()
                .await
                .map_err(|err| err.to_string())?;
            let cancel = self.calls.child_token();
            if kind == "classifier" {
                let model = registry
                    .classifiers()
                    .iter()
                    .find(|model| model.provider == provider && model.id == id)
                    .cloned()
                    .ok_or_else(|| format!("Unknown classifier model \"{provider}/{id}\""))?;
                let context: ri_types::classify::ClassifierContext =
                    serde_json::from_value(context.clone()).map_err(|err| err.to_string())?;
                let options = ri_ai::api::classify::ClassifyOptions {
                    cancel,
                    ..Default::default()
                };
                serde_json::to_value(registry.classify(&model, &context, options).await)
            } else {
                let model = registry
                    .image_models()
                    .iter()
                    .find(|model| model.provider == provider && model.id == id)
                    .cloned()
                    .ok_or_else(|| format!("Unknown image model \"{provider}/{id}\""))?;
                let context: ri_types::classify::ImagesContext =
                    serde_json::from_value(context.clone()).map_err(|err| err.to_string())?;
                let options = ri_ai::api::images::ImagesOptions {
                    cancel,
                    ..Default::default()
                };
                let result = registry.generate_images(&model, &context, options).await;
                let images = result
                    .output
                    .iter()
                    .filter(|block| matches!(block, ri_types::classify::ImagesContent::Image(_)))
                    .count();
                self.generated_images.fetch_add(images, Ordering::Relaxed);
                serde_json::to_value(result)
            }
            .map_err(|err| err.to_string())?
        };
        let usage: Option<Usage> = result
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok());
        {
            let mut progress = lock(&self.progress);
            let record = &mut progress.calls[index];
            record.duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            record.status = result.as_object().map_or("error", models::status);
            if let Some(error) = result["errorMessage"].as_str() {
                record.error = Some(truncate_text(error, ERROR_PREVIEW_CHARS));
            }
            if let Some(usage) = &usage {
                record.cost = Some(usage.cost.total);
            }
        }
        if let Some(usage) = usage {
            let mut total = lock(&self.model_usage);
            *total = Some(match total.as_ref() {
                Some(previous) => ri_core::compaction::combine_usage(previous, &usage),
                None => usage,
            });
        }
        self.publish();
        Ok(script_value(Some(&result)))
    }
}

/// The bridge an instance holds: dropped with the run, so late operations
/// find nothing.
struct WeakBridge(Weak<ScriptBridge>);

impl Bridge for WeakBridge {
    fn request(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        let bridge = self.0.upgrade().ok_or("The script has ended")?;
        if kind != "codemode.output" {
            return Err(format!("{kind} is not available to scripts"));
        }
        let mut progress = lock(&bridge.progress);
        if !progress.finished {
            progress.output.push(payload.clone());
        }
        Ok(Value::Null)
    }

    fn start(&self, kind: &str, payload: Value) -> BoxFuture<'static, Result<Value, String>> {
        let Some(bridge) = self.0.upgrade() else {
            return Box::pin(async { Err("The script has ended".into()) });
        };
        let name = payload["name"].as_str().unwrap_or_default().to_owned();
        let args = payload["args"].as_str().map(str::to_owned);
        match kind {
            "codemode.call" => Box::pin(bridge.call(name, args)),
            "codemode.global" => {
                let args: Value = args
                    .as_deref()
                    .and_then(|args| serde_json::from_str(args).ok())
                    .unwrap_or(Value::Null);
                if name.starts_with("models.") {
                    return Box::pin(bridge.model_global(name, args));
                }
                let result = bridge.global(&name, &args);
                Box::pin(async move { result })
            }
            other => {
                let message = format!("{other} is not available to scripts");
                Box::pin(async move { Err(message) })
            }
        }
    }

    /// Engine diagnostics are discarded, as pi discards them.
    fn log(&self, _level: &str, _message: &str) {}
}

/// How a script ended.
enum Ending {
    Done {
        value: Option<String>,
        writes: Option<String>,
    },
    Failed(String),
}

/// `error.stack`, or `<name>: <message>`, of the prelude's error description.
fn script_error(json: &str) -> String {
    let error: Value = serde_json::from_str(json).unwrap_or_default();
    if let Some(stack) = error["stack"].as_str() {
        return stack.to_owned();
    }
    format!(
        "{}: {}",
        error["name"].as_str().unwrap_or("Error"),
        error["message"].as_str().unwrap_or_default()
    )
}

fn summary(calls: &[CallRecord]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".into();
    }
    let calls: Vec<String> = calls
        .iter()
        .map(|call| format!("{} ({})", call.name, call.status))
        .collect();
    format!(
        "Tool calls made before the failure (they are not undone): {}",
        calls.join(", ")
    )
}

/// Writes the full text output to a temp file; the path or why not.
fn spill(text: &str) -> Result<String, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|err| err.to_string())?;
    let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = std::env::temp_dir().join(format!("ri-codemode-{name}.txt"));
    std::fs::write(&path, text).map_err(|err| err.to_string())?;
    Ok(path.display().to_string())
}

/// Applies the token budget: over it, the text items become one item with
/// the start and end of the text, and images follow it; pi's `truncateOutput`.
fn truncate_output(items: Vec<Value>, max_tokens: u64) -> (Vec<Value>, Option<String>) {
    let texts: Vec<&str> = items
        .iter()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect();
    let combined = texts.join("\n");
    let length = ri_types::js::len(&combined);
    let budget = (max_tokens as usize).saturating_mul(CHARS_PER_TOKEN);
    if texts.is_empty() || length <= budget {
        return (items, None);
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let removed = length - head_chars - tail_chars;
    let head = ri_types::js::slice(&combined, 0, head_chars);
    let tail = if tail_chars > 0 {
        ri_types::js::slice(&combined, length - tail_chars, length)
    } else {
        String::new()
    };
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{head}…{} tokens truncated…{tail}",
        length.div_ceil(CHARS_PER_TOKEN),
        combined.split('\n').count(),
        removed.div_ceil(CHARS_PER_TOKEN),
    );
    let spilled = spill(&combined);
    match &spilled {
        Ok(path) => text += &format!("\n\n[Full output: {path} (read with offset/limit)]"),
        Err(error) => text += &format!("\n\n[Could not save the full output: {error}]"),
    }
    let mut out = vec![json!({"type": "text", "text": text})];
    out.extend(items.into_iter().filter(|item| item["type"] == "image"));
    (out, spilled.ok())
}

/// `writes` from the prelude as the `codemode-store` entry data, or `None`
/// when the script wrote nothing.
fn store_entry(writes: &str) -> Option<Value> {
    let writes: Vec<Vec<Value>> = serde_json::from_str(writes).ok()?;
    let mut set = Map::new();
    let mut deleted = Vec::new();
    for write in writes {
        let Some(key) = write.first().and_then(Value::as_str) else {
            continue;
        };
        match write.get(1).and_then(Value::as_str) {
            Some(json) => {
                set.insert(
                    key.to_owned(),
                    serde_json::from_str(json).unwrap_or(Value::Null),
                );
            }
            None => deleted.push(Value::String(key.to_owned())),
        }
    }
    (!set.is_empty() || !deleted.is_empty()).then(|| json!({"set": set, "delete": deleted}))
}

/// What runs scripts: the engine, created on first use.
pub(crate) struct Runner {
    cache_dir: Option<std::path::PathBuf>,
    engine: Arc<tokio::sync::OnceCell<Engine>>,
    /// The codemode reference; scripts with a session reach `models` when set.
    docs: Option<String>,
}

impl Runner {
    /// A runner that compiles the runtime on first use, caching it in
    /// `cache_dir`. With `docs`, scripts reach `models`.
    pub fn new(cache_dir: Option<std::path::PathBuf>, docs: Option<String>) -> Runner {
        Runner {
            cache_dir,
            engine: Arc::new(tokio::sync::OnceCell::new()),
            docs,
        }
    }

    /// A runner on `engine`, with `models` when `docs` is set.
    pub fn with_engine(engine: Engine, docs: Option<String>) -> Runner {
        Runner {
            cache_dir: None,
            engine: Arc::new(tokio::sync::OnceCell::new_with(Some(engine))),
            docs,
        }
    }

    /// The engine, loaded on first use. The load runs in a task of its own,
    /// as pi's wasm module promise does, so a script that times out or is
    /// aborted while it loads leaves it to finish for the next script.
    async fn engine(&self) -> Result<&Engine, String> {
        if let Some(engine) = self.engine.get() {
            return Ok(engine);
        }
        let (cell, cache) = (Arc::clone(&self.engine), self.cache_dir.clone());
        tokio::spawn(async move {
            cell.get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || Engine::new(cache.as_deref()))
                    .await
                    .map_err(|err| err.to_string())?
                    .map_err(|err| err.to_string())
            })
            .await
            .map(|_| ())
        })
        .await
        .map_err(|err| err.to_string())??;
        self.engine
            .get()
            .ok_or_else(|| "the engine was not loaded".to_owned())
    }

    /// Runs script `args.code` for tool call `call_id`. `Err` for invalid
    /// options, which pi reports without the result header.
    pub async fn execute(
        &self,
        session: Option<AgentSession>,
        call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> Result<ToolResult, String> {
        let started = Instant::now();
        let (code, options) = parse_source(args["code"].as_str().unwrap_or_default())?;
        let tools: Vec<Callable> = session
            .as_ref()
            .map(AgentSession::callable_tools)
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| tool.name() != super::NAME)
            .map(|registered| {
                let declaration = super::declaration(&registered);
                let sample = sample(&declaration);
                Callable {
                    registered,
                    declaration,
                    sample,
                }
            })
            .collect();
        let tools_json: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({"name": tool.registered.name(), "jsName": identifier(tool.registered.name()), "description": tool.sample})
            })
            .collect();
        let store = read_store(session.as_ref());
        // pi's models need the session's registry.
        let docs = self.docs.clone().filter(|_| session.is_some());
        let mut globals: Vec<Value> = ["searchTools", "describeTool", "describeNamespace"]
            .iter()
            .map(|name| json!({"name": name, "spread": true}))
            .collect();
        if docs.is_some() {
            globals.extend(
                models::GLOBALS
                    .iter()
                    .map(|name| json!({"name": name, "spread": true})),
            );
        }
        let bridge = Arc::new(ScriptBridge {
            session: session.clone(),
            call_id,
            tools,
            progress: Mutex::default(),
            updates,
            calls: CancellationToken::new(),
            docs,
            model_slots: tokio::sync::Semaphore::new(models::MAX_CONCURRENT_MODEL_CALLS),
            model_calls: AtomicU64::new(0),
            model_usage: Mutex::default(),
            generated_images: AtomicUsize::new(0),
        });
        let payload = json!({
            "code": code,
            "tools": ri_types::json::to_string(&tools_json).unwrap_or_default(),
            "globals": ri_types::json::to_string(&globals).unwrap_or_default(),
            "store": ri_types::json::to_string(&store).unwrap_or_default(),
        });
        let ending = self
            .run(&bridge, session.as_ref(), &payload, &options, &cancel)
            .await;

        let (output, mut calls) = {
            let mut progress = lock(&bridge.progress);
            progress.finished = true;
            (std::mem::take(&mut progress.output), progress.calls.clone())
        };
        bridge.calls.cancel();
        // Calls still running were cut off by the end, a timeout or an abort.
        for call in &mut calls {
            if call.status == "running" {
                call.status = "cancelled";
            }
        }
        let mut items = output;
        let ok = match &ending {
            Ending::Done { value, writes } => {
                if let (Some(session), Some(data)) =
                    (&session, writes.as_deref().and_then(store_entry))
                {
                    let _ = session.append_custom_entry(STORE_ENTRY_TYPE, Some(data));
                }
                if let Some(value) = value {
                    let parsed: Value = serde_json::from_str(value).unwrap_or(Value::Null);
                    let text = match parsed {
                        Value::String(text) => text,
                        other => ri_types::json::to_string(&other).unwrap_or_default(),
                    };
                    items.push(json!({"type": "text", "text": text}));
                }
                true
            }
            Ending::Failed(head) => {
                items.push(json!({"type": "text", "text": format!("Script error:\n{head}\n\n{}", summary(&calls))}));
                false
            }
        };
        let generated = bridge.generated_images.load(Ordering::Relaxed);
        if generated > 0 && !items.iter().any(|item| item["type"] == "image") {
            items.push(models::unshown_images_note(generated));
        }
        let (items, full_output_path) = truncate_output(
            items,
            options
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        );
        let wall = ri_types::js::to_fixed(started.elapsed().as_secs_f64(), 1);
        let header = format!(
            "{}\nWall time {wall} seconds\nOutput:\n",
            if ok {
                "Script completed"
            } else {
                "Script failed"
            }
        );
        let mut content = vec![ContentBlock::Text(ri_types::message::TextContent {
            text: header,
            text_signature: None,
        })];
        content.extend(
            items
                .into_iter()
                .filter_map(|item| serde_json::from_value::<ContentBlock>(item).ok()),
        );
        let mut details = json!({"calls": calls.iter().map(CallRecord::json).collect::<Vec<_>>()});
        if let Some(path) = full_output_path {
            details["fullOutputPath"] = json!(path);
        }
        let usage = lock(&bridge.model_usage).take();
        Ok(ToolResult {
            content,
            details: Some(details),
            usage,
            is_error: (!ok).then_some(true),
            ..ToolResult::default()
        })
    }

    /// Runs the script in a fresh instance until it ends, times out or is
    /// aborted.
    async fn run(
        &self,
        bridge: &Arc<ScriptBridge>,
        session: Option<&AgentSession>,
        payload: &Value,
        options: &SourceOptions,
        cancel: &CancellationToken,
    ) -> Ending {
        let timeout = async {
            match options.timeout_ms {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending().await,
            }
        };
        let aborted = || Ending::Failed("Script aborted: This operation was aborted".into());
        let timed_out = || {
            Ending::Failed(format!(
                "Script timed out: Execution timed out after {} ms",
                options.timeout_ms.unwrap_or_default()
            ))
        };
        let sandbox = |message: String| Ending::Failed(format!("Script sandbox failed: {message}"));
        let script = async {
            let engine = match self.engine().await {
                Ok(engine) => engine,
                Err(error) => return sandbox(format!("Failed to load QuickJS: {error}")),
            };
            let cwd =
                session.map_or_else(std::env::temp_dir, |session| session.cwd().to_path_buf());
            let mut instance_options = Options::new(cwd);
            instance_options.grants = Grants::none();
            instance_options.filesystem_roots = Vec::new();
            instance_options.memory_limit = INSTANCE_MEMORY_LIMIT;
            let instance = match Instance::start(
                engine,
                instance_options,
                Arc::new(WeakBridge(Arc::downgrade(bridge))),
            )
            .await
            {
                Ok(instance) => instance,
                Err(error) => return sandbox(format!("Failed to start the sandbox: {error}")),
            };
            match instance.call("codemode", payload).await {
                Ok(done) if done["ok"] == true => Ending::Done {
                    value: done["value"].as_str().map(str::to_owned),
                    writes: done["writes"].as_str().map(str::to_owned),
                },
                Ok(done) => Ending::Failed(script_error(done["error"].as_str().unwrap_or("{}"))),
                // A trap, such as runaway recursion, which QuickJS does not
                // bound on WASI: its reason without the wasm backtrace.
                Err(crate::Error::Crashed(reason)) => sandbox(
                    reason
                        .rfind("wasm trap: ")
                        .map_or(reason.as_str(), |index| &reason[index..])
                        .to_owned(),
                ),
                Err(error) => sandbox(error.to_string()),
            }
        };
        tokio::select! {
            ending = script => ending,
            () = cancel.cancelled() => aborted(),
            () = timeout => timed_out(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script that gives up while the engine loads, as one with a short
    /// timeout does, leaves the load running for the next script.
    #[tokio::test(flavor = "multi_thread")]
    async fn engine_load_outlives_a_script_that_gives_up() {
        let runner = Runner::new(None, None);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), runner.engine())
                .await
                .is_err()
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        while !runner.engine.initialized() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(runner.engine.initialized());
    }

    #[test]
    fn parses_pi_options_lines() {
        assert_eq!(
            parse_source("  // @options: {\"timeout_ms\": 5}\r\nreturn 1").unwrap(),
            (
                "\nreturn 1".into(),
                SourceOptions {
                    max_output_tokens: None,
                    timeout_ms: Some(5)
                }
            )
        );
        assert_eq!(
            parse_source("//@options: {}\nx").unwrap().0,
            "//@options: {}\nx"
        );
        assert_eq!(
            parse_source("// @options: {\"foo\": 1}\nreturn 1").unwrap_err(),
            "@options only supports `max_output_tokens` and `timeout_ms`; got `foo`"
        );
        assert_eq!(
            parse_source("// @options: {\"timeout_ms\": 0}\nx").unwrap_err(),
            "@options field `timeout_ms` must be a positive integer up to 2147483647"
        );
        assert_eq!(
            parse_source("// @options: []\nx").unwrap_err(),
            "@options must be a JSON object with supported fields `max_output_tokens` and `timeout_ms`"
        );
        assert_eq!(
            parse_source("// @options: {}").unwrap_err(),
            "The @options line must be followed by JavaScript source on subsequent lines"
        );
        assert!(
            parse_source(" \n")
                .unwrap_err()
                .starts_with("Expected JavaScript source text")
        );
    }

    #[test]
    fn truncates_like_pi() {
        let items = vec![
            json!({"type": "text", "text": "x".repeat(100)}),
            json!({"type": "text", "text": "tail-end"}),
        ];
        let (items, path) = truncate_output(items, 10);
        let path = path.unwrap();
        assert_eq!(
            items[0]["text"],
            format!(
                "Warning: truncated output (original token count: 28)\nTotal output lines: 2\n\n{}…18 tokens truncated…{}\ntail-end\n\n[Full output: {path} (read with offset/limit)]",
                "x".repeat(20),
                "x".repeat(11)
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\ntail-end", "x".repeat(100))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn store_writes_become_entry_data() {
        assert_eq!(
            store_entry("[[\"k\",\"{\\\"n\\\":1}\"],[\"gone\"]]"),
            Some(json!({"set": {"k": {"n": 1}}, "delete": ["gone"]}))
        );
        assert_eq!(store_entry("[]"), None);
    }
}
