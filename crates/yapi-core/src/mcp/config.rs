//! `mcp.json`: the MCP servers of the agent directory and, for trusted
//! projects, of `<project>/.yapi/mcp.json`, in the `mcpServers` shape other MCP
//! clients share. Project entries replace global ones of the same name. Port
//! of `extensions/mcp/config.ts` and `core/mcp-servers.ts` in pi `v1.0.0`.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use reqwest::Url;
use serde_json::{Map, Value};
use yapi_types::config::ConfigFile;

use crate::config::PROJECT_DIR;

/// How an MCP server's tools reach the model; pi's `McpExposure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpExposure {
    /// Called from codemode scripts. The default.
    Codemode,
    /// Declared once `tool_search` loads them.
    Deferred,
    /// Declared to the model like built-in tools.
    Direct,
    /// Unreachable.
    Hidden,
}

impl McpExposure {
    /// The configuration name.
    pub fn as_str(self) -> &'static str {
        match self {
            McpExposure::Codemode => "codemode",
            McpExposure::Deferred => "deferred",
            McpExposure::Direct => "direct",
            McpExposure::Hidden => "hidden",
        }
    }

    /// Parses a configuration name or its alias.
    fn parse(value: &Value) -> Option<McpExposure> {
        match value.as_str()? {
            "codemode" | "codemode-deferred" => Some(McpExposure::Codemode),
            "deferred" => Some(McpExposure::Deferred),
            "direct" => Some(McpExposure::Direct),
            "hidden" => Some(McpExposure::Hidden),
            _ => None,
        }
    }

    /// The tool exposure: `codemode` tools are deferred tools that codemode
    /// reaches; they differ only in which discovery tool is activated.
    pub fn tool_exposure(self) -> crate::tools::Exposure {
        use crate::tools::Exposure;
        match self {
            McpExposure::Codemode | McpExposure::Deferred => Exposure::Deferred,
            McpExposure::Direct => Exposure::Direct,
            McpExposure::Hidden => Exposure::Hidden,
        }
    }
}

/// How to reach a server.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerTransport {
    /// A process speaking JSON-RPC on stdin and stdout.
    Stdio {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Environment values, which may reference variables or commands.
        env: IndexMap<String, String>,
        /// Working directory, relative to the session's.
        cwd: Option<String>,
    },
    /// A streamable HTTP endpoint.
    Http {
        /// The URL.
        url: String,
        /// Header values, which may reference variables or commands.
        headers: IndexMap<String, String>,
        /// The `/login` provider whose token is sent.
        auth_provider: Option<String>,
    },
}

impl ServerTransport {
    /// Whether the server would sign in with OAuth: HTTP without an
    /// `Authorization` header or provider auth.
    pub fn uses_oauth(&self) -> bool {
        matches!(self, ServerTransport::Http { headers, auth_provider: None, .. }
            if !headers.keys().any(|name| name.eq_ignore_ascii_case("authorization")))
    }
}

/// One validated server entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    /// Transport settings.
    pub transport: ServerTransport,
    /// Default exposure of its tools.
    pub exposure: Option<McpExposure>,
    /// What the server offers, in a sentence.
    pub description: Option<String>,
    /// Exposure of single tools by name or `*` pattern, in file order.
    pub tool_exposure: IndexMap<String, McpExposure>,
    /// `false` keeps the entry without connecting.
    pub enabled: Option<bool>,
    /// Per-request timeout in seconds.
    pub timeout: Option<f64>,
}

impl ServerConfig {
    /// The server's exposure; `codemode` by default.
    pub fn exposure(&self) -> McpExposure {
        self.exposure.unwrap_or(McpExposure::Codemode)
    }

    /// Whether it connects.
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    /// pi's `getMcpToolExposure`: an exact `toolExposure` entry, else the
    /// first matching pattern, else the server's exposure.
    pub fn tool_exposure(&self, tool: &str) -> McpExposure {
        if let Some(exposure) = self.tool_exposure.get(tool) {
            return *exposure;
        }
        for (pattern, exposure) in &self.tool_exposure {
            if pattern.contains('*') && glob(pattern, tool) {
                return *exposure;
            }
        }
        self.exposure()
    }

