//! An MCP client over one transport: the `initialize` handshake, requests with
//! timeouts, progress and cancellation, server requests (`ping`,
//! `roots/list`) and notifications. Port of `client.ts` in pi-mcp `v1.0.0`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use yapi_types::sync::lock;

use super::jsonrpc::{Incoming, METHOD_NOT_FOUND, McpError, classify, id_key, is_id};
use super::transport::{Event, Transport};

/// The protocol version the client asks for.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// Versions accepted from a server; servers answer with their own latest one
/// when they do not support the requested version.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const MAX_LIST_PAGES: usize = 1_000;

/// A directory the client exposes to the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    /// A `file://` URI.
    pub uri: String,
    /// A display name.
    pub name: Option<String>,
}

/// Who the client is and how it behaves.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientOptions {
    /// Client name, sent in `initialize`.
    pub name: String,
    /// Client version.
    pub version: String,
    /// Default per-request timeout; zero waits forever.
    pub request_timeout: Duration,
    /// Roots answered to `roots/list`; with any, the `roots` capability is
    /// declared.
    pub roots: Vec<Root>,
}

/// Receives the parameters of `notifications/progress` for one request.
pub type ProgressListener = Arc<dyn Fn(&Value) + Send + Sync>;

/// Per-request options.
#[derive(Clone, Default)]
pub struct RequestOptions {
    /// Cancels the request; the server is notified.
    pub cancel: Option<CancellationToken>,
    /// Overrides the client's timeout. Progress notifications restart it.
    pub timeout: Option<Duration>,
    /// Asks for progress notifications and receives them.
    pub on_progress: Option<ProgressListener>,
}

/// A tool a server offers.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// Name as the server knows it.
    pub name: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// What it does.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON schema of the arguments.
    pub input_schema: Map<String, Value>,
    /// JSON schema of `structuredContent`.
    #[serde(default)]
    pub output_schema: Option<Map<String, Value>>,
    /// Hints such as `readOnlyHint`.
    #[serde(default)]
    pub annotations: Option<Map<String, Value>>,
}

/// What the server said in `initialize`.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerInfo {
    /// The negotiated protocol version.
    pub protocol_version: String,
    /// Server capabilities.
    pub capabilities: Map<String, Value>,
    /// Instructions for using the server's tools.
    pub instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Connecting,
    Connected,
    Closed,
}

struct Pending {
    reply: oneshot::Sender<Result<Value, McpError>>,
    progress: Option<ProgressListener>,
    touched: Arc<Notify>,
}

type Listener = Arc<dyn Fn(&Value) + Send + Sync>;

struct Inner {
    options: ClientOptions,
    transport: Transport,
    state: Mutex<State>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    /// Progress token to request id.
    progress: Mutex<HashMap<String, String>>,
    server: Mutex<Option<ServerInfo>>,
    listeners: Mutex<Vec<(String, Listener)>>,
    close_listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

/// A connected client. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

fn validate_initialize(value: &Value) -> Result<ServerInfo, McpError> {
    let invalid = || McpError::invalid("Invalid MCP initialize result");
    let object = value.as_object().ok_or_else(invalid)?;
    let version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let info = object
        .get("serverInfo")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if !info.get("name").is_some_and(Value::is_string)
        || !info.get("version").is_some_and(Value::is_string)
    {
        return Err(invalid());
    }
    let instructions = match object.get("instructions") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid()),
    };
    Ok(ServerInfo {
        protocol_version: version.to_owned(),
        capabilities: capabilities.clone(),
        instructions,
    })
}

