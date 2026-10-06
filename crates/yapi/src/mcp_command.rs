//! `yapi mcp`: add, remove and check MCP servers outside a session.
//!
//! Port of `packages/coding-agent/src/extensions/mcp/cli.ts` in pi `v1.0.0`.
//! OAuth sign-in for MCP servers is not available in yapi yet, so `login` and
//! `logout` report that once they have found the server.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use yapi_core::config::{APP_NAME, PROJECT_DIR, agent_dir};
use yapi_core::mcp::ResourceKind;
use yapi_core::mcp::config::{
    LoadedConfig, Scope, ServerEntry, ServerTransport, add_server_config, load,
    remove_server_config, resolve_exposure_aliases, validate_server,
};
use yapi_core::mcp::connection::{Connection, State};
use yapi_core::trust::TrustStore;

use crate::{err, out};

fn help(bold: bool) -> String {
    let usage = if bold {
        "\x1b[1mUsage:\x1b[22m"
    } else {
        "Usage:"
    };
    format!(
        r#"{usage}
  {APP_NAME} mcp add <server> [options] -- <command> [args...]
  {APP_NAME} mcp add <server> [options] --url <url>
  {APP_NAME} mcp remove <server> [-l]
  {APP_NAME} mcp list [--json]
  {APP_NAME} mcp login <server> [--timeout <seconds>]
  {APP_NAME} mcp logout <server>

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/{PROJECT_DIR}/agent/mcp.json and, in trusted projects, {PROJECT_DIR}/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add and remove:
  -l, --local             Use {PROJECT_DIR}/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send "Authorization: Bearer ${{NAME}}"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${{NAME}} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     How long login waits for the browser (default: 300)"#
    )
}

/// pi's dimmed pointer to the help, plain when stderr is not a terminal.
fn help_hint() -> String {
    let text = format!("Use \"{APP_NAME} mcp --help\" for usage.");
    if std::io::stderr().is_terminal() {
        format!("\x1b[2m{text}\x1b[22m")
    } else {
        text
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Flag,
    Value,
    List,
}

#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    values: HashMap<String, Option<String>>,
    lists: HashMap<String, Vec<String>>,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(Option::as_deref)
    }

    fn has(&self, name: &str) -> bool {
        self.values.contains_key(name) || self.lists.contains_key(name)
    }
}

/// pi's `parseOptions`: `--` ends the options, as does reaching
/// `max_positionals` positional arguments, so a server command's own options
/// pass through.
fn parse_options(
    args: &[String],
    known: &[(&str, Kind)],
    max_positionals: usize,
) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = match args[index].as_str() {
            "-l" => "--local",
            other => other,
        };
        if arg == "--" || parsed.positional.len() >= max_positionals {
            let from = if arg == "--" { index + 1 } else { index };
            parsed.positional.extend(args[from..].iter().cloned());
            break;
        }
        let Some(name) = arg.strip_prefix("--") else {
            parsed.positional.push(arg.to_owned());
            index += 1;
            continue;
        };
        let Some((_, kind)) = known.iter().find(|(known, _)| *known == name) else {
            return Err(format!("Unknown option {arg}.\n{}", help_hint()));
        };
        if *kind == Kind::Flag {
            parsed.values.insert(name.to_owned(), None);
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("{arg} needs a value."));
        };
        if *kind == Kind::List {
            parsed
                .lists
                .entry(name.to_owned())
                .or_default()
                .push(value.clone());
        } else {
            parsed.values.insert(name.to_owned(), Some(value.clone()));
        }
        index += 2;
    }
    Ok(parsed)
}

