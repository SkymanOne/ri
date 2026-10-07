//! The built-in MCP extension: connects the servers in `mcp.json` when a
//! session starts and registers their tools as `mcp__<server>__<tool>`. Port
//! of `extensions/mcp/index.ts` in pi `v1.0.0`.
//!
//! Connections run in the background. The first prompt waits only for servers
//! with `direct` tools; `tool_search` and the resource tools wait for the
//! servers they need when they run. Servers whose tools are not declared are
//! listed in the `mcp_servers` system prompt section. In the TUI `/mcp` opens
//! a manager to sign in, reconnect, enable or disable servers and change
//! their exposure, saved to the `mcp.json` that defines the server. Elsewhere
//! it reports the status; `/mcp login` signs in to OAuth servers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::Value;
use tokio::sync::{oneshot, watch};
use yapi_types::autocomplete::{ArgumentCompletions, AutocompleteItem};
use yapi_types::config::ConfigFile;
use yapi_types::rpc::SourceInfo;
use yapi_types::sync::lock;

use super::config::{self, ConfigPatch, McpExposure, ServerEntry, namespace};
use super::connection::{Connection, State};
use super::http::ProviderToken;
use super::jsonrpc::McpError;
use super::sign_in::SignInPrompt;
use super::tools::{
    LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, McpTool, READ_MCP_RESOURCE_TOOL,
    resource_tools, tool_name,
};
use crate::extensions::codemode;
use crate::extensions::tool_search::{TOOL_SEARCH_TOOL_NAME, is_tool_search};
use crate::extensions::{
    Command, Context, DialogOptions, Extension, ExtensionUi, Mode, NotifyKind, Tools,
    builtin_source,
};
use crate::tools::{Exposure, Namespace, RegisteredTool};

/// How long the first prompt waits for servers with `direct` tools.
const STARTUP_WAIT: Duration = Duration::from_secs(10);
/// The system prompt section that lists servers whose tools are not declared.
pub const SERVERS_SECTION: &str = "mcp_servers";
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;
const MAX_SERVERS_SECTION_CHARS: usize = 4096;
const USAGE: &str =
    "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";
/// How often an open `/mcp` menu looks for changes.
const MENU_REFRESH: Duration = Duration::from_millis(100);
/// pi's `EXPOSURE_DESCRIPTIONS`: the exposures `/mcp` offers.
const EXPOSURES: [(McpExposure, &str); 3] = [
    (
        McpExposure::Codemode,
        "called from codemode scripts, which find them with searchTools()",
    ),
    (
        McpExposure::Deferred,
        "not declared until tool_search loads them, then called directly; no codemode needed",
    ),
    (
        McpExposure::Direct,
        "declared to the model like built-in tools",
    ),
];

/// A menu of the `/mcp` manager; pi's `McpMenu`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpMenu {
    /// The title.
    pub title: String,
    /// Shown below the title.
    pub details: Option<String>,
    /// Shown below the details in the error color.
    pub error: Option<String>,
    /// The items.
    pub items: Vec<AutocompleteItem>,
    /// Shown when there are no items.
    pub empty: String,
    /// Value of the item selected when the menu opens.
    pub selected: Option<String>,
    /// What the confirm key does, for the key hint.
    pub confirm_label: String,
    /// What the cancel key does, for the key hint.
    pub cancel_label: String,
}

/// A screen of the `/mcp` manager; pi's `McpUi`.
#[derive(Debug)]
pub enum McpScreen {
    /// A menu, rebuilt through the receiver while it is open, keeping the
    /// selected item. The reply is the chosen item's value, or `None` when
    /// cancelled.
    Menu(watch::Receiver<McpMenu>, oneshot::Sender<Option<String>>),
    /// A title and a message, shown while an operation runs.
    Status(String, String),
    /// A title, the authorization URL, and a field for the URL the browser
    /// was redirected to. The reply is that URL, or `None` when cancelled.
    RedirectUrl(String, String, oneshot::Sender<Option<String>>),
    /// Closes the manager.
    Close,
}

/// A configured server; disabled servers have no connection.
struct Server {
    entry: ServerEntry,
    connection: Option<Arc<Connection>>,
    /// True once the connection started for the server connected or failed.
    ready: Option<watch::Receiver<bool>>,
    /// Why the last `/mcp` action failed, shown in the manager.
    message: Option<String>,
}

#[derive(Default)]
struct Shared {
    servers: Vec<Server>,
    config_errors: Vec<String>,
    /// True once startup connections settled and their problems were reported.
    startup: Option<watch::Receiver<bool>>,
    warned_unreachable: bool,
    /// `autoEnableCodemode`: activate codemode for `codemode` servers.
    auto_enable_codemode: bool,
    waited_for_startup: bool,
    generation: u64,
    /// Tool name to the `<server>\0<tool>` it belongs to.
    tool_owners: HashMap<String, String>,
    /// Tool names each server offers now.
    server_tools: HashMap<String, Vec<String>>,
    /// The last tool registered under each name.
    definitions: HashMap<String, RegisteredTool>,
    /// The exposure the resource tools were last registered with.
    resource_tools_exposure: Option<McpExposure>,
    tools: Option<Tools>,
}

/// The MCP extension. Each session has its own.
#[derive(Clone, Default)]
pub struct McpExtension {
    shared: Arc<Mutex<Shared>>,
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

fn has_direct_tools(entry: &ServerEntry) -> bool {
    entry.config.exposures().contains(&McpExposure::Direct)
}

fn has_indirect_tools(entry: &ServerEntry) -> bool {
    let exposures = entry.config.exposures();
    exposures.contains(&McpExposure::Codemode) || exposures.contains(&McpExposure::Deferred)
}

fn truncate(text: &str, max: usize) -> String {
    if yapi_types::js::len(text) <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return String::new();
    }
    format!("{}…", yapi_types::js::slice(text, 0, max - 1).trim_end())
}