/// One page of a list result: the items under `key`, each checked by `valid`.
fn list_page(
    method: &str,
    key: &str,
    value: &Value,
    valid: fn(&Map<String, Value>) -> bool,
) -> Result<(Vec<Value>, Option<String>), McpError> {
    let items = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::invalid(format!("Invalid MCP {method} result")))?;
    for item in items {
        if !item.as_object().is_some_and(valid) {
            return Err(McpError::invalid(format!(
                "Invalid entry in MCP {method} result"
            )));
        }
    }
    // Some servers end pagination with `null` or `""` instead of omitting it.
    let cursor = match value.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(McpError::invalid(format!("Invalid MCP {method} cursor")));
        }
    };
    Ok((items.clone(), cursor))
}

fn is_tool(tool: &Map<String, Value>) -> bool {
    tool.get("name").is_some_and(Value::is_string)
        && tool.get("inputSchema").is_some_and(Value::is_object)
}

/// `name` is required by the spec, but some servers omit it.
fn is_resource(resource: &Map<String, Value>) -> bool {
    resource.get("uri").is_some_and(Value::is_string)
        && resource.get("name").is_none_or(Value::is_string)
}

fn is_resource_template(template: &Map<String, Value>) -> bool {
    template.get("uriTemplate").is_some_and(Value::is_string)
        && template.get("name").is_none_or(Value::is_string)
}

/// Fills a missing `name` from `field`, as pi-mcp does.
fn with_name(mut item: Value, field: &str) -> Value {
    if item.get("name").is_none()
        && let Some(fallback) = item.get(field).cloned()
        && let Some(object) = item.as_object_mut()
    {
        object.insert("name".into(), fallback);
    }
    item
}

/// `items` with names from their `field` where they lack one.
fn named(items: Vec<Value>, field: &str) -> Vec<Value> {
    items
        .into_iter()
        .map(|item| with_name(item, field))
        .collect()
}

impl McpClient {
    /// Opens `transport` and performs the `initialize` handshake.
    pub async fn connect(
        options: ClientOptions,
        transport: Transport,
    ) -> Result<McpClient, McpError> {
        Self::connect_with_stderr(options, transport)
            .await
            .map_err(|failure| failure.0)
    }

    /// [`McpClient::connect`], failing with a stdio server's stderr too, as
    /// pi reads it for every failed connection.
    pub async fn connect_with_stderr(
        options: ClientOptions,
        transport: Transport,
    ) -> Result<McpClient, Box<(McpError, Option<String>)>> {
        let (events, receiver) = mpsc::unbounded_channel();
        let client = McpClient {
            inner: Arc::new(Inner {
                options,
                transport,
                state: Mutex::new(State::Connecting),
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                progress: Mutex::new(HashMap::new()),
                server: Mutex::new(None),
                listeners: Mutex::new(Vec::new()),
                close_listeners: Mutex::new(Vec::new()),
            }),
        };
        tokio::spawn(client.clone().dispatch(receiver));
        match client.handshake(events).await {
            Ok(()) => Ok(client),
            Err(error) => {
                client.close().await;
                Err(Box::new((error, client.stderr())))
            }
        }
    }