    /// Every exposure its tools can have, known before it connects.
    pub fn exposures(&self) -> Vec<McpExposure> {
        let mut exposures = vec![self.exposure()];
        for exposure in self.tool_exposure.values() {
            if !exposures.contains(exposure) {
                exposures.push(*exposure);
            }
        }
        exposures
    }
}

/// pi's `toolPatternRegExp`: `*` matches any characters.
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<String> = pattern.split('*').map(regex_lite::escape).collect();
    regex_lite::Regex::new(&format!("^{}$", parts.join(".*")))
        .is_ok_and(|regex| regex.is_match(text))
}

/// Which file defined a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The agent directory's `mcp.json`.
    Global,
    /// The project's `mcp.json`.
    Project,
}

/// A configured server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerEntry {
    /// Its name.
    pub name: String,
    /// Its settings.
    pub config: ServerConfig,
    /// The file that defined it.
    pub source: PathBuf,
    /// Which file that is.
    pub scope: Scope,
}

/// Everything `mcp.json` files configure, and their problems.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedConfig {
    /// Servers in definition order.
    pub servers: Vec<ServerEntry>,
    /// Whether codemode is activated for `codemode` servers.
    pub auto_enable_codemode: Option<bool>,
    /// Problems, each prefixed with its file.
    pub errors: Vec<String>,
}

/// `mcp__<server>` with `-` as `_`, the prefix of the server's tool names.
pub fn namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// The host as JavaScript's `URL.hostname` gives it, IPv6 in brackets.
fn host(url: &Url) -> String {
    url.host_str().unwrap_or_default().to_owned()
}