/// Runs `yapi mcp <args>` and returns the exit code.
pub async fn run(args: &[String]) -> u8 {
    let Some(command) = args.first().map(String::as_str) else {
        out(&help(std::io::stdout().is_terminal()));
        return 0;
    };
    if command == "help" || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        out(&help(std::io::stdout().is_terminal()));
        return 0;
    }
    let rest = &args[1..];
    let Ok(cwd) = std::env::current_dir() else {
        err("Error: cannot read the working directory");
        return 1;
    };
    let agent_dir = agent_dir();
    let project_config = cwd.join(PROJECT_DIR).join("mcp.json");
    match command {
        "add" => return add(rest, &project_config, &cwd, &agent_dir),
        "remove" => return remove(rest, &project_config, &cwd, &agent_dir),
        _ => {}
    }
    let trusted = TrustStore::new(&agent_dir).get(&cwd) == Some(true);
    let loaded = load(&agent_dir, &cwd, trusted);
    let untrusted_note = (!trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start {APP_NAME} in the project to trust it.",
            project_config.display()
        )
    });
    match command {
        "list" => {
            let parsed = match parse_options(rest, &[("json", Kind::Flag)], usize::MAX) {
                Ok(parsed) => parsed,
                Err(message) => {
                    err(&message);
                    return 1;
                }
            };
            if !parsed.positional.is_empty() {
                err(&format!(
                    "Usage: {APP_NAME} mcp list [--json]\n{}",
                    help_hint()
                ));
                return 1;
            }
            list(
                &loaded,
                parsed.has("json"),
                untrusted_note.as_deref(),
                &cwd,
                &agent_dir,
            )
            .await
        }
        "login" | "logout" => {
            let known: &[(&str, Kind)] = if command == "login" {
                &[("timeout", Kind::Value)]
            } else {
                &[]
            };
            let parsed = match parse_options(rest, known, usize::MAX) {
                Ok(parsed) => parsed,
                Err(message) => {
                    err(&message);
                    return 1;
                }
            };
            let [name] = parsed.positional.as_slice() else {
                err(&format!(
                    "Usage: {APP_NAME} mcp {command} <server>\n{}",
                    help_hint()
                ));
                return 1;
            };
            let Some(entry) = loaded.servers.iter().find(|server| server.name == *name) else {
                let names: Vec<&str> = loaded
                    .servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect();
                let note = untrusted_note
                    .as_deref()
                    .map(|note| format!(" {note}"))
                    .unwrap_or_default();
                let configured = if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                };
                err(&format!(
                    "No MCP server named \"{name}\".{note} Configured: {configured}."
                ));
                return 1;
            };
            if !entry.config.transport.uses_oauth() {
                err(&format!(
                    "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
                ));
                return 1;
            }
            err(&format!(
                "Signing in to MCP servers is not available in {APP_NAME} yet; \"{name}\" cannot be signed in to or out of."
            ));
            1
        }
        _ => {
            err(&format!(
                "Unknown mcp command \"{command}\".\n{}",
                help_hint()
            ));
            1
        }
    }
}

/// `KEY=VALUE` pairs of a repeatable option, in order.
fn pairs(option: &str, values: Option<&Vec<String>>) -> Result<Map<String, Value>, String> {
    let mut record = Map::new();
    for pair in values.into_iter().flatten() {
        match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                record.insert(key.to_owned(), Value::String(value.to_owned()));
            }
            _ => return Err(format!("--{option} expects KEY=VALUE, got \"{pair}\".")),
        }
    }
    Ok(record)
}