    async fn handshake(&self, events: super::transport::Events) -> Result<(), McpError> {
        self.inner.transport.start(events).await?;
        let mut capabilities = Map::new();
        if !self.inner.options.roots.is_empty() {
            capabilities.insert("roots".into(), json!({}));
        }
        let params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": capabilities,
            "clientInfo": {"name": self.inner.options.name, "version": self.inner.options.version},
        });
        let result = self
            .request_internal("initialize", Some(params), RequestOptions::default(), true)
            .await?;
        let info = validate_initialize(&result)?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&info.protocol_version.as_str()) {
            return Err(McpError::Other(format!(
                "MCP server selected unsupported protocol version {}",
                info.protocol_version
            )));
        }
        self.inner
            .transport
            .set_protocol_version(&info.protocol_version);
        *lock(&self.inner.server) = Some(info);
        self.notify_internal("notifications/initialized", None, true)
            .await?;
        *lock(&self.inner.state) = State::Connected;
        Ok(())
    }

    /// Whether `other` is a clone of this client.
    pub fn same(&self, other: &McpClient) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Whether the connection is open.
    pub fn is_connected(&self) -> bool {
        *lock(&self.inner.state) == State::Connected
    }

    /// What the server said in `initialize`.
    pub fn server(&self) -> Option<ServerInfo> {
        lock(&self.inner.server).clone()
    }

    /// The end of a stdio server's stderr.
    pub fn stderr(&self) -> Option<String> {
        self.inner.transport.stderr()
    }

    /// Calls `listener` with the parameters of every `method` notification.
    pub fn on_notification(&self, method: &str, listener: impl Fn(&Value) + Send + Sync + 'static) {
        lock(&self.inner.listeners).push((method.to_owned(), Arc::new(listener)));
    }

    /// Calls `listener` once when the connection closes, for any reason.
    pub fn on_close(&self, listener: impl Fn() + Send + Sync + 'static) {
        lock(&self.inner.close_listeners).push(Arc::new(listener));
    }

    /// Sends a request and waits for its result.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        self.request_internal(method, params, options, false).await
    }

    /// Every tool, following pagination.
    pub async fn list_tools(&self, options: RequestOptions) -> Result<Vec<Tool>, McpError> {
        let items = self
            .list_all("tools/list", "tools", is_tool, options)
            .await?;
        items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item)
                    .map_err(|_| McpError::invalid("Invalid entry in MCP tools/list result"))
            })
            .collect()
    }

    /// Every resource, following pagination.
    pub async fn list_resources(&self, options: RequestOptions) -> Result<Vec<Value>, McpError> {
        let items = self
            .list_all("resources/list", "resources", is_resource, options)
            .await?;
        Ok(named(items, "uri"))
    }

    /// One page of resources from `cursor`, and the next cursor.
    pub async fn list_resources_page(
        &self,
        cursor: Option<String>,
        options: RequestOptions,
    ) -> Result<(Vec<Value>, Option<String>), McpError> {
        let (items, next) = self
            .list_page("resources/list", "resources", is_resource, cursor, options)
            .await?;
        Ok((named(items, "uri"), next))
    }

    /// Every resource template, following pagination.
    pub async fn list_resource_templates(
        &self,
        options: RequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        let items = self
            .list_all(
                "resources/templates/list",
                "resourceTemplates",
                is_resource_template,
                options,
            )
            .await?;
        Ok(named(items, "uriTemplate"))
    }

    /// One page of resource templates from `cursor`, and the next cursor.
    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<String>,
        options: RequestOptions,
    ) -> Result<(Vec<Value>, Option<String>), McpError> {
        let (items, next) = self
            .list_page(
                "resources/templates/list",
                "resourceTemplates",
                is_resource_template,
                cursor,
                options,
            )
            .await?;
        Ok((named(items, "uriTemplate"), next))
    }

    /// Reads one resource.
    pub async fn read_resource(
        &self,
        uri: &str,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        let result = self
            .request("resources/read", Some(json!({ "uri": uri })), options)
            .await?;
        let contents = result
            .get("contents")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::invalid("Invalid MCP resources/read result"))?;
        for item in contents {
            let valid = item.get("uri").is_some_and(Value::is_string)
                && (item.get("text").is_some_and(Value::is_string)
                    || item.get("blob").is_some_and(Value::is_string));
            if !valid {
                return Err(McpError::invalid(
                    "Invalid contents in MCP resources/read result",
                ));
            }
        }
        Ok(result)
    }

    /// Calls a tool. A result without `content` gets an empty one, as the SDKs
    /// default it.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        let mut params = json!({ "name": name });
        if !arguments.is_null() {
            params["arguments"] = arguments;
        }
        let mut result = self.request("tools/call", Some(params), options).await?;
        let object = result
            .as_object_mut()
            .filter(|object| object.get("content").is_none_or(Value::is_array))
            .ok_or_else(|| McpError::invalid("Invalid MCP tools/call result"))?;
        if object
            .get("structuredContent")
            .is_some_and(|value| !value.is_object())
        {
            return Err(McpError::invalid(
                "Invalid MCP tools/call structured content",
            ));
        }
        if !object.contains_key("content") {
            object.insert("content".into(), json!([]));
        }
        Ok(result)
    }

    /// Closes the connection; requests in flight fail.
    pub async fn close(&self) {
        self.mark_closed();
        self.inner.transport.close().await;
    }

    async fn list_page(
        &self,
        method: &str,
        key: &str,
        valid: fn(&Map<String, Value>) -> bool,
        cursor: Option<String>,
        options: RequestOptions,
    ) -> Result<(Vec<Value>, Option<String>), McpError> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        let result = self.request(method, params, options).await?;
        list_page(method, key, &result, valid)
    }

    async fn list_all(
        &self,
        method: &str,
        key: &str,
        valid: fn(&Map<String, Value>) -> bool,
        options: RequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor = None;
        for _ in 0..MAX_LIST_PAGES {
            let (page, next) = self
                .list_page(method, key, valid, cursor, options.clone())
                .await?;
            items.extend(page);
            let Some(next) = next else {
                return Ok(items);
            };
            if !seen.insert(next.clone()) {
                return Err(McpError::Other(format!(
                    "MCP {method} returned duplicate cursor: {next}"
                )));
            }
            cursor = Some(next);
        }
        Err(McpError::Other(format!(
            "MCP {method} exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    fn require(&self, allow_connecting: bool) -> Result<(), McpError> {
        let state = *lock(&self.inner.state);
        match state {
            State::Connected => Ok(()),
            State::Connecting if allow_connecting => Ok(()),
            State::Connecting => Err(McpError::Closed("MCP client is connecting".into())),
            State::Closed => Err(McpError::Closed("MCP client is closed".into())),
        }
    }

    async fn request_internal(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
        allow_connecting: bool,
    ) -> Result<Value, McpError> {
        self.require(allow_connecting)?;
        let cancel = options.cancel.clone().unwrap_or_default();
        if cancel.is_cancelled() {
            return Err(McpError::Aborted);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let key = id_key(&json!(id));
        let params = match (&options.on_progress, params) {
            (Some(_), params) => {
                let mut params = params.unwrap_or_else(|| json!({}));
                let mut meta = params
                    .get("_meta")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                meta.insert("progressToken".into(), json!(id));
                params["_meta"] = Value::Object(meta);
                Some(params)
            }
            (None, params) => params,
        };
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        let (reply, mut result) = oneshot::channel();
        let touched = Arc::new(Notify::new());
        lock(&self.inner.pending).insert(
            key.clone(),
            Pending {
                reply,
                progress: options.on_progress.clone(),
                touched: Arc::clone(&touched),
            },
        );
        if options.on_progress.is_some() {
            lock(&self.inner.progress).insert(key.clone(), key.clone());
        }
        if let Err(error) = self.inner.transport.send(&message).await {
            self.remove_pending(&key);
            return Err(error);
        }
        let timeout = options
            .timeout
            .unwrap_or(self.inner.options.request_timeout);
        // The spec forbids cancelling `initialize`.
        let cancellable = method != "initialize";
        loop {
            let expired = async {
                if timeout.is_zero() {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(timeout).await;
            };
            tokio::select! {
                outcome = &mut result => {
                    return outcome.unwrap_or_else(|_| Err(McpError::closed()));
                }
                () = touched.notified() => {}
                () = expired => {
                    self.remove_pending(&key);
                    if cancellable {
                        self.send_cancelled(id, "Request timed out");
                    }
                    return Err(McpError::Timeout(timeout.as_millis() as u64));
                }
                () = cancel.cancelled() => {
                    self.remove_pending(&key);
                    if cancellable {
                        self.send_cancelled(id, "AbortError: This operation was aborted");
                    }
                    return Err(McpError::Aborted);
                }
            }
        }
    }

    fn send_cancelled(&self, id: u64, reason: &str) {
        let client = self.clone();
        let message = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": reason},
        });
        tokio::spawn(async move {
            if !matches!(*lock(&client.inner.state), State::Closed) {
                let _ = client.inner.transport.send(&message).await;
            }
        });
    }

    async fn notify_internal(
        &self,
        method: &str,
        params: Option<Value>,
        allow_connecting: bool,
    ) -> Result<(), McpError> {
        self.require(allow_connecting)?;
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.inner.transport.send(&message).await
    }

    fn remove_pending(&self, key: &str) -> Option<Pending> {
        lock(&self.inner.progress).retain(|_, request| request != key);
        lock(&self.inner.pending).remove(key)
    }

    /// Rejects requests in flight and reports the close once.
    fn mark_closed(&self) {
        let was = std::mem::replace(&mut *lock(&self.inner.state), State::Closed);
        let pending: Vec<Pending> = lock(&self.inner.pending).drain().map(|(_, p)| p).collect();
        lock(&self.inner.progress).clear();
        for entry in pending {
            let _ = entry.reply.send(Err(McpError::closed()));
        }
        if was != State::Closed {
            let listeners = lock(&self.inner.close_listeners).clone();
            for listener in listeners {
                listener();
            }
        }
    }

    async fn dispatch(self, mut events: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = events.recv().await {
            match event {
                Event::Message(message) => self.handle(&message),
                // pi-mcp reports these to error listeners, which pi does not add.
                Event::Error(_) => {}
                Event::Closed => {
                    self.mark_closed();
                    break;
                }
            }
        }
    }

    fn handle(&self, message: &Value) {
        match classify(message) {
            Some(Incoming::Response { id, outcome }) => {
                let Some(entry) = self.remove_pending(&id_key(id)) else {
                    return;
                };
                let _ = entry.reply.send(outcome.cloned());
            }
            Some(Incoming::Request { id, method, .. }) => {
                let result = match method {
                    "ping" => Ok(json!({})),
                    "roots/list" if !self.inner.options.roots.is_empty() => {
                        let roots: Vec<Value> = self
                            .inner
                            .options
                            .roots
                            .iter()
                            .map(|root| match &root.name {
                                Some(name) => json!({"uri": root.uri, "name": name}),
                                None => json!({"uri": root.uri}),
                            })
                            .collect();
                        Ok(json!({ "roots": roots }))
                    }
                    other => Err(json!({
                        "code": METHOD_NOT_FOUND,
                        "message": format!("Method not found: {other}"),
                    })),
                };
                let response = match result {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                };
                let client = self.clone();
                tokio::spawn(async move {
                    let _ = client.inner.transport.send(&response).await;
                });
            }
            Some(Incoming::Notification { method, params }) => {
                let params = params.cloned().unwrap_or(Value::Null);
                if method == "notifications/progress" {
                    self.progress(&params);
                }
                let listeners: Vec<Listener> = lock(&self.inner.listeners)
                    .iter()
                    .filter(|(name, _)| name == method)
                    .map(|(_, listener)| Arc::clone(listener))
                    .collect();
                for listener in listeners {
                    listener(&params);
                }
            }
            None => {}
        }
    }

    fn progress(&self, params: &Value) {
        let Some(token) = params.get("progressToken").filter(|token| is_id(token)) else {
            return;
        };
        if !params.get("progress").is_some_and(Value::is_number) {
            return;
        }
        let Some(request) = lock(&self.inner.progress).get(&id_key(token)).cloned() else {
            return;
        };
        let (touched, listener) = match lock(&self.inner.pending).get(&request) {
            Some(entry) => (Arc::clone(&entry.touched), entry.progress.clone()),
            None => return,
        };
        touched.notify_one();
        if let Some(listener) = listener {
            listener(params);
        }
    }
}