fn string_map(value: &Value) -> Option<IndexMap<String, String>> {
    value
        .as_object()?
        .iter()
        .map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

fn validate_oauth(value: &Value) -> Option<String> {
    let Some(oauth) = value.as_object() else {
        return Some("oauth must be an object".into());
    };
    let string = |key: &str| oauth.get(key).is_none_or(Value::is_string);
    if !string("clientId") {
        return Some("oauth.clientId must be a string".into());
    }
    if !string("clientSecret") {
        return Some("oauth.clientSecret must be a string".into());
    }
    let port = oauth.get("callbackPort");
    if let Some(port) = port
        && !port
            .as_f64()
            .is_some_and(|port| port.fract() == 0.0 && (1.0..=65535.0).contains(&port))
    {
        return Some("oauth.callbackPort must be a port number".into());
    }
    if let Some(callback) = oauth.get("callbackUrl") {
        let url = callback.as_str().and_then(|text| Url::parse(text).ok());
        let loopback = url.as_ref().is_some_and(|url| {
            url.scheme() == "http"
                && LOOPBACK_HOSTS.contains(&host(url).as_str())
                && url.query().is_none()
                && url.fragment().is_none()
        });
        let Some(url) = url.filter(|_| loopback) else {
            return Some("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment".into());
        };
        if let (Some(url_port), Some(port)) = (url.port(), port.and_then(Value::as_f64))
            && f64::from(url_port) != port
        {
            return Some("oauth.callbackUrl and oauth.callbackPort name different ports".into());
        }
    }
    if !string("scope") {
        return Some("oauth.scope must be a string".into());
    }
    if let Some(name) = oauth.get("clientName")
        && !name.as_str().is_some_and(|name| !name.trim().is_empty())
    {
        return Some("oauth.clientName must be a non-empty string".into());
    }
    if let Some(metadata) = oauth.get("authServerMetadataUrl") {
        let url = metadata.as_str().and_then(|text| Url::parse(text).ok());
        let allowed = url.is_some_and(|url| {
            url.scheme() == "https"
                || (url.scheme() == "http" && LOOPBACK_HOSTS.contains(&host(&url).as_str()))
        });
        if !allowed {
            return Some("oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]".into());
        }
    }
    None
}

/// pi's `validateMcpServerConfig`: the entry, or an error message.
pub fn validate_server(name: &str, raw: &Value) -> Result<ServerConfig, String> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_name {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(value) = raw.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    let exposures = "\"codemode\", \"deferred\", \"direct\", \"hidden\"";
    let exposure = match value.get("exposure") {
        None => None,
        Some(raw) => Some(
            McpExposure::parse(raw)
                .ok_or_else(|| format!("server \"{name}\": exposure must be one of {exposures}"))?,
        ),
    };
    let mut tool_exposure = IndexMap::new();
    if let Some(raw) = value.get("toolExposure") {
        let Some(entries) = raw.as_object() else {
            return Err(format!(
                "server \"{name}\": toolExposure must map tool names to exposures"
            ));
        };
        for (tool, entry) in entries {
            let exposure = McpExposure::parse(entry).ok_or_else(|| {
                format!("server \"{name}\": toolExposure \"{tool}\" must be one of {exposures}")
            })?;
            tool_exposure.insert(tool.clone(), exposure);
        }
    }
    let enabled = match value.get("enabled") {
        None => None,
        Some(Value::Bool(enabled)) => Some(*enabled),
        Some(_) => return Err(format!("server \"{name}\": enabled must be a boolean")),
    };
    let description = match value.get("description") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(format!("server \"{name}\": description must be a string")),
    };
    let timeout = match value.get("timeout") {
        None => None,
        Some(raw) => match raw.as_f64() {
            Some(seconds) if seconds > 0.0 => Some(seconds),
            _ => {
                return Err(format!(
                    "server \"{name}\": timeout must be a positive number of seconds"
                ));
            }
        },
    };
    let kind = value.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return Err(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    let base = |transport| ServerConfig {
        transport,
        exposure,
        description: description.clone(),
        tool_exposure: tool_exposure.clone(),
        enabled,
        timeout,
    };
    let kind_is = |names: &[&str]| {
        value.get("type").is_none() || kind.is_some_and(|kind| names.contains(&kind))
    };
    if let Some(url) = value.get("url").and_then(Value::as_str)
        && kind_is(&["http", "streamable-http"])
    {
        let parsed = Url::parse(url)
            .ok()
            .filter(|url| url.scheme() == "http" || url.scheme() == "https")
            .ok_or_else(|| format!("server \"{name}\": url must be an http or https URL"))?;
        let headers = match value.get("headers") {
            None => IndexMap::new(),
            Some(raw) => string_map(raw)
                .ok_or_else(|| format!("server \"{name}\": headers must map names to strings"))?,
        };
        if let Some(oauth) = value.get("oauth")
            && let Some(problem) = validate_oauth(oauth)
        {
            return Err(format!("server \"{name}\": {problem}"));
        }
        let auth_provider = match value.get("auth") {
            None => None,
            Some(auth) => {
                let provider = auth
                    .get("provider")
                    .and_then(Value::as_str)
                    .filter(|provider| !provider.is_empty())
                    .ok_or_else(|| {
                        format!("server \"{name}\": auth.provider must be a provider name")
                    })?;
                if parsed.scheme() != "https" && !LOOPBACK_HOSTS.contains(&host(&parsed).as_str()) {
                    return Err(format!(
                        "server \"{name}\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
                    ));
                }
                Some(provider.to_owned())
            }
        };
        return Ok(base(ServerTransport::Http {
            url: url.to_owned(),
            headers,
            auth_provider,
        }));
    }
    if let Some(command) = value.get("command").and_then(Value::as_str)
        && kind_is(&["stdio"])
    {
        let args = match value.get("args") {
            None => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            Some(_) => {
                return Err(format!(
                    "server \"{name}\": args must be an array of strings"
                ));
            }
        };
        let env = match value.get("env") {
            None => IndexMap::new(),
            Some(raw) => string_map(raw)
                .ok_or_else(|| format!("server \"{name}\": env must map names to strings"))?,
        };
        let cwd = match value.get("cwd") {
            None => None,
            Some(Value::String(cwd)) => Some(cwd.clone()),
            Some(_) => return Err(format!("server \"{name}\": cwd must be a string")),
        };
        return Ok(base(ServerTransport::Stdio {
            command: command.to_owned(),
            args,
            env,
            cwd,
        }));
    }
    Err(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

fn read_file(path: &Path, scope: Scope, loaded: &mut LoadedConfig) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let parsed: Map<String, Value> = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(parsed)) if parsed.get("mcpServers").is_none_or(Value::is_object) => {
            parsed
        }
        Ok(_) => {
            loaded.errors.push(format!(
                "{}: expected an object with an \"mcpServers\" object",
                path.display()
            ));
            return;
        }
        Err(error) => {
            loaded.errors.push(format!("{}: {error}", path.display()));
            return;
        }
    };
    match parsed.get("autoEnableCodemode") {
        None => {}
        Some(Value::Bool(value)) => loaded.auto_enable_codemode = Some(*value),
        Some(_) => loaded.errors.push(format!(
            "{}: autoEnableCodemode must be a boolean",
            path.display()
        )),
    }
    let empty = Map::new();
    let servers = parsed
        .get("mcpServers")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (name, raw) in servers {
        let config = match validate_server(name, raw) {
            Ok(config) => config,
            Err(problem) => {
                loaded.errors.push(format!("{}: {problem}", path.display()));
                continue;
            }
        };
        // Names that differ only in `-` and `_` would share a namespace.
        if let Some(clash) = loaded
            .servers
            .iter()
            .find(|other| other.name != *name && namespace(&other.name) == namespace(name))
        {
            loaded.errors.push(format!(
                "{}: server \"{name}\" conflicts with \"{}\"",
                path.display(),
                clash.name
            ));
            continue;
        }
        if scope == Scope::Project
            && matches!(
                config.transport,
                ServerTransport::Http {
                    auth_provider: Some(_),
                    ..
                }
            )
        {
            loaded.errors.push(format!(
                "{}: server \"{name}\": auth is only allowed in the global mcp.json",
                path.display()
            ));
            continue;
        }
        let entry = ServerEntry {
            name: name.clone(),
            config,
            source: path.to_path_buf(),
            scope,
        };
        match loaded.servers.iter_mut().find(|other| other.name == *name) {
            Some(existing) => *existing = entry,
            None => loaded.servers.push(entry),
        }
    }
}