fn add(args: &[String], project_config: &Path, cwd: &Path, agent_dir: &Path) -> u8 {
    let usage = format!(
        "Usage: {APP_NAME} mcp add <server> [options] (--url <url> | -- <command> [args...])\n{}",
        help_hint()
    );
    let known = [
        ("local", Kind::Flag),
        ("url", Kind::Value),
        ("env", Kind::List),
        ("cwd", Kind::Value),
        ("header", Kind::List),
        ("bearer-token-env-var", Kind::Value),
        ("oauth-client-id", Kind::Value),
        ("oauth-client-secret", Kind::Value),
        ("oauth-callback-port", Kind::Value),
        ("oauth-client-name", Kind::Value),
        ("exposure", Kind::Value),
        ("description", Kind::Value),
    ];
    let parsed = match parse_options(args, &known, 2) {
        Ok(parsed) => parsed,
        Err(message) => {
            err(&message);
            return 1;
        }
    };
    let Some((name, command)) = parsed.positional.split_first() else {
        err(&usage);
        return 1;
    };
    let url = parsed.value("url");
    if url.is_none() == command.is_empty() {
        err(&usage);
        return 1;
    }
    let http_only = [
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
    ];
    let stdio_only = ["env", "cwd"];
    let misplaced = if url.is_none() {
        &http_only[..]
    } else {
        &stdio_only[..]
    }
    .iter()
    .find(|option| parsed.has(option));
    if let Some(option) = misplaced {
        let kind = if url.is_none() {
            "HTTP servers (--url)"
        } else {
            "stdio servers"
        };
        err(&format!("--{option} only applies to {kind}."));
        return 1;
    }

    let mut config = Map::new();
    if let Some(url) = url {
        let mut headers = match pairs("header", parsed.lists.get("header")) {
            Ok(headers) => headers,
            Err(message) => {
                err(&message);
                return 1;
            }
        };
        if let Some(variable) = parsed.value("bearer-token-env-var") {
            headers.insert(
                "Authorization".into(),
                Value::String(format!("Bearer ${{{variable}}}")),
            );
        }
        let mut oauth = Map::new();
        if let Some(id) = parsed.value("oauth-client-id") {
            oauth.insert("clientId".into(), json!(id));
        }
        if let Some(secret) = parsed.value("oauth-client-secret") {
            oauth.insert("clientSecret".into(), json!(secret));
        }
        if let Some(port) = parsed.value("oauth-callback-port") {
            oauth.insert("callbackPort".into(), js_number(port));
        }
        if let Some(client_name) = parsed.value("oauth-client-name") {
            oauth.insert("clientName".into(), json!(client_name));
        }
        config.insert("url".into(), json!(url));
        if !headers.is_empty() {
            config.insert("headers".into(), Value::Object(headers));
        }
        if !oauth.is_empty() {
            config.insert("oauth".into(), Value::Object(oauth));
        }
    } else {
        let env = match pairs("env", parsed.lists.get("env")) {
            Ok(env) => env,
            Err(message) => {
                err(&message);
                return 1;
            }
        };
        config.insert("command".into(), json!(command[0]));
        if command.len() > 1 {
            config.insert("args".into(), json!(command[1..]));
        }
        if !env.is_empty() {
            config.insert("env".into(), Value::Object(env));
        }
        if let Some(dir) = parsed.value("cwd") {
            config.insert("cwd".into(), json!(dir));
        }
    }
    if let Some(exposure) = parsed.value("exposure") {
        config.insert("exposure".into(), json!(exposure));
    }
    if let Some(description) = parsed.value("description") {
        config.insert("description".into(), json!(description));
    }
    let config = resolve_exposure_aliases(&Value::Object(config));
    let validated = match validate_server(name, &config) {
        Ok(validated) => validated,
        Err(message) => {
            err(&message);
            return 1;
        }
    };

    let project = parsed.has("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    let replaced = match add_server_config(&path, name, config) {
        Ok(replaced) => replaced,
        Err(message) => {
            err(&format!("Could not update {}: {message}", path.display()));
            return 1;
        }
    };
    let verb = if replaced { "Replaced" } else { "Added" };
    out(&format!(
        "{verb} {scope} MCP server \"{name}\" in {}.",
        path.display()
    ));
    if project && TrustStore::new(agent_dir).get(cwd) != Some(true) {
        out(&format!(
            "The project is not trusted, so {} is ignored until you start {APP_NAME} in the project and trust it.",
            path.display()
        ));
    }
    // HTTP servers without an Authorization header may use OAuth.
    let may_need_sign_in = matches!(
        &validated.transport,
        ServerTransport::Http { headers, .. }
            if !headers.keys().any(|header| header.eq_ignore_ascii_case("authorization"))
    );
    let sign_in = if may_need_sign_in {
        format!(". If it requires sign-in: {APP_NAME} mcp login {name}")
    } else {
        String::new()
    };
    out(&format!("Check it with: {APP_NAME} mcp list{sign_in}"));
    0
}

/// `Number(text)` as JSON: an integer when it is one, else a float, else
/// `null` for `NaN`.
fn js_number(text: &str) -> Value {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return json!(0);
    }
    // `0x`, `0o` and `0b` integers, without a sign.
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = trimmed
            .get(..2)
            .filter(|start| start.eq_ignore_ascii_case(prefix))
            .and_then(|_| trimmed.get(2..))
        {
            let value = digits.chars().try_fold(0.0_f64, |value, c| {
                c.to_digit(radix)
                    .map(|digit| value * f64::from(radix) + f64::from(digit))
            });
            return match value {
                Some(value) if !digits.is_empty() => js_integer(value),
                _ => Value::Null,
            };
        }
    }
    match trimmed.parse::<f64>() {
        Ok(number) if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 => {
            json!(number as i64)
        }
        Ok(number) if number.is_finite() => json!(number),
        _ => Value::Null,
    }
}

