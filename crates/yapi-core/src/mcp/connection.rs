//! One configured server's connection: lazy connects and reconnects, its
//! tools and resources, and its state for `/mcp`. Port of
//! `extensions/mcp/runtime.ts` in pi `v1.0.0`, without OAuth sign-in.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use yapi_types::sync::lock;

use super::client::{ClientOptions, McpClient, RequestOptions, ResourceKind, Root, Tool};
use super::config::{ServerEntry, ServerTransport};
use super::http::{HttpOptions, HttpTransport};
use super::jsonrpc::{METHOD_NOT_FOUND, McpError};
use super::stdio::{StdioOptions, StdioTransport};
use super::tools::is_app_resource;
use super::transport::Transport;

const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
const STDERR_TAIL_CHARS: usize = 2_000;
/// Delays between attempts to reach an HTTP server after a transient error.
const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(250), Duration::from_millis(1_000)];

/// Where a connection stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Connecting.
    Connecting,
    /// Connected.
    Connected,
    /// The connection dropped; the next call reconnects.
    Disconnected,
    /// The server needs a sign-in.
    NeedsAuth,
    /// Connecting failed.
    Failed,
    /// Shut down.
    Closed,
}

impl State {
    /// pi's name for the state.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Connecting => "connecting",
            State::Connected => "connected",
            State::Disconnected => "disconnected",
            State::NeedsAuth => "needs-auth",
            State::Failed => "failed",
            State::Closed => "closed",
        }
    }
}

/// What a connection knows of its server.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Where it stands.
    pub state: State,
    /// Why it failed or dropped.
    pub error: Option<String>,
    /// The server's tools.
    pub tools: Vec<Tool>,
    /// Whether the server offers resources.
    pub has_resources: bool,
    /// Resources listed at the last connect, without MCP App resources.
    pub resources: Vec<Value>,
    /// Server instructions from `initialize`.
    pub instructions: Option<String>,
}

/// Called when the server's tools or resources change.
pub type ToolsListener = Arc<dyn Fn(&Arc<Connection>) + Send + Sync>;