/// Loads the global and, when trusted, the project `mcp.json`. Disabled
/// servers are included.
pub fn load(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedConfig {
    let mut loaded = LoadedConfig::default();
    let name = ConfigFile::Mcp.file_name();
    read_file(&agent_dir.join(name), Scope::Global, &mut loaded);
    if project_trusted {
        read_file(
            &cwd.join(PROJECT_DIR).join(name),
            Scope::Project,
            &mut loaded,
        );
    }
    loaded
}

/// pi's `resolveExposureAliases`: `raw` with exposure aliases, in
/// `exposure` and `toolExposure`, replaced by their current names.
pub fn resolve_exposure_aliases(raw: &Value) -> Value {
    let canonical = |value: &Value| match McpExposure::parse(value) {
        Some(exposure) => Value::String(exposure.as_str().to_owned()),
        None => value.clone(),
    };
    let mut resolved = raw.clone();
    if let Some(object) = resolved.as_object_mut() {
        if let Some(exposure) = object.get_mut("exposure") {
            *exposure = canonical(exposure);
        }
        if let Some(Value::Object(tools)) = object.get_mut("toolExposure") {
            for value in tools.values_mut() {
                *value = canonical(value);
            }
        }
    }
    resolved
}

/// pi's `addMcpServerConfig`: adds `config` as server `name` to the
/// `mcp.json` at `path`, creating the file when missing. Returns whether an
/// entry of that name was replaced.
pub fn add_server_config(path: &Path, name: &str, config: Value) -> Result<bool, String> {
    let mut replaced = false;
    edit_servers(path, |document| {
        let servers = document
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(servers) = servers {
            replaced = servers.insert(name.to_owned(), config).is_some();
        }
        true
    })?;
    Ok(replaced)
}

/// pi's `removeMcpServerConfig`: removes server `name` from the `mcp.json`
/// at `path`. Returns false when the file does not define it.
pub fn remove_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_servers(path, |document| {
        removed = document
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .is_some_and(|servers| servers.shift_remove(name).is_some());
        removed
    })?;
    Ok(removed)
}