fn js_integer(value: f64) -> Value {
    if value < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn remove(args: &[String], project_config: &Path, cwd: &Path, agent_dir: &Path) -> u8 {
    let parsed = match parse_options(args, &[("local", Kind::Flag)], usize::MAX) {
        Ok(parsed) => parsed,
        Err(message) => {
            err(&message);
            return 1;
        }
    };
    let [name] = parsed.positional.as_slice() else {
        err(&format!(
            "Usage: {APP_NAME} mcp remove <server> [-l]\n{}",
            help_hint()
        ));
        return 1;
    };
    let project = parsed.has("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        agent_dir.join("mcp.json")
    };
    let (scope, scope_name) = if project {
        (Scope::Project, "project")
    } else {
        (Scope::Global, "global")
    };
    match remove_server_config(&path, name) {
        Ok(true) => {
            out(&format!(
                "Removed {scope_name} MCP server \"{name}\" from {}.",
                path.display()
            ));
            0
        }
        Ok(false) => {
            let other = load(agent_dir, cwd, true)
                .servers
                .into_iter()
                .find(|server| server.name == *name && server.scope != scope);
            let hint = other
                .map(|other| {
                    let flag = if other.scope == Scope::Project {
                        "use --local"
                    } else {
                        "omit --local"
                    };
                    format!(" It is defined in {}; {flag}.", other.source.display())
                })
                .unwrap_or_default();
            err(&format!(
                "No {scope_name} MCP server named \"{name}\" in {}.{hint}",
                path.display()
            ));
            1
        }
        Err(message) => {
            err(&format!("Could not update {}: {message}", path.display()));
            1
        }
    }
}

/// What `list` reports for one server.
struct Report {
    name: String,
    scope: &'static str,
    source: String,
    enabled: bool,
    exposure: &'static str,
    transport: String,
    state: &'static str,
    tools: Vec<String>,
    tool_exposure: Vec<(String, &'static str)>,
    resources: Option<(usize, usize)>,
    error: Option<String>,
}

fn describe_transport(entry: &ServerEntry) -> String {
    match &entry.config.transport {
        ServerTransport::Http { url, .. } => url.clone(),
        ServerTransport::Stdio { command, args, .. } => std::iter::once(command)
            .chain(args)
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    }
}

async fn report(entry: ServerEntry, cwd: &Path, agent_dir: &Path) -> Report {
    let mut report = Report {
        name: entry.name.clone(),
        scope: match entry.scope {
            Scope::Global => "global",
            Scope::Project => "project",
        },
        source: entry.source.display().to_string(),
        enabled: entry.config.is_enabled(),
        exposure: entry.config.exposure().as_str(),
        transport: describe_transport(&entry),
        state: "disabled",
        tools: Vec::new(),
        tool_exposure: Vec::new(),
        resources: None,
        error: None,
    };
    if !report.enabled {
        return report;
    }
    let config = entry.config.clone();
    let connection = Connection::new(
        entry,
        cwd.to_path_buf(),
        agent_dir.to_path_buf(),
        Arc::new(|_: &Arc<Connection>| {}),
        Arc::new(|_: &str| false),
    );
    let connected = connection.client().await.is_ok();
    let snapshot = connection.snapshot();
    report.state = snapshot.state.as_str();
    report.tools = snapshot
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    report.tool_exposure = report
        .tools
        .iter()
        .filter_map(|tool| {
            let exposure = config.tool_exposure(tool).as_str();
            (exposure != report.exposure).then(|| (tool.clone(), exposure))
        })
        .collect();
    if connected && snapshot.has_resources {
        let templates = connection
            .all_resources(ResourceKind::Templates, Default::default())
            .await
            .map(|templates| templates.len())
            .unwrap_or(0);
        report.resources = Some((snapshot.resources.len(), templates));
    }
    if snapshot.state != State::Connected {
        report.error = snapshot.error;
    }
    connection.close().await;
    report
}

async fn list(
    loaded: &LoadedConfig,
    json: bool,
    untrusted_note: Option<&str>,
    cwd: &Path,
    agent_dir: &Path,
) -> u8 {
    let reports = futures_util::future::join_all(
        loaded
            .servers
            .iter()
            .cloned()
            .map(|entry| report(entry, cwd, agent_dir)),
    )
    .await;
    let failed = !loaded.errors.is_empty()
        || reports
            .iter()
            .any(|report| report.enabled && report.state != "connected");
    let code = u8::from(failed);

    if json {
        let servers: Vec<Value> = reports
            .iter()
            .map(|report| {
                let mut server = json!({
                    "name": report.name,
                    "scope": report.scope,
                    "source": report.source,
                    "enabled": report.enabled,
                    "exposure": report.exposure,
                    "transport": report.transport,
                    "state": report.state,
                    "tools": report.tools,
                });
                if !report.tool_exposure.is_empty() {
                    let overrides: Map<String, Value> = report
                        .tool_exposure
                        .iter()
                        .map(|(tool, exposure)| (tool.clone(), json!(exposure)))
                        .collect();
                    server["toolExposure"] = Value::Object(overrides);
                }
                if let Some((resources, templates)) = report.resources {
                    server["resources"] = json!(resources);
                    server["resourceTemplates"] = json!(templates);
                }
                if let Some(error) = &report.error {
                    server["error"] = json!(error);
                }
                server
            })
            .collect();
        let mut document = json!({"servers": servers, "errors": loaded.errors});
        if let Some(note) = untrusted_note {
            document["note"] = json!(note);
        }
        out(&yapi_types::json::to_string_pretty(&document, "  ").unwrap_or_default());
        return code;
    }
    if reports.is_empty() && loaded.errors.is_empty() {
        out(&format!(
            "No MCP servers configured. Add them to {} or {PROJECT_DIR}/mcp.json.",
            agent_dir.join("mcp.json").display()
        ));
    }
    for report in &reports {
        let state = match report.state {
            "connected" => {
                let count = report.tools.len();
                let plural = if count == 1 { "" } else { "s" };
                format!("connected, {count} tool{plural}")
            }
            "needs-auth" => "needs sign-in".to_owned(),
            other => other.to_owned(),
        };
        out(&format!(
            "{}: {state} ({}, {})",
            report.name, report.exposure, report.scope
        ));
        out(&format!("  {}", report.transport));
        if report.state == "needs-auth" {
            out(&format!(
                "  sign in with: {APP_NAME} mcp login {}",
                report.name
            ));
        }
        if !report.tools.is_empty() {
            let tools: Vec<String> = report
                .tools
                .iter()
                .map(
                    |tool| match report.tool_exposure.iter().find(|(name, _)| name == tool) {
                        Some((_, exposure)) => format!("{tool} [{exposure}]"),
                        None => tool.clone(),
                    },
                )
                .collect();
            out(&format!("  tools: {}", tools.join(", ")));
        }
        if let Some((resources, templates)) = report.resources {
            out(&format!(
                "  resources: {resources}, URI templates: {templates}"
            ));
        }
        if let Some(error) = &report.error {
            out(&format!("  {}", error.replace('\n', "\n  ")));
        }
    }
    for error in &loaded.errors {
        out(&format!("config error: {error}"));
    }
    if let Some(note) = untrusted_note {
        out(note);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn options_stop_at_the_server_command() {
        let known = [("local", Kind::Flag), ("env", Kind::List)];
        let parsed = parse_options(
            &strings(&["-l", "x", "--env", "A=1", "npx", "--yes", "pkg"]),
            &known,
            2,
        )
        .unwrap_or_default();
        assert!(parsed.has("local"));
        assert_eq!(parsed.positional, strings(&["x", "npx", "--yes", "pkg"]));
        assert_eq!(parsed.lists["env"], strings(&["A=1"]));
        assert!(
            parse_options(&strings(&["--bogus"]), &known, 2)
                .err()
                .is_some_and(|message| message.starts_with("Unknown option --bogus."))
        );
        assert_eq!(
            parse_options(&strings(&["--env"]), &known, 2)
                .err()
                .as_deref(),
            Some("--env needs a value.")
        );
    }

    #[test]
    fn numbers_follow_javascript() {
        assert_eq!(js_number("8080"), json!(8080));
        assert_eq!(js_number("1.5"), json!(1.5));
        assert_eq!(js_number("x"), Value::Null);
        // `Number()`'s radix prefixes.
        assert_eq!(js_number("0x1F90"), json!(8080));
        assert_eq!(js_number(" 0o17 "), json!(15));
        assert_eq!(js_number("0B101"), json!(5));
        assert_eq!(js_number("0x"), Value::Null);
        assert_eq!(js_number("-0x10"), Value::Null);
        assert_eq!(js_number("0x+1"), Value::Null);
    }
}