/// A configured server, connected on demand.
pub struct Connection {
    entry: ServerEntry,
    cwd: PathBuf,
    agent_dir: PathBuf,
    snapshot: Mutex<Snapshot>,
    client: Mutex<Option<McpClient>>,
    opening: tokio::sync::Mutex<()>,
    closed: AtomicBool,
    on_tools: ToolsListener,
    readable_resources: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

/// pi's `resolveConfigValueOrThrow`: environment references and `!command`.
async fn resolve_value(value: &str, description: &str) -> Result<String, McpError> {
    if let Some(resolved) = yapi_ai::credentials::resolve(value, None, false).await {
        return Ok(resolved);
    }
    if let Some(command) = value.strip_prefix('!') {
        return Err(McpError::Other(format!(
            "Failed to resolve {description} from shell command: {command}"
        )));
    }
    let missing: Vec<String> = yapi_ai::credentials::env_var_names(value)
        .into_iter()
        .filter(|name| std::env::var(name).map_or(true, |value| value.is_empty()))
        .collect();
    Err(McpError::Other(match missing.as_slice() {
        [name] => format!("Failed to resolve {description} from environment variable: {name}"),
        [] => format!("Failed to resolve {description}"),
        names => format!(
            "Failed to resolve {description} from environment variables: {}",
            names.join(", ")
        ),
    }))
}

/// `file://` URL of a directory, as Node's `pathToFileURL`.
fn file_url(path: &Path) -> String {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

impl Connection {
    /// A connection for `entry`, not yet connected. `on_tools` learns of tool
    /// and resource changes; `readable_resources` tells whether the resource
    /// tools reach a server.
    pub fn new(
        entry: ServerEntry,
        cwd: PathBuf,
        agent_dir: PathBuf,
        on_tools: ToolsListener,
        readable_resources: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Arc<Connection> {
        Arc::new(Connection {
            entry,
            cwd,
            agent_dir,
            snapshot: Mutex::new(Snapshot {
                state: State::Connecting,
                error: None,
                tools: Vec::new(),
                has_resources: false,
                resources: Vec::new(),
                instructions: None,
            }),
            client: Mutex::new(None),
            opening: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
            on_tools,
            readable_resources,
        })
    }

    /// The server's name.
    pub fn name(&self) -> &str {
        &self.entry.name
    }

    /// The configured entry.
    pub fn entry(&self) -> &ServerEntry {
        &self.entry
    }

    /// The per-request timeout.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs_f64(self.entry.config.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS))
    }

    /// What it knows of its server.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.snapshot).clone()
    }

    /// Whether `read_mcp_resource` reaches this server.
    pub fn readable_resources(&self) -> bool {
        (self.readable_resources)(&self.entry.name)
    }

    fn connected_client(&self) -> Option<McpClient> {
        lock(&self.client)
            .as_ref()
            .filter(|client| client.is_connected())
            .cloned()
    }

    /// The connected client, connecting when needed.
    pub async fn client(self: &Arc<Self>) -> Result<McpClient, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Other(format!(
                "MCP server \"{}\" is shut down",
                self.entry.name
            )));
        }
        if let Some(client) = self.connected_client() {
            return Ok(client);
        }
        let _opening = self.opening.lock().await;
        if let Some(client) = self.connected_client() {
            return Ok(client);
        }
        self.open().await
    }

    async fn open(self: &Arc<Self>) -> Result<McpClient, McpError> {
        self.set_state(State::Connecting, None);
        let http = matches!(self.entry.config.transport, ServerTransport::Http { .. });
        let mut attempt = 0;
        loop {
            match self.connect_once().await {
                Ok(client) => return Ok(client),
                Err(failure) => {
                    let (error, stderr) = *failure;
                    let delay = http.then(|| CONNECT_RETRY_DELAYS.get(attempt)).flatten();
                    attempt += 1;
                    match delay {
                        Some(delay)
                            if error.is_transient() && !self.closed.load(Ordering::SeqCst) =>
                        {
                            tokio::time::sleep(*delay).await;
                        }
                        _ => return Err(self.connect_failed(&error, stderr)),
                    }
                }
            }
        }
    }

    async fn transport(&self) -> Result<Transport, McpError> {
        let name = &self.entry.name;
        match &self.entry.config.transport {
            ServerTransport::Http { url, headers, .. } => {
                let mut resolved = Vec::new();
                for (key, value) in headers {
                    let description = format!("MCP server \"{name}\" header \"{key}\"");
                    resolved.push((key.clone(), resolve_value(value, &description).await?));
                }
                Ok(Transport::Http(HttpTransport::new(HttpOptions {
                    url: url.clone(),
                    headers: resolved,
                })))
            }
            ServerTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut resolved = Vec::new();
                for (key, value) in env {
                    let description = format!("MCP server \"{name}\" env \"{key}\"");
                    resolved.push((key.clone(), resolve_value(value, &description).await?));
                }
                Ok(Transport::Stdio(Box::new(StdioTransport::new(
                    StdioOptions {
                        command: crate::tools::path::expand_home(command),
                        args: args
                            .iter()
                            .map(|arg| crate::tools::path::expand_home(arg))
                            .collect(),
                        cwd: self.cwd.join(crate::tools::path::expand_home(
                            cwd.as_deref().unwrap_or("."),
                        )),
                        env: resolved,
                    },
                ))))
            }
        }
    }

    /// One attempt; on failure, the error and a stdio server's stderr.
    async fn connect_once(self: &Arc<Self>) -> Result<McpClient, Box<(McpError, Option<String>)>> {
        let transport = self
            .transport()
            .await
            .map_err(|error| Box::new((error, None)))?;
        let options = ClientOptions {
            name: "yapi".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            request_timeout: self.timeout(),
            roots: vec![Root {
                uri: file_url(&self.cwd),
                name: self
                    .cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
            }],
        };
        let client = McpClient::connect_with_stderr(options, transport).await?;
        let log = super::log::Log::new(self.agent_dir.join("mcp.log"));
        let server = self.entry.name.clone();
        client.on_notification("notifications/message", move |params| {
            log.write(&server, params)
        });
        let weak = Arc::downgrade(self);
        let refreshing = client.clone();
        client.on_notification("notifications/tools/list_changed", move |_| {
            if let Some(connection) = weak.upgrade() {
                let client = refreshing.clone();
                tokio::spawn(async move { connection.refresh_tools(client).await });
            }
        });
        let weak = Arc::downgrade(self);
        let refreshing = client.clone();
        client.on_notification("notifications/resources/list_changed", move |_| {
            if let Some(connection) = weak.upgrade() {
                let client = refreshing.clone();
                tokio::spawn(async move { connection.refresh_resources(client).await });
            }
        });
        let weak = Arc::downgrade(self);
        let watched = client.clone();
        client.on_close(move || {
            if let Some(connection) = weak.upgrade() {
                connection.client_closed(&watched);
            }
        });
        let capabilities = client
            .server()
            .map(|server| server.capabilities)
            .unwrap_or_default();
        let has_resources = capabilities.contains_key("resources");
        let setup = async {
            let tools = if capabilities.contains_key("tools") {
                client.list_tools(RequestOptions::default()).await?
            } else {
                Vec::new()
            };
            let resources = if has_resources {
                fetch_resources(&client).await
            } else {
                Vec::new()
            };
            Ok::<_, McpError>((tools, resources))
        };
        let (tools, resources) = match setup.await {
            Ok(found) => found,
            Err(error) => {
                let stderr = client.stderr();
                client.close().await;
                return Err(Box::new((error, stderr)));
            }
        };
        if self.closed.load(Ordering::SeqCst) || !client.is_connected() {
            let stderr = client.stderr();
            client.close().await;
            let reason = if self.closed.load(Ordering::SeqCst) {
                "shut down while connecting"
            } else {
                "connection closed during setup"
            };
            return Err(Box::new((McpError::Other(reason.into()), stderr)));
        }
        let instructions = client
            .server()
            .and_then(|server| server.instructions)
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        *lock(&self.client) = Some(client.clone());
        {
            let mut snapshot = lock(&self.snapshot);
            snapshot.tools = tools;
            snapshot.has_resources = has_resources;
            snapshot.resources = resources;
            snapshot.instructions = instructions;
            snapshot.state = State::Connected;
            snapshot.error = None;
        }
        (self.on_tools)(self);
        Ok(client)
    }

    fn connect_failed(&self, error: &McpError, stderr: Option<String>) -> McpError {
        let closed = self.closed.load(Ordering::SeqCst);
        let tail = stderr
            .map(|text| {
                let text = text.trim();
                let start = text
                    .char_indices()
                    .rev()
                    .nth(STDERR_TAIL_CHARS - 1)
                    .map_or(0, |(index, _)| index);
                text[start..].to_owned()
            })
            .filter(|text| !text.is_empty());
        let message = match tail {
            Some(tail) => format!("{error}\n{tail}"),
            None => error.to_string(),
        };
        self.set_state(
            if closed { State::Closed } else { State::Failed },
            Some(message.clone()),
        );
        McpError::Other(format!(
            "MCP server \"{}\" failed to connect: {message}",
            self.entry.name
        ))
    }

    fn set_state(&self, state: State, error: Option<String>) {
        let mut snapshot = lock(&self.snapshot);
        snapshot.state = state;
        snapshot.error = error;
    }

    /// The transport dropped; the next call reconnects.
    fn client_closed(&self, client: &McpClient) {
        let mut current = lock(&self.client);
        if !current.as_ref().is_some_and(|current| current.same(client))
            || self.closed.load(Ordering::SeqCst)
        {
            return;
        }
        *current = None;
        drop(current);
        let stderr = client
            .stderr()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        let error = match stderr {
            Some(stderr) => format!("Connection closed\n{stderr}"),
            None => "Connection closed".to_owned(),
        };
        self.set_state(State::Disconnected, Some(error));
    }

    async fn refresh_tools(self: Arc<Self>, client: McpClient) {
        match client.list_tools(RequestOptions::default()).await {
            Ok(tools) => {
                if self.closed.load(Ordering::SeqCst) || !client.is_connected() {
                    return;
                }
                lock(&self.snapshot).tools = tools;
                (self.on_tools)(&self);
            }
            Err(error) => {
                lock(&self.snapshot).error = Some(format!("Failed to refresh tools: {error}"));
            }
        }
    }

    async fn refresh_resources(self: Arc<Self>, client: McpClient) {
        let resources = fetch_resources(&client).await;
        if self.closed.load(Ordering::SeqCst) || !client.is_connected() {
            return;
        }
        lock(&self.snapshot).resources = resources;
        (self.on_tools)(&self);
    }

    /// Runs a request, reconnecting when needed. Read-only requests are
    /// retried once after a transient HTTP error; any request is retried once
    /// on a new session when the server forgot the old one.
    async fn with_client<T, F>(
        self: &Arc<Self>,
        read_only: bool,
        run: impl Fn(McpClient) -> F,
    ) -> Result<T, McpError>
    where
        F: std::future::Future<Output = Result<T, McpError>>,
    {
        let mut attempt = 1;
        loop {
            let client = self.client().await?;
            match run(client.clone()).await {
                Ok(value) => return Ok(value),
                Err(error)
                    if read_only
                        && attempt == 1
                        && error.status().is_some()
                        && error.is_transient() =>
                {
                    tokio::time::sleep(CONNECT_RETRY_DELAYS[0]).await;
                }
                // The server forgot the session, so it did not run the request.
                // The old client is detached, not closed: its other calls in
                // flight get the same answer and retry the same way.
                Err(McpError::SessionExpired) if attempt == 1 => {
                    let mut current = lock(&self.client);
                    if current
                        .as_ref()
                        .is_some_and(|current| current.same(&client))
                    {
                        *current = None;
                    }
                }
                Err(error) => return Err(error),
            }
            attempt += 1;
        }
    }

    /// Calls one of the server's tools. Not retried, since it may have run.
    pub async fn call_tool(
        self: &Arc<Self>,
        name: &str,
        args: Value,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        self.with_client(false, |client| {
            let (args, options) = (args.clone(), options.clone());
            async move { client.call_tool(name, args, options).await }
        })
        .await
    }

    /// Reads a resource.
    pub async fn read_resource(
        self: &Arc<Self>,
        uri: &str,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        self.with_client(true, |client| {
            let options = options.clone();
            async move { client.read_resource(uri, options).await }
        })
        .await
    }

    /// One page of resources or resource templates; no templates for servers
    /// without the method.
    pub async fn resources_page(
        self: &Arc<Self>,
        kind: ResourceKind,
        cursor: Option<String>,
        options: RequestOptions,
    ) -> Result<(Vec<Value>, Option<String>), McpError> {
        self.with_client(true, |client| {
            let (cursor, options) = (cursor.clone(), options.clone());
            async move {
                let page = client.list_resources_page(kind, cursor, options).await;
                without_templates(kind, page, (Vec::new(), None))
            }
        })
        .await
    }

    /// Every resource or resource template; no templates for servers without
    /// the method.
    pub async fn all_resources(
        self: &Arc<Self>,
        kind: ResourceKind,
        options: RequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        self.with_client(true, |client| {
            let options = options.clone();
            async move {
                without_templates(kind, client.list_resources(kind, options).await, Vec::new())
            }
        })
        .await
    }

    /// Every resource template: [`Connection::all_resources`] of templates.
    pub async fn all_resource_templates(
        self: &Arc<Self>,
        options: RequestOptions,
    ) -> Result<Vec<Value>, McpError> {
        self.all_resources(ResourceKind::Templates, options).await
    }

    /// Connects again.
    pub async fn reconnect(self: &Arc<Self>) -> Result<(), McpError> {
        let _opening = self.opening.lock().await;
        let client = lock(&self.client).take();
        if let Some(client) = client {
            client.close().await;
        }
        self.open().await.map(|_| ())
    }

    /// Shuts the connection down for good.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.set_state(State::Closed, None);
        let client = lock(&self.client).take();
        if let Some(client) = client {
            client.close().await;
        }
    }
}

/// Servers that do not implement `resources/templates/list` have no templates.
fn without_templates<T>(
    kind: ResourceKind,
    result: Result<T, McpError>,
    empty: T,
) -> Result<T, McpError> {
    match result {
        Err(McpError::Rpc { code, .. })
            if kind == ResourceKind::Templates && code == METHOD_NOT_FOUND =>
        {
            Ok(empty)
        }
        other => other,
    }
}

/// Resources at connect time, for the counts in `/mcp`. Failures leave the
/// list empty; the resource tools list on demand.
async fn fetch_resources(client: &McpClient) -> Vec<Value> {
    client
        .list_resources(ResourceKind::Resources, RequestOptions::default())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|resource| !is_app_resource(resource))
        .collect()
}