/// Reads an `mcp.json` (empty when missing), lets `edit` change it, and
/// writes it back with its own indentation when `edit` returns true. Other
/// content is kept.
fn edit_servers(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path).ok();
    let parsed: Value = match &text {
        Some(text) => serde_json::from_str(text).map_err(|err| err.to_string())?,
        None => Value::Object(Map::new()),
    };
    let Value::Object(mut document) = parsed else {
        return Err(format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
    };
    if document
        .get("mcpServers")
        .is_some_and(|servers| !servers.is_object())
    {
        return Err(format!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
    }
    if !edit(&mut document) {
        return Ok(());
    }
    // The first indented line's indentation, as pi detects it.
    let indent = text
        .as_deref()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.trim_start_matches([' ', '\t']);
                (rest.len() < line.len()
                    && !rest.is_empty()
                    && !rest.starts_with(char::is_whitespace))
                .then(|| &line[..line.len() - rest.len()])
            })
        })
        .unwrap_or("  ");
    let json = yapi_types::json::to_string_pretty(&Value::Object(document), indent)
        .map_err(|err| err.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    std::fs::write(path, format!("{json}\n")).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_like_pi() {
        let stdio = validate_server(
            "fs",
            &json!({"command": "npx", "args": ["-y", "x"], "exposure": "codemode-deferred"}),
        )
        .unwrap();
        assert_eq!(stdio.exposure, Some(McpExposure::Codemode));
        let problems = [
            (
                json!({"command": 1}),
                "server \"x\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)",
            ),
            (
                json!({"url": "ftp://a"}),
                "server \"x\": url must be an http or https URL",
            ),
            (
                json!({"url": "https://a", "type": "sse"}),
                "server \"x\": legacy SSE transport is not supported; use the streamable HTTP URL",
            ),
            (
                json!({"command": "a", "exposure": "all"}),
                "server \"x\": exposure must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\"",
            ),
            (
                json!({"command": "a", "timeout": 0}),
                "server \"x\": timeout must be a positive number of seconds",
            ),
            (
                json!({"url": "http://example.com", "auth": {"provider": "github"}}),
                "server \"x\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]",
            ),
            (
                json!({"url": "https://a", "oauth": {"callbackUrl": "http://localhost:1/cb", "callbackPort": 2}}),
                "server \"x\": oauth.callbackUrl and oauth.callbackPort name different ports",
            ),
        ];
        for (raw, message) in problems {
            assert_eq!(validate_server("x", &raw).unwrap_err(), message);
        }
        assert_eq!(
            validate_server("a b", &json!({})).unwrap_err(),
            "invalid server name \"a b\" (use letters, digits, \"_\" and \"-\")"
        );
        assert!(
            validate_server(
                "x",
                &json!({"url": "http://[::1]:8080/mcp", "auth": {"provider": "p"}})
            )
            .is_ok()
        );
    }

    #[test]
    fn tool_exposure_prefers_exact_names() {
        let config = validate_server(
            "gh",
            &json!({"command": "a", "exposure": "hidden", "toolExposure": {"list_*": "direct", "list_issues": "deferred", "*_pr": "codemode"}}),
        )
        .unwrap();
        assert_eq!(config.tool_exposure("list_issues"), McpExposure::Deferred);
        assert_eq!(config.tool_exposure("list_repos"), McpExposure::Direct);
        assert_eq!(config.tool_exposure("merge_pr"), McpExposure::Codemode);
        assert_eq!(config.tool_exposure("delete"), McpExposure::Hidden);
        assert_eq!(namespace("my-server"), "mcp__my_server");
    }

    #[test]
    fn edits_servers_keeping_indentation() {
        let dir = std::env::temp_dir().join(format!("yapi-mcp-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mcp.json");
        assert!(!remove_server_config(&path, "x").unwrap());
        assert!(!add_server_config(&path, "a", json!({"command": "a"})).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"mcpServers\": {\n    \"a\": {\n      \"command\": \"a\"\n    }\n  }\n}\n"
        );
        std::fs::write(
            &path,
            "{\n\t\"other\": 1,\n\t\"mcpServers\": {\"a\": {\"url\": \"https://x\"}}\n}",
        )
        .unwrap();
        assert!(add_server_config(&path, "a", json!({"command": "b"})).unwrap());
        assert!(remove_server_config(&path, "a").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n\t\"other\": 1,\n\t\"mcpServers\": {}\n}\n"
        );
        assert_eq!(
            resolve_exposure_aliases(
                &json!({"exposure": "codemode-deferred", "toolExposure": {"t": "codemode-deferred"}})
            ),
            json!({"exposure": "codemode", "toolExposure": {"t": "codemode"}})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