/// pi's `renderServersSection`: every enabled server with codemode or
/// deferred tools, how its tools are reached, and a one-line summary.
fn render_servers_section(servers: &[(&ServerEntry, Option<String>)]) -> Option<String> {
    let mut listed: Vec<&(&ServerEntry, Option<String>)> = servers
        .iter()
        .filter(|(entry, _)| entry.config.is_enabled() && has_indirect_tools(entry))
        .collect();
    if listed.is_empty() {
        return None;
    }
    listed.sort_by(|a, b| yapi_types::collate::locale_compare(&a.0.name, &b.0.name));
    let reaches: Vec<&str> = listed
        .iter()
        .map(|(entry, _)| {
            if entry.config.exposures().contains(&McpExposure::Codemode) {
                "codemode"
            } else {
                "tool_search"
            }
        })
        .collect();
    let mut intro = String::from("MCP servers whose tools are not declared to you.");
    if reaches.contains(&"codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains(&"tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    let heads: Vec<String> = listed
        .iter()
        .zip(&reaches)
        .map(|((entry, _), reach)| format!("- {} ({reach})", namespace(&entry.name)))
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        vec![format!(
            "- … {count} more server{}; find their tools with searchTools()",
            if count == 1 { "" } else { "s" }
        )]
    };
    let size = |kept: usize| -> usize {
        let mut lines = vec![intro.clone()];
        lines.extend(heads[..kept].iter().cloned());
        lines.extend(omitted(listed.len() - kept));
        yapi_types::js::len(&lines.join("\n"))
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // Each description also takes a ": " separator.
    let per_server = (MAX_SERVERS_SECTION_CHARS - size(kept))
        .checked_div(kept)
        .map_or(0, |room| {
            room.saturating_sub(2).min(MAX_SERVER_DESCRIPTION_CHARS)
        });
    let mut lines = vec![intro.clone()];
    for (index, (entry, instructions)) in listed[..kept].iter().enumerate() {
        let source = entry
            .config
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or(instructions.as_deref())
            .unwrap_or_default();
        let summary = if per_server > 0 {
            truncate(first_line(source).trim(), per_server)
        } else {
            String::new()
        };
        if summary.is_empty() {
            lines.push(heads[index].clone());
        } else {
            lines.push(format!("{}: {summary}", heads[index]));
        }
    }
    lines.extend(omitted(listed.len() - kept));
    Some(lines.join("\n"))
}

/// Short state for lists; `with_error` appends the first line of a failure.
fn describe_state(server: &Server, with_error: bool) -> String {
    if !server.entry.config.is_enabled() {
        return "disabled".into();
    }
    let Some(connection) = &server.connection else {
        return "starting".into();
    };
    let snapshot = connection.snapshot();
    match snapshot.state {
        State::NeedsAuth => "needs sign-in".into(),
        State::Failed if with_error => format!(
            "failed: {}",
            first_line(snapshot.error.as_deref().unwrap_or("unknown error"))
        ),
        State::Failed => "failed".into(),
        State::Connected => {
            let tools = snapshot.tools.len();
            let count = snapshot.resources.len();
            let resources = if count > 0 {
                format!(" · {count} resource{}", if count == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            format!(
                "connected · {tools} tool{}{resources}",
                if tools == 1 { "" } else { "s" }
            )
        }
        State::Connecting => "connecting…".into(),
        other => other.as_str().into(),
    }
}

impl McpExtension {
    /// A new extension.
    pub fn new() -> McpExtension {
        McpExtension::default()
    }

    fn tools(&self) -> Option<Tools> {
        lock(&self.shared).tools.clone()
    }

    /// pi's `registerTools`: names, exposure and namespace for each of the
    /// server's tools; tools it dropped are re-registered as hidden.
    fn register_tools(&self, connection: &Arc<Connection>) {
        let Some(tools) = self.tools() else {
            return;
        };
        let snapshot = connection.snapshot();
        let mut shared = lock(&self.shared);
        let server = connection.name().to_owned();
        let entry = shared
            .servers
            .iter()
            .find(|candidate| candidate.entry.name == server)
            .map_or_else(|| connection.entry().clone(), |found| found.entry.clone());
        let namespace = Namespace {
            name: namespace(&server),
            description: entry
                .config
                .description
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            instructions: snapshot.instructions.clone(),
        };
        let previous = shared
            .server_tools
            .get(&server)
            .cloned()
            .unwrap_or_default();
        let mut current: Vec<String> = Vec::new();
        let mut unique: Vec<&str> = Vec::new();
        for tool in &snapshot.tools {
            if !unique.contains(&tool.name.as_str()) {
                unique.push(&tool.name);
            }
        }
        let plain: Vec<String> = unique
            .iter()
            .map(|tool| tool_name(&server, tool, |_| false))
            .collect();
        let mut registered = Vec::new();
        for tool in &snapshot.tools {
            let owner = format!("{server}\0{}", tool.name);
            let name = tool_name(&server, &tool.name, |candidate| {
                shared
                    .tool_owners
                    .get(candidate)
                    .is_some_and(|existing| *existing != owner)
                    || current.iter().any(|taken| taken == candidate)
                    || plain.iter().filter(|name| *name == candidate).count() > 1
            });
            shared.tool_owners.insert(name.clone(), owner);
            current.push(name.clone());
            let definition = RegisteredTool {
                tool: Arc::new(McpTool::new(Arc::clone(connection), tool, name.clone())),
                snippet: None,
                guidelines: Vec::new(),
                exposure: entry.config.tool_exposure(&tool.name).tool_exposure(),
                namespace: Some(namespace.clone()),
                default_active: true,
            };
            shared.definitions.insert(name, definition.clone());
            registered.push(definition);
        }
        for name in &previous {
            if !current.contains(name)
                && let Some(definition) = shared.definitions.get(name)
            {
                registered.push(RegisteredTool {
                    exposure: Exposure::Hidden,
                    ..definition.clone()
                });
            }
        }
        shared.server_tools.insert(server, current);
        drop(shared);
        for definition in registered {
            tools.register(definition);
        }
        self.sync_resource_tools();
    }

    /// Enabled, non-hidden servers with resources, with their exposure: what
    /// the resource tools reach.
    fn servers_with_resources(&self) -> Vec<(McpExposure, Arc<Connection>)> {
        lock(&self.shared)
            .servers
            .iter()
            .map(|server| (server.entry.config.exposure(), server))
            .filter(|(exposure, server)| {
                server.entry.config.is_enabled() && *exposure != McpExposure::Hidden
            })
            .filter_map(|(exposure, server)| Some((exposure, server.connection.clone()?)))
            .filter(|(_, connection)| connection.snapshot().has_resources)
            .collect()
    }

    fn resource_servers(&self) -> Vec<Arc<Connection>> {
        self.servers_with_resources()
            .into_iter()
            .map(|(_, connection)| connection)
            .collect()
    }

    /// Registers the resource tools with the widest exposure of the servers
    /// they reach; hidden when none has resources.
    fn sync_resource_tools(&self) {
        let Some(tools) = self.tools() else {
            return;
        };
        let exposures: HashSet<&'static str> = self
            .servers_with_resources()
            .iter()
            .map(|(exposure, _)| exposure.as_str())
            .collect();
        let next = [
            McpExposure::Direct,
            McpExposure::Codemode,
            McpExposure::Deferred,
        ]
        .into_iter()
        .find(|exposure| exposures.contains(exposure.as_str()))
        .unwrap_or(McpExposure::Hidden);
        let previous = {
            let mut shared = lock(&self.shared);
            let previous = shared.resource_tools_exposure;
            if previous == Some(next) || (previous.is_none() && next == McpExposure::Hidden) {
                return;
            }
            shared.resource_tools_exposure = Some(next);
            previous
        };
        let this = self.clone();
        let servers: super::tools::ResourceServers = Arc::new(move || this.resource_servers());
        let definitions = resource_tools(servers);
        let names: Vec<String> = definitions
            .iter()
            .map(|tool| yapi_agent::Tool::declaration(tool).name.clone())
            .collect();
        for tool in definitions {
            tools.register(RegisteredTool {
                tool: Arc::new(tool),
                snippet: None,
                guidelines: Vec::new(),
                exposure: next.tool_exposure(),
                namespace: None,
                default_active: true,
            });
        }
        if previous == Some(McpExposure::Direct) {
            let active = tools.active();
            tools.set_active(
                active
                    .into_iter()
                    .filter(|name| !names.contains(name))
                    .collect(),
            );
        }
    }

    /// Activates the tool that reaches undeclared MCP tools: codemode for
    /// `codemode` exposure unless `autoEnableCodemode` is false, `tool_search`
    /// for `deferred`. Warns once when neither is active.
    fn ensure_discovery_active(&self, ctx: &Context) {
        let (needs_codemode, needs_tool_search, warned, auto_enable) = {
            let shared = lock(&self.shared);
            let mut exposures = HashSet::new();
            for server in &shared.servers {
                if server.entry.config.is_enabled() {
                    exposures.extend(
                        server
                            .entry
                            .config
                            .exposures()
                            .into_iter()
                            .map(McpExposure::as_str),
                    );
                }
            }
            (
                exposures.contains("codemode"),
                exposures.contains("deferred"),
                shared.warned_unreachable,
                shared.auto_enable_codemode,
            )
        };
        if !needs_codemode && !needs_tool_search {
            return;
        }
        // Other extensions' tools of these names cannot reach MCP tools.
        let all = ctx.tools.all();
        let has_codemode = all.iter().any(codemode::is_codemode_tool);
        let has_tool_search = all.iter().any(is_tool_search);
        let active = ctx.tools.active();
        let is_active = |name: &str| active.iter().any(|active| active == name);
        let mut activate = Vec::new();
        if needs_codemode && has_codemode && auto_enable && !is_active(codemode::NAME) {
            activate.push(codemode::NAME.to_owned());
        }
        if needs_tool_search && has_tool_search && !is_active(TOOL_SEARCH_TOOL_NAME) {
            activate.push(TOOL_SEARCH_TOOL_NAME.to_owned());
        }
        let reachable: Vec<String> = active.iter().cloned().chain(activate.clone()).collect();
        if !activate.is_empty() {
            ctx.tools.set_active(reachable.clone());
        }
        let reaches = |name: &str| reachable.iter().any(|active| active == name);
        if (has_codemode && reaches(codemode::NAME))
            || (has_tool_search && reaches(TOOL_SEARCH_TOOL_NAME))
            || warned
        {
            return;
        }
        lock(&self.shared).warned_unreachable = true;
        let reason = if needs_codemode && has_codemode && !auto_enable {
            " (autoEnableCodemode is false)"
        } else {
            ""
        };
        ctx.ui.notify(
            &format!("MCP tools are only reachable from the codemode or tool_search tool, but neither is active{reason}; they cannot be called."),
            NotifyKind::Warning,
        );
    }

    /// One message for everything that needs the user after startup.
    fn report_problems(&self, ui: &dyn ExtensionUi) {
        let lines: Vec<String> = {
            let shared = lock(&self.shared);
            let mut lines: Vec<String> = shared
                .config_errors
                .iter()
                .map(|error| format!("config: {error}"))
                .collect();
            for server in &shared.servers {
                let state = server
                    .connection
                    .as_ref()
                    .map(|connection| connection.snapshot().state);
                if matches!(state, Some(State::NeedsAuth | State::Failed)) {
                    lines.push(format!(
                        "{}: {}",
                        server.entry.name,
                        describe_state(server, true)
                    ));
                }
            }
            lines
        };
        if lines.is_empty() {
            return;
        }
        let body: Vec<String> = lines.iter().map(|line| format!("  {line}")).collect();
        ui.notify(
            &format!(
                "MCP servers need attention:\n{}\nRun /mcp to fix.",
                body.join("\n")
            ),
            NotifyKind::Warning,
        );
    }

    /// Creates the server's connection and connects it in the background.
    fn start(
        &self,
        index: usize,
        ctx: &Context,
        generation: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let (entry, ready_tx) = {
            let mut shared = lock(&self.shared);
            let server = shared.servers.get_mut(index)?;
            let (ready_tx, ready_rx) = watch::channel(false);
            server.ready = Some(ready_rx);
            (server.entry.clone(), ready_tx)
        };
        let this = self.clone();
        let on_tools =
            Arc::new(move |connection: &Arc<Connection>| this.register_tools(connection));
        let this = self.clone();
        let readable = Arc::new(move |name: &str| {
            this.resource_servers()
                .iter()
                .any(|connection| connection.name() == name)
        });
        let session = ctx.session.clone();
        // pi's `getApiKeyForProvider`, for servers with `auth.provider`.
        let provider_token: ProviderToken = Arc::new(move |provider: String| {
            let session = session.clone();
            Box::pin(async move {
                let registry = session.upgrade()?.registry();
                registry.provider_token(&provider).await
            })
        });
        let connection = Connection::new(
            entry,
            ctx.cwd.clone(),
            ctx.agent_dir.clone(),
            on_tools,
            readable,
            Some(provider_token),
        );
        {
            let mut shared = lock(&self.shared);
            if shared.generation != generation {
                return None;
            }
            if let Some(server) = shared.servers.get_mut(index) {
                server.connection = Some(Arc::clone(&connection));
            }
        }
        Some(tokio::spawn(async move {
            let _ = connection.client().await;
            let _ = ready_tx.send(true);
        }))
    }

    /// Waits for `waiting` to settle, or for `cancel`.
    async fn wait_for(
        waiting: Vec<watch::Receiver<bool>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        let all = async {
            for mut ready in waiting {
                let _ = ready.wait_for(|done| *done).await;
            }
        };
        tokio::select! {
            () = all => {}
            () = cancel.cancelled() => {}
        }
    }

    fn pending(&self, filter: impl Fn(&ServerEntry) -> bool) -> Vec<watch::Receiver<bool>> {
        lock(&self.shared)
            .servers
            .iter()
            .filter(|server| server.entry.config.is_enabled() && filter(&server.entry))
            .filter(|server| {
                server
                    .connection
                    .as_ref()
                    .is_none_or(|connection| connection.snapshot().state != State::Connected)
            })
            .filter_map(|server| server.ready.clone())
            .filter(|ready| !*ready.borrow())
            .collect()
    }

    /// The plain status `/mcp` shows without the TUI.
    fn format_status(&self, agent_dir: &Path) -> String {
        let shared = lock(&self.shared);
        if shared.servers.is_empty() && shared.config_errors.is_empty() {
            return no_servers(agent_dir);
        }
        let mut lines: Vec<String> = shared
            .servers
            .iter()
            .map(|server| {
                let name = &server.entry.name;
                let exposure = server.entry.config.exposure().as_str();
                let snapshot = server
                    .connection
                    .as_ref()
                    .map(|connection| connection.snapshot());
                if snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.state == State::NeedsAuth)
                {
                    return format!("{name}: needs sign-in, run /mcp login {name} ({exposure})");
                }
                let tools = match &snapshot {
                    Some(snapshot) if snapshot.state == State::Connected => {
                        format!(", {} tools", snapshot.tools.len())
                    }
                    _ => String::new(),
                };
                let state = if !server.entry.config.is_enabled() {
                    "disabled".to_owned()
                } else {
                    match &snapshot {
                        Some(snapshot) if snapshot.state == State::Disconnected => {
                            "disconnected, reconnects on next call".to_owned()
                        }
                        Some(snapshot) => snapshot.state.as_str().to_owned(),
                        None => "starting".to_owned(),
                    }
                };
                let error = match &snapshot {
                    Some(snapshot) if snapshot.state != State::Connected => snapshot
                        .error
                        .as_ref()
                        .map(|error| format!("\n    {}", error.replace('\n', "\n    ")))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                format!("{name}: {state}{tools} ({exposure}){error}")
            })
            .collect();
        lines.extend(
            shared
                .config_errors
                .iter()
                .map(|error| format!("config error: {error}")),
        );
        lines.join("\n")
    }

    /// The server a subcommand names, or the only or preferred candidate.
    async fn pick(
        &self,
        name: Option<&str>,
        ctx: &Context,
        eligible: impl Fn(&Server) -> bool,
        preferred: impl Fn(&Server) -> bool,
        none: &str,
    ) -> Option<String> {
        let (names, preferred_names, found) = {
            let shared = lock(&self.shared);
            let found = name.map(|name| {
                shared
                    .servers
                    .iter()
                    .find(|server| server.entry.name == name)
                    .map(&eligible)
            });
            let candidates: Vec<&Server> = shared
                .servers
                .iter()
                .filter(|server| eligible(server))
                .collect();
            (
                candidates
                    .iter()
                    .map(|server| server.entry.name.clone())
                    .collect::<Vec<_>>(),
                candidates
                    .iter()
                    .filter(|server| preferred(server))
                    .map(|server| server.entry.name.clone())
                    .collect::<Vec<_>>(),
                found,
            )
        };
        if let Some(name) = name {
            return match found.flatten() {
                None => {
                    ctx.ui.notify(
                        &format!("No MCP server named \"{name}\"."),
                        NotifyKind::Error,
                    );
                    None
                }
                Some(false) => {
                    ctx.ui.notify(none, NotifyKind::Error);
                    None
                }
                Some(true) => Some(name.to_owned()),
            };
        }
        match (names.as_slice(), preferred_names.as_slice()) {
            ([], _) => {
                ctx.ui.notify(none, NotifyKind::Info);
                None
            }
            ([only], _) | (_, [only]) => Some(only.clone()),
            _ => {
                ctx.ui
                    .select("MCP server", names, DialogOptions::default())
                    .await
            }
        }
    }

    /// pi's `signIn`: a failure message, or `None` once signed in and
    /// reconnected.
    async fn sign_in(&self, name: &str, prompt: SignInPrompt) -> Option<String> {
        let Some(connection) = self
            .connection(name)
            .filter(|connection| connection.oauth_url().is_some())
        else {
            return Some(format!("MCP server \"{name}\" does not use OAuth."));
        };
        match connection.sign_in(&prompt).await {
            Err(yapi_ai::auth::AuthError::Cancelled) => return Some("Sign-in cancelled.".into()),
            Err(error) => return Some(format!("Sign-in failed: {error}")),
            Ok(()) => {}
        }
        match connection.reconnect().await {
            Err(error) => Some(format!("Signed in, but {error}")),
            Ok(()) => None,
        }
    }

    /// pi's `signOut`: removes the stored credentials and disconnects;
    /// whether any were stored.
    async fn sign_out(&self, name: &str) -> Result<bool, McpError> {
        let Some(connection) = self.connection(name) else {
            return Ok(false);
        };
        let removed = connection.remove_credentials().await?;
        connection.sign_out().await;
        Ok(removed)
    }

    /// pi's `/mcp` manager, `manage`: the servers menu and each server's
    /// menu, until closed.
    async fn manage(&self, ctx: &Context) {
        let ui = ctx.ui.as_ref();
        while let Some(Some(name)) = Self::menu(ui, || self.servers_menu(&ctx.agent_dir)).await {
            loop {
                let Some(action) = Self::menu(ui, || self.server_menu(&name)).await else {
                    return;
                };
                let Some(action) = action.filter(|_| self.server(&name, |_| ()).is_some()) else {
                    break;
                };
                if self.run_action(ctx, &name, &action).await.is_none() {
                    return;
                }
            }
        }
    }

    /// pi's `McpUi.menu`: shows the menu `build` makes, kept current while it
    /// is open. The chosen value, `Some(None)` when cancelled, or `None` when
    /// the manager closed.
    async fn menu(ui: &dyn ExtensionUi, build: impl Fn() -> McpMenu) -> Option<Option<String>> {
        let (menus, receiver) = watch::channel(build());
        let (reply, mut answer) = oneshot::channel();
        ui.mcp_manager(McpScreen::Menu(receiver, reply));
        // ponytail: polls instead of pi's change events; a listener on
        // `Connection` state changes if polling ever shows in a profile.
        let mut refresh = tokio::time::interval(MENU_REFRESH);
        loop {
            tokio::select! {
                answer = &mut answer => return answer.ok(),
                _ = refresh.tick() => {
                    let next = build();
                    menus.send_if_modified(|menu| {
                        let changed = *menu != next;
                        *menu = next;
                        changed
                    });
                }
            }
        }
    }

    /// `read` applied to server `name`.
    fn server<T>(&self, name: &str, read: impl FnOnce(&Server) -> T) -> Option<T> {
        lock(&self.shared)
            .servers
            .iter()
            .find(|server| server.entry.name == name)
            .map(read)
    }

    /// pi's `serversMenu`: servers that need the user first.
    fn servers_menu(&self, agent_dir: &Path) -> McpMenu {
        let shared = lock(&self.shared);
        let mut servers: Vec<&Server> = shared.servers.iter().collect();
        servers.sort_by(|a, b| {
            attention_rank(a)
                .cmp(&attention_rank(b))
                .then_with(|| yapi_types::collate::locale_compare(&a.entry.name, &b.entry.name))
        });
        let notices: Vec<String> = shared
            .config_errors
            .iter()
            .map(|error| format!("config: {error}"))
            .collect();
        McpMenu {
            title: "MCP servers".into(),
            error: (!notices.is_empty()).then(|| notices.join("\n")),
            items: servers
                .iter()
                .map(|server| {
                    let description = format!(
                        "{} · {} · {}",
                        describe_state(server, true),
                        server.entry.config.exposure().as_str(),
                        server.entry.scope.as_str()
                    );
                    item(&server.entry.name, &server.entry.name, Some(description))
                })
                .collect(),
            empty: no_servers(agent_dir),
            confirm_label: "manage".into(),
            cancel_label: "close".into(),
            ..McpMenu::default()
        }
    }

    /// pi's `serverMenu`: the actions the server's state allows.
    fn server_menu(&self, name: &str) -> McpMenu {
        let menu = self.server(name, |server| {
            let entry = &server.entry;
            let saved = format!("saved to the {} mcp.json", entry.scope.as_str());
            let snapshot = server
                .connection
                .as_ref()
                .map(|connection| connection.snapshot());
            let state = snapshot.as_ref().map(|snapshot| snapshot.state);
            let mut items = Vec::new();
            if !entry.config.is_enabled() {
                items.push(item("enable", "Enable", Some(saved)));
            } else {
                if state == Some(State::NeedsAuth) {
                    items.push(item("signin", "Sign in", Some("opens the browser".into())));
                }
                if let Some(snapshot) = snapshot
                    .as_ref()
                    .filter(|snapshot| snapshot.state == State::Connected)
                {
                    let offered = format!("{} offered", snapshot.tools.len());
                    items.push(item("tools", "Tools", Some(offered)));
                }
                if matches!(
                    state,
                    Some(State::Failed | State::Disconnected | State::Connected | State::NeedsAuth)
                ) {
                    items.push(item("reconnect", "Reconnect", None));
                }
                let oauth = server
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.oauth_url().is_some());
                if state == Some(State::Connected) && oauth {
                    let deletes = "deletes the stored credentials".to_owned();
                    items.push(item("signout", "Sign out", Some(deletes)));
                }
                let exposure = entry.config.exposure().as_str().to_owned();
                items.push(item("exposure", "Exposure", Some(exposure)));
                items.push(item("disable", "Disable", Some(saved)));
            }
            let details = [
                entry.describe_transport(),
                format!("{}: {}", entry.scope.as_str(), entry.source.display()),
                format!("State: {}", describe_state(server, false)),
            ];
            let connection_error = snapshot
                .as_ref()
                .filter(|snapshot| snapshot.state != State::Connected)
                .and_then(|snapshot| snapshot.error.clone());
            let error: Vec<String> = server
                .message
                .clone()
                .into_iter()
                .chain(connection_error)
                .collect();
            McpMenu {
                title: format!("MCP server {name}"),
                details: Some(details.join("\n")),
                error: (!error.is_empty()).then(|| error.join("\n")),
                selected: items.first().map(|item| item.value.clone()),
                items,
                confirm_label: "select".into(),
                cancel_label: "back".into(),
                ..McpMenu::default()
            }
        });
        menu.unwrap_or_else(|| McpMenu {
            title: name.into(),
            empty: "This server is no longer configured.".into(),
            cancel_label: "back".into(),
            ..McpMenu::default()
        })
    }

    /// pi's `showTools`; `None` when the manager closed.
    async fn show_tools(&self, ui: &dyn ExtensionUi, name: &str) -> Option<()> {
        let build = || {
            self.server(name, |server| {
                let config = &server.entry.config;
                let exposure = config.exposure();
                let overridden = if config.tool_exposure.is_empty() {
                    ""
                } else {
                    "\nSome tools override it with toolExposure."
                };
                let tools = server
                    .connection
                    .as_ref()
                    .map(|connection| connection.snapshot().tools)
                    .unwrap_or_default();
                McpMenu {
                    title: format!("Tools of {name}"),
                    details: Some(format!(
                        "Exposure {}: {}{overridden}",
                        exposure.as_str(),
                        describe_exposure(exposure)
                    )),
                    items: tools
                        .iter()
                        .map(|tool| {
                            let description =
                                first_line(tool.description.as_deref().unwrap_or_default());
                            let description = match config.tool_exposure(&tool.name) {
                                own if own == exposure => description.to_owned(),
                                own => format!("[{}] {description}", own.as_str()),
                            };
                            item(&tool.name, &tool.name, Some(description))
                        })
                        .collect(),
                    empty: "The server offers no tools.".into(),
                    confirm_label: "back".into(),
                    cancel_label: "back".into(),
                    ..McpMenu::default()
                }
            })
            .unwrap_or_default()
        };
        Self::menu(ui, build).await.map(drop)
    }

    /// pi's `chooseExposure`: why the choice could not be saved; `None` when
    /// the manager closed.
    async fn choose_exposure(&self, ctx: &Context, name: &str) -> Option<Option<String>> {
        let Some((current, source)) = self.server(name, |server| {
            (server.entry.config.exposure(), server.entry.source.clone())
        }) else {
            return Some(None);
        };
        let build = || McpMenu {
            title: format!("Exposure of {name}"),
            details: Some(format!("Saved to {}.", source.display())),
            items: EXPOSURES
                .iter()
                .map(|(exposure, description)| {
                    let mark = if *exposure == current { "✓ " } else { "  " };
                    let label = format!("{mark}{}", exposure.as_str());
                    item(exposure.as_str(), &label, Some((*description).into()))
                })
                .collect(),
            selected: Some(current.as_str().into()),
            confirm_label: "save".into(),
            cancel_label: "back".into(),
            ..McpMenu::default()
        };
        let choice = Self::menu(ctx.ui.as_ref(), build).await?;
        let exposure = EXPOSURES
            .iter()
            .map(|(exposure, _)| *exposure)
            .find(|exposure| choice.as_deref() == Some(exposure.as_str()))
            .filter(|exposure| *exposure != current);
        Some(exposure.and_then(|exposure| self.set_exposure(ctx, name, exposure)))
    }

    /// pi's `runAction`; `None` when the manager closed.
    async fn run_action(&self, ctx: &Context, name: &str, action: &str) -> Option<()> {
        let ui = ctx.ui.as_ref();
        let status = |title: String, message: &str| {
            ui.mcp_manager(McpScreen::Status(title, message.to_owned()));
        };
        let message = match action {
            "signin" => {
                let title = format!("Sign in to {name}");
                status(title.clone(), "Contacting the authorization server…");
                self.sign_in(name, manager_prompt(&ctx.ui, title)).await
            }
            "reconnect" => {
                // A failure shows as the connection's state and error.
                status(format!("MCP server {name}"), "Reconnecting…");
                if let Some(connection) = self.connection(name) {
                    let _ = connection.reconnect().await;
                }
                None
            }
            "signout" => {
                let _ = self.sign_out(name).await;
                None
            }
            "tools" => {
                self.show_tools(ui, name).await?;
                None
            }
            "exposure" => self.choose_exposure(ctx, name).await?,
            "enable" | "disable" => {
                let enable = action == "enable";
                let doing = if enable {
                    "Connecting…"
                } else {
                    "Disconnecting…"
                };
                status(format!("MCP server {name}"), doing);
                self.set_enabled(ctx, name, enable).await
            }
            _ => None,
        };
        if let Some(server) = lock(&self.shared)
            .servers
            .iter_mut()
            .find(|server| server.entry.name == name)
        {
            server.message = message;
        }
        self.ensure_discovery_active(ctx);
        Some(())
    }

    /// pi's `saveConfig`: saves `patch` to the `mcp.json` that defines the
    /// server and applies it; why it could not be saved.
    fn save_config(&self, name: &str, patch: ConfigPatch) -> Option<String> {
        let source = self.server(name, |server| server.entry.source.clone())?;
        if let Err(error) = config::update_server_config(&source, name, patch) {
            return Some(format!("Could not update {}: {error}", source.display()));
        }
        if let Some(server) = lock(&self.shared)
            .servers
            .iter_mut()
            .find(|server| server.entry.name == name)
        {
            patch.apply(&mut server.entry.config);
        }
        None
    }

    /// pi's `setEnabled`: why the config could not be saved; connection
    /// errors show in the state.
    async fn set_enabled(&self, ctx: &Context, name: &str, enabled: bool) -> Option<String> {
        if let Some(failed) = self.save_config(name, ConfigPatch::Enabled(enabled)) {
            return Some(failed);
        }
        if !enabled {
            let connection = lock(&self.shared)
                .servers
                .iter_mut()
                .find(|server| server.entry.name == name)
                .and_then(|server| server.connection.take());
            self.hide_tools(name);
            if let Some(connection) = connection {
                connection.close().await;
            }
            return None;
        }
        let (index, generation) = {
            let shared = lock(&self.shared);
            let index = shared
                .servers
                .iter()
                .position(|server| server.entry.name == name);
            (index, shared.generation)
        };
        if let Some(handle) = index.and_then(|index| self.start(index, ctx, generation)) {
            let _ = handle.await;
        }
        None
    }

    /// pi's `setExposure`: why the config could not be saved.
    fn set_exposure(&self, ctx: &Context, name: &str, exposure: McpExposure) -> Option<String> {
        if let Some(failed) = self.save_config(name, ConfigPatch::Exposure(exposure)) {
            return Some(failed);
        }
        if let Some(connection) = self
            .connection(name)
            .filter(|connection| connection.state() == State::Connected)
        {
            self.register_tools(&connection);
        }
        self.sync_resource_tools();
        // Tools no longer exposed directly leave the declared set; direct
        // tools are activated on registration.
        let indirect: HashSet<String> = ctx
            .tools
            .all()
            .into_iter()
            .filter(|tool| tool.exposure != Exposure::Direct)
            .map(|tool| tool.name)
            .collect();
        let own = lock(&self.shared)
            .server_tools
            .get(name)
            .cloned()
            .unwrap_or_default();
        let active = ctx
            .tools
            .active()
            .into_iter()
            .filter(|tool| !own.contains(tool) || !indirect.contains(tool))
            .collect();
        ctx.tools.set_active(active);
        None
    }

    /// pi's `hideTools`: makes a disabled server's tools unreachable.
    fn hide_tools(&self, name: &str) {
        let Some(tools) = self.tools() else {
            return;
        };
        let hidden: Vec<RegisteredTool> = {
            let mut shared = lock(&self.shared);
            let names = shared
                .server_tools
                .insert(name.to_owned(), Vec::new())
                .unwrap_or_default();
            names
                .iter()
                .filter_map(|tool| shared.definitions.get(tool))
                .map(|definition| RegisteredTool {
                    exposure: Exposure::Hidden,
                    ..definition.clone()
                })
                .collect()
        };
        for definition in hidden {
            tools.register(definition);
        }
        self.sync_resource_tools();
    }
}

/// The sign-in prompt of `/mcp login`: notifications and an input dialog.
fn login_prompt(ctx: &Context, name: &str) -> SignInPrompt {
    let (ui, server, tui) = (Arc::clone(&ctx.ui), name.to_owned(), ctx.mode == Mode::Tui);
    let show_authorization_url = Box::new(move |url: &str| {
        // pi links both lines; yapi shows the text and the terminal detects the URL.
        let lines = match tui {
            true if cfg!(target_os = "macos") => format!("{url}\nCmd+click to open"),
            true => format!("{url}\nCtrl+click to open"),
            false => url.to_owned(),
        };
        ui.notify(
            &format!("Sign in to MCP server \"{server}\" in your browser:\n{lines}"),
            NotifyKind::Info,
        );
        yapi_ai::auth::open_browser(url);
    });
    let (ui, server) = (Arc::clone(&ctx.ui), name.to_owned());
    let redirect_url = Box::new(move |cancel| {
        ui.input(
            &format!("Waiting for sign-in to \"{server}\". If the browser cannot reach this machine, paste the URL it was redirected to."),
            Some("http://127.0.0.1:.../callback?code=..."),
            DialogOptions {
                cancel: Some(cancel),
                ..DialogOptions::default()
            },
        )
    });
    SignInPrompt {
        show_authorization_url,
        redirect_url,
    }
}

/// The sign-in prompt of the `/mcp` manager: its screen for the redirect URL.
fn manager_prompt(ui: &Arc<dyn ExtensionUi>, title: String) -> SignInPrompt {
    let url = Arc::new(Mutex::new(String::new()));
    let shown = Arc::clone(&url);
    let ui = Arc::clone(ui);
    SignInPrompt {
        show_authorization_url: Box::new(move |link: &str| {
            link.clone_into(&mut lock(&shown));
            yapi_ai::auth::open_browser(link);
        }),
        redirect_url: Box::new(move |cancel| {
            let (ui, title, url) = (Arc::clone(&ui), title.clone(), lock(&url).clone());
            Box::pin(async move {
                let (reply, answer) = oneshot::channel();
                ui.mcp_manager(McpScreen::RedirectUrl(title.clone(), url, reply));
                let value = tokio::select! {
                    value = answer => value.ok().flatten(),
                    () = cancel.cancelled() => None,
                };
                ui.mcp_manager(McpScreen::Status(title, "Connecting…".into()));
                value
            })
        }),
    }
}

/// A menu item.
fn item(value: &str, label: &str, description: Option<String>) -> AutocompleteItem {
    AutocompleteItem {
        value: value.to_owned(),
        label: label.to_owned(),
        description,
    }
}

/// pi's `attentionRank`: servers that need the user first.
fn attention_rank(server: &Server) -> u8 {
    if !server.entry.config.is_enabled() {
        return 5;
    }
    match server
        .connection
        .as_ref()
        .map(|connection| connection.state())
    {
        Some(State::NeedsAuth) => 0,
        Some(State::Failed) => 1,
        Some(State::Disconnected) => 2,
        Some(State::Connected) => 4,
        _ => 3,
    }
}

/// What an exposure means, for the tools menu.
fn describe_exposure(exposure: McpExposure) -> &'static str {
    EXPOSURES
        .iter()
        .find(|(known, _)| *known == exposure)
        .map_or("unreachable", |(_, description)| description)
}

/// What `/mcp` shows without servers.
fn no_servers(agent_dir: &Path) -> String {
    format!(
        "No MCP servers configured. Add them to {} or .yapi/mcp.json.",
        agent_dir.join(ConfigFile::Mcp.file_name()).display()
    )
}

impl McpExtension {
    /// Reconnects servers that need a sign-in when their credentials were
    /// stored since, as by `yapi mcp login` in another process.
    async fn reconnect_signed_in(&self, ctx: &Context) {
        let connections: Vec<Arc<Connection>> = lock(&self.shared)
            .servers
            .iter()
            .filter_map(|server| server.connection.clone())
            .collect();
        let mut signed_in = Vec::new();
        for connection in connections {
            if connection.signed_in_elsewhere().await {
                signed_in.push(connection);
            }
        }
        if signed_in.is_empty() {
            return;
        }
        futures_util::future::join_all(signed_in.iter().map(|connection| connection.reconnect()))
            .await;
        self.ensure_discovery_active(ctx);
    }

    fn connection(&self, name: &str) -> Option<Arc<Connection>> {
        lock(&self.shared)
            .servers
            .iter()
            .find(|server| server.entry.name == name)
            .and_then(|server| server.connection.clone())
    }

    async fn command(&self, args: &str, ctx: &Context) {
        let startup = lock(&self.shared).startup.clone();
        Self::wait_for(startup.into_iter().collect(), &ctx.cancel).await;
        let words: Vec<&str> = args.split_whitespace().collect();
        let Some((action, rest)) = words.split_first() else {
            if ctx.mode == Mode::Tui {
                self.manage(ctx).await;
                ctx.ui.mcp_manager(McpScreen::Close);
            } else {
                ctx.ui
                    .notify(&self.format_status(&ctx.agent_dir), NotifyKind::Info);
            }
            return;
        };
        if rest.len() > 1 {
            ctx.ui.notify(USAGE, NotifyKind::Warning);
            return;
        }
        let name = rest.first().copied();
        let none = "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.";
        let oauth = |server: &Server| {
            server.connection.is_some() && server.entry.config.transport.uses_oauth()
        };
        let needs_auth = |server: &Server| {
            server
                .connection
                .as_ref()
                .is_some_and(|connection| connection.snapshot().state == State::NeedsAuth)
        };
        match *action {
            "login" => {
                let Some(name) = self.pick(name, ctx, oauth, needs_auth, none).await else {
                    return;
                };
                if !ctx.ui.has_ui() {
                    ctx.ui.notify(
                        &format!("Signing in to MCP server \"{name}\" requires interactive mode."),
                        NotifyKind::Error,
                    );
                    return;
                }
                if let Some(failure) = self.sign_in(&name, login_prompt(ctx, &name)).await {
                    let kind = if failure == "Sign-in cancelled." {
                        NotifyKind::Info
                    } else {
                        NotifyKind::Error
                    };
                    ctx.ui.notify(&failure, kind);
                    return;
                }
                self.ensure_discovery_active(ctx);
                let tools = self
                    .connection(&name)
                    .map_or(0, |connection| connection.snapshot().tools.len());
                ctx.ui.notify(
                    &format!("Signed in to MCP server \"{name}\" ({tools} tools)."),
                    NotifyKind::Info,
                );
            }
            "logout" => {
                let Some(name) = self.pick(name, ctx, oauth, needs_auth, none).await else {
                    return;
                };
                let removed = match self.sign_out(&name).await {
                    Ok(removed) => removed,
                    Err(error) => {
                        ctx.ui.notify(&error.to_string(), NotifyKind::Error);
                        return;
                    }
                };
                let message = if removed {
                    format!("Signed out of MCP server \"{name}\".")
                } else {
                    format!("No stored credentials for MCP server \"{name}\".")
                };
                ctx.ui.notify(&message, NotifyKind::Info);
            }
            "reconnect" => {
                let failed = |server: &Server| {
                    server.connection.as_ref().is_some_and(|connection| {
                        matches!(
                            connection.snapshot().state,
                            State::Failed | State::Disconnected
                        )
                    })
                };
                let Some(name) = self
                    .pick(
                        name,
                        ctx,
                        |server| server.connection.is_some(),
                        failed,
                        "No enabled MCP server to reconnect.",
                    )
                    .await
                else {
                    return;
                };
                let Some(connection) = self.connection(&name) else {
                    return;
                };
                match connection.reconnect().await {
                    Err(error) => ctx.ui.notify(&error.to_string(), NotifyKind::Error),
                    Ok(()) => {
                        self.ensure_discovery_active(ctx);
                        let state = {
                            let shared = lock(&self.shared);
                            shared
                                .servers
                                .iter()
                                .find(|server| server.entry.name == name)
                                .map(|server| describe_state(server, true))
                                .unwrap_or_default()
                        };
                        ctx.ui.notify(
                            &format!("Reconnected to MCP server \"{name}\" ({state})."),
                            NotifyKind::Info,
                        );
                    }
                }
            }
            _ => ctx.ui.notify(USAGE, NotifyKind::Warning),
        }
    }

    /// pi-mcp's completions of `/mcp` arguments.
    fn argument_completions(&self, prefix: &str) -> Option<Vec<AutocompleteItem>> {
        let words: Vec<&str> = prefix.split_whitespace().collect();
        let trailing = prefix.ends_with(char::is_whitespace);
        let (action, server) = match (words.as_slice(), trailing) {
            ([], _) => ("", None),
            ([action], false) => (*action, None),
            ([action], true) => (*action, Some("")),
            ([action, server], false) => (*action, Some(*server)),
            _ => return None,
        };
        let Some(server) = server else {
            return Some(
                ["login", "logout", "reconnect"]
                    .into_iter()
                    .filter(|item| item.starts_with(action))
                    .map(|item| AutocompleteItem {
                        value: format!("{item} "),
                        label: item.into(),
                        description: None,
                    })
                    .collect(),
            );
        };
        if !matches!(action, "login" | "logout" | "reconnect") {
            return None;
        }
        let shared = lock(&self.shared);
        let items: Vec<AutocompleteItem> = shared
            .servers
            .iter()
            .filter(|candidate| {
                if action == "reconnect" {
                    candidate.connection.is_some()
                } else {
                    candidate.connection.is_some() && candidate.entry.config.transport.uses_oauth()
                }
            })
            .filter(|candidate| candidate.entry.name.starts_with(server))
            .map(|candidate| AutocompleteItem {
                value: format!("{action} {}", candidate.entry.name),
                label: candidate.entry.name.clone(),
                description: Some(describe_state(candidate, true)),
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }
}

impl Extension for McpExtension {
    fn source(&self) -> SourceInfo {
        builtin_source("mcp")
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command {
            name: "mcp".into(),
            description:
                "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure"
                    .into(),
        }]
    }

    fn complete(&self, _command: &str, prefix: &str) -> ArgumentCompletions {
        self.argument_completions(prefix).into()
    }

    fn run_command<'a>(
        &'a self,
        _command: &'a str,
        args: &'a str,
        ctx: &'a Context,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.command(args, ctx))
    }

    // Picks up sign-ins done outside the session, such as `yapi mcp login` run by the agent.
    fn handles(&self, kind: &str) -> bool {
        kind == "turn_start"
    }

    fn handle<'a>(&'a self, ctx: &'a Context, event: &'a Value) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            if event["type"] == "turn_start" {
                self.reconnect_signed_in(ctx).await;
            }
            None
        })
    }

    fn session_start<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let loaded = config::load(&ctx.agent_dir, &ctx.cwd, ctx.project_trusted);
            let generation = {
                let mut shared = lock(&self.shared);
                shared.config_errors = loaded.errors;
                shared.warned_unreachable = false;
                shared.auto_enable_codemode = loaded.auto_enable_codemode.unwrap_or(true);
                shared.waited_for_startup = false;
                shared.startup = None;
                shared.generation += 1;
                shared.tools = Some(ctx.tools.clone());
                shared.servers = loaded
                    .servers
                    .into_iter()
                    .map(|entry| Server {
                        entry,
                        connection: None,
                        ready: None,
                        message: None,
                    })
                    .collect();
                shared.generation
            };
            self.ensure_discovery_active(ctx);
            let enabled: Vec<usize> = lock(&self.shared)
                .servers
                .iter()
                .enumerate()
                .filter(|(_, server)| server.entry.config.is_enabled())
                .map(|(index, _)| index)
                .collect();
            if enabled.is_empty() {
                self.report_problems(ctx.ui.as_ref());
                return;
            }
            let handles: Vec<_> = enabled
                .into_iter()
                .filter_map(|index| self.start(index, ctx, generation))
                .collect();
            let (done, startup) = watch::channel(false);
            lock(&self.shared).startup = Some(startup);
            let this = self.clone();
            let ui = Arc::clone(&ctx.ui);
            tokio::spawn(async move {
                for handle in handles {
                    let _ = handle.await;
                }
                if lock(&this.shared).generation == generation {
                    this.report_problems(ui.as_ref());
                }
                let _ = done.send(true);
            });
        })
    }

    fn before_agent_start<'a>(
        &'a self,
        ctx: &'a Context,
        sections: &'a mut IndexMap<String, String>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let first = !std::mem::replace(&mut lock(&self.shared).waited_for_startup, true);
            if first {
                let waiting = self.pending(has_direct_tools);
                if !waiting.is_empty() {
                    let finished = tokio::time::timeout(
                        STARTUP_WAIT,
                        Self::wait_for(waiting, &tokio_util::sync::CancellationToken::new()),
                    )
                    .await
                    .is_ok();
                    if !finished {
                        ctx.ui.notify(
                            "MCP servers are still connecting; their tools become available once connected.",
                            NotifyKind::Info,
                        );
                    }
                }
            }
            let listing: Vec<(ServerEntry, Option<String>)> = lock(&self.shared)
                .servers
                .iter()
                .map(|server| {
                    let instructions = server
                        .connection
                        .as_ref()
                        .and_then(|connection| connection.snapshot().instructions);
                    (server.entry.clone(), instructions)
                })
                .collect();
            let borrowed: Vec<(&ServerEntry, Option<String>)> = listing
                .iter()
                .map(|(entry, instructions)| (entry, instructions.clone()))
                .collect();
            match render_servers_section(&borrowed) {
                Some(section) => {
                    sections.insert(SERVERS_SECTION.into(), section);
                }
                None => {
                    sections.shift_remove(SERVERS_SECTION);
                }
            }
        })
    }

    fn tool_call<'a>(
        &'a self,
        ctx: &'a Context,
        tool: &'a str,
        _input: &'a Value,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let waits = tool == TOOL_SEARCH_TOOL_NAME
                || [
                    LIST_MCP_RESOURCES_TOOL,
                    LIST_MCP_RESOURCE_TEMPLATES_TOOL,
                    READ_MCP_RESOURCE_TOOL,
                ]
                .contains(&tool);
            if waits {
                let pending = self.pending(|_| true);
                Self::wait_for(pending, &ctx.cancel).await;
            }
            None
        })
    }

    fn session_shutdown<'a>(&'a self, _ctx: &'a Context) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let closing: Vec<Arc<Connection>> = {
                let mut shared = lock(&self.shared);
                shared.generation += 1;
                std::mem::take(&mut shared.servers)
                    .into_iter()
                    .filter_map(|server| server.connection)
                    .collect()
            };
            for connection in closing {
                connection.close().await;
            }
        })
    }
}
