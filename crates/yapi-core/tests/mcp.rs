//! The MCP client against the test server in `tests/fixtures/mcp`, over stdio
//! and streamable HTTP, with OAuth and provider tokens. Needs `python3` on
//! `PATH`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use yapi_core::mcp::config::{Scope, ServerEntry, validate_server};
use yapi_core::mcp::connection::{Connection, State};
use yapi_core::mcp::http::{HttpOptions, HttpTransport};
use yapi_core::mcp::sign_in::SignInPrompt;
use yapi_core::mcp::stdio::{StdioOptions, StdioTransport};
use yapi_core::mcp::{
    ClientOptions, McpClient, McpError, RequestOptions, ResourceKind, Root, Transport,
};

fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mcp/server.py")
}

fn options() -> ClientOptions {
    ClientOptions {
        name: "pi".into(),
        version: "test".into(),
        request_timeout: Duration::from_secs(10),
        roots: vec![Root {
            uri: "file:///work".into(),
            name: Some("work".into()),
        }],
    }
}

fn stdio(env: Vec<(String, String)>) -> Transport {
    Transport::Stdio(Box::new(StdioTransport::new(StdioOptions {
        command: "python3".into(),
        args: vec![server_script().display().to_string()],
        cwd: std::env::temp_dir(),
        env,
    })))
}

async fn exercise(client: &McpClient) {
    let server = client.server().unwrap();
    assert_eq!(server.protocol_version, "2025-11-25");
    assert_eq!(
        server.instructions.as_deref(),
        Some("Test tools for ri.\nThey echo, add and fail.")
    );
    let tools = client.list_tools(RequestOptions::default()).await.unwrap();
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].title.as_deref(), Some("Add numbers"));

    let echo = client
        .call_tool("echo", json!({"text": "hi"}), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(echo, json!({"content": [{"type": "text", "text": "hi"}]}));
    let add = client
        .call_tool("add", json!({"a": 2, "b": 3}), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(add, json!({"structuredContent": {"sum": 5}, "content": []}));
    let fail = client
        .call_tool("fail", Value::Null, RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(fail["isError"], true);

    let updates = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&updates);
    let done = client
        .call_tool(
            "progress",
            json!({}),
            RequestOptions {
                on_progress: Some(Arc::new(move |progress: &Value| {
                    seen.lock().unwrap().push(progress["progress"].clone());
                })),
                ..RequestOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(done["content"][0]["text"], "Done");
    assert_eq!(*updates.lock().unwrap(), vec![json!(1), json!(2)]);

    let resources = client
        .list_resources(ResourceKind::Resources, RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(resources[0]["uri"], "test://notes/readme");
    let read = client
        .read_resource("test://notes/readme", RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(read["contents"][0]["text"], "Read me first.");
    let templates = client
        .list_resources(ResourceKind::Templates, RequestOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(templates, McpError::Rpc { code: -32601, .. }));
    let unknown = client
        .call_tool("nope", json!({}), RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(unknown.to_string(), "Unknown tool: nope");
}

#[tokio::test]
async fn stdio_server() {
    let client = McpClient::connect(
        options(),
        stdio(vec![("YAPI_TEST".into(), "visible".into())]),
    )
    .await
    .unwrap();
    exercise(&client).await;
    let env = client
        .call_tool(
            "env",
            json!({"name": "YAPI_TEST"}),
            RequestOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(env["content"][0]["text"], "visible");

    let timeout = client
        .call_tool(
            "sleep",
            json!({"seconds": 1}),
            RequestOptions {
                timeout: Some(Duration::from_millis(50)),
                ..RequestOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(timeout.to_string(), "MCP request timed out after 50ms");

    let closed = Arc::new(Mutex::new(0));
    let count = Arc::clone(&closed);
    client.on_close(move || *count.lock().unwrap() += 1);
    client.close().await;
    client.close().await;
    assert_eq!(*closed.lock().unwrap(), 1);
    let after = client
        .call_tool("echo", json!({}), RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(after.to_string(), "MCP client is closed");
}

#[tokio::test]
async fn stdio_server_that_exits_closes_the_client() {
    let client = McpClient::connect(options(), stdio(Vec::new()))
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Mutex::new(Some(tx));
    client.on_close(move || {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    });
    let outcome = client
        .call_tool("exit", json!({}), RequestOptions::default())
        .await;
    assert!(matches!(outcome, Err(McpError::Closed(_))), "{outcome:?}");
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!client.is_connected());
}

#[tokio::test]
async fn missing_command_fails_like_pi() {
    let transport = Transport::Stdio(Box::new(StdioTransport::new(StdioOptions {
        command: "yapi-no-such-command".into(),
        cwd: std::env::temp_dir(),
        ..StdioOptions::default()
    })));
    let error = McpClient::connect(options(), transport)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "spawn yapi-no-such-command ENOENT");
}

#[tokio::test]
async fn http_server() {
    let (mut server, url) = spawn_http(&[]).await;
    let transport = HttpTransport::new(HttpOptions {
        url,
        ..HttpOptions::default()
    });
    let client = McpClient::connect(options(), Transport::Http(transport.clone()))
        .await
        .unwrap();
    assert_eq!(transport.session_id().as_deref(), Some("test-session"));
    exercise(&client).await;
    client.close().await;
    server.kill().await.unwrap();
}

#[tokio::test]
async fn unreachable_http_server_fails_like_pi() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let transport = HttpTransport::new(HttpOptions {
        url: format!("http://127.0.0.1:{port}/mcp"),
        ..HttpOptions::default()
    });
    let error = McpClient::connect(options(), Transport::Http(transport))
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "fetch failed");
}

/// The test server over HTTP with `flags`, and its URL; killed when dropped.
async fn spawn_http(flags: &[&str]) -> (tokio::process::Child, String) {
    let mut server = tokio::process::Command::new("python3")
        .arg(server_script())
        .arg("--http")
        .args(flags)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = server.stdout.take().unwrap();
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
    let url = lines.next_line().await.unwrap().unwrap();
    (server, url)
}

fn http_connection(
    url: &str,
    config: Value,
    agent_dir: &Path,
    provider_token: Option<yapi_core::mcp::http::ProviderToken>,
) -> Arc<Connection> {
    let mut config = config;
    config["url"] = json!(url);
    Connection::new(
        ServerEntry {
            name: "demo".into(),
            config: validate_server("demo", &config).unwrap(),
            source: agent_dir.join("mcp.json"),
            scope: Scope::Global,
        },
        std::env::temp_dir(),
        agent_dir.to_path_buf(),
        Arc::new(|_: &Arc<Connection>| {}),
        Arc::new(|_: &str| false),
        provider_token,
    )
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yapi-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A prompt that opens the authorization URL like a browser and records it;
/// nobody pastes a redirect URL.
fn browser(opened: Arc<Mutex<Vec<String>>>) -> SignInPrompt {
    SignInPrompt {
        show_authorization_url: Box::new(move |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            let url = url.to_owned();
            tokio::spawn(async move { reqwest::get(url).await.unwrap().text().await.unwrap() });
        }),
        redirect_url: Box::new(|cancel| {
            Box::pin(async move {
                cancel.cancelled().await;
                None
            })
        }),
    }
}

fn stored_state(agent_dir: &Path, url: &str) -> serde_json::Map<String, Value> {
    let text = std::fs::read_to_string(agent_dir.join("mcp-auth.json")).unwrap();
    assert!(text.ends_with("}\n"));
    let states: Value = serde_json::from_str(&text).unwrap();
    states[format!("mcp__demo|{url}")]
        .as_object()
        .cloned()
        .unwrap()
}

/// `mcp-auth.json` with the values that differ between sign-ins masked: the
/// server and callback URLs, `state`, the PKCE verifier and the expiry.
fn masked_state_file(text: &str, client_name: &str) -> String {
    let mut states: serde_json::Map<String, Value> = serde_json::from_str(text).unwrap();
    let key = states.keys().next().unwrap().clone();
    let mut state = states.shift_remove(&key).unwrap();
    let base = state["discovery"]["authorizationServerUrl"]
        .as_str()
        .unwrap()
        .to_owned();
    let redirect = state["clientInformation"]["redirect_uris"][0]
        .as_str()
        .unwrap()
        .to_owned();
    state["oauthState"] = json!("<state>");
    state["codeVerifier"] = json!("<verifier>");
    state["tokensExpireAt"] = json!(0);
    states.insert(key, state);
    yapi_types::config::ConfigFile::McpAuth
        .render(&states)
        .unwrap()
        .replace(&redirect, "<redirect>")
        .replace(&base, "<base>")
        .replace(
            &format!("\"client_name\": \"{client_name}\""),
            "\"client_name\": \"<app>\"",
        )
}

fn pi_state_file() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi/agent/mcp-auth.json"),
    )
    .unwrap()
}

async fn echo(connection: &Arc<Connection>) -> Result<Value, McpError> {
    connection
        .call_tool("echo", json!({"text": "hi"}), RequestOptions::default())
        .await
}

#[tokio::test]
async fn oauth_sign_in_refresh_and_sign_out() {
    let (_server, url) = spawn_http(&["--oauth"]).await;
    let base = url.trim_end_matches("/mcp").to_owned();
    let agent_dir = scratch("oauth");
    let connection = http_connection(&url, json!({}), &agent_dir, None);

    // Without credentials the server needs a sign-in, and its challenge names its metadata.
    let error = connection.client().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "MCP server \"demo\" requires sign-in. Run /mcp to sign in."
    );
    assert_eq!(connection.snapshot().state, State::NeedsAuth);
    assert_eq!(
        connection.challenge().unwrap().resource_metadata_url,
        Some(format!("{base}/.well-known/oauth-protected-resource/mcp"))
    );

    // Discovery, registration and the code flow with PKCE.
    let opened = Arc::new(Mutex::new(Vec::new()));
    connection.sign_in(&browser(opened.clone())).await.unwrap();
    let authorization = reqwest::Url::parse(&opened.lock().unwrap()[0]).unwrap();
    assert_eq!(authorization.path(), "/authorize");
    let params: std::collections::HashMap<String, String> =
        authorization.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "client-1");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["scope"], "mcp:read");
    assert_eq!(params["resource"], url);
    assert!(params["redirect_uri"].starts_with("http://127.0.0.1:"));
    assert!(connection.challenge().is_none());
    // pi's state, in the order pi's sign-in writes it.
    let state = stored_state(&agent_dir, &url);
    let keys: Vec<&str> = state.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "serverUrl",
            "discovery",
            "clientInformation",
            "oauthState",
            "codeVerifier",
            "tokens",
            "tokensExpireAt"
        ]
    );
    assert_eq!(state["serverUrl"], json!(url));
    assert_eq!(state["discovery"]["authorizationServerUrl"], json!(base));
    assert_eq!(
        state["clientInformation"]["redirect_uris"],
        json!([params["redirect_uri"]])
    );
    // The same file pi writes for the same sign-in (`mcp-auth.mjs` in the fixture generator).
    assert_eq!(
        masked_state_file(
            &std::fs::read_to_string(agent_dir.join("mcp-auth.json")).unwrap(),
            "yapi"
        ),
        masked_state_file(&pi_state_file(), "pi")
    );

    // The token is sent.
    connection.reconnect().await.unwrap();
    assert_eq!(echo(&connection).await.unwrap()["content"][0]["text"], "hi");

    // A rejected token is refreshed once, and the request retried.
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/test/expire"))
        .send()
        .await
        .unwrap();
    assert_eq!(echo(&connection).await.unwrap()["content"][0]["text"], "hi");
    assert_eq!(
        stored_state(&agent_dir, &url)["tokens"]["refresh_token"],
        "refresh-2"
    );

    // A token about to expire is refreshed before it is sent.
    let path = agent_dir.join("mcp-auth.json");
    let mut states: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    states[format!("mcp__demo|{url}")]["tokensExpireAt"] = json!(0);
    std::fs::write(&path, states.to_string()).unwrap();
    assert_eq!(echo(&connection).await.unwrap()["content"][0]["text"], "hi");
    let stats = client
        .get(format!("{base}/test/stats"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let stats: Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats, json!({"registrations": 1, "refreshes": 2}));

    // Signing out removes the credentials and leaves the server needing a sign-in.
    assert!(connection.remove_credentials().await.unwrap());
    connection.sign_out().await;
    assert_eq!(connection.snapshot().state, State::NeedsAuth);
    assert!(!connection.remove_credentials().await.unwrap());
    assert!(!connection.signed_in_elsewhere().await);

    // A sign-in by another process is noticed.
    let other = http_connection(&url, json!({}), &agent_dir, None);
    let _ = other.client().await;
    other.sign_in(&browser(opened.clone())).await.unwrap();
    assert!(connection.signed_in_elsewhere().await);
    connection.reconnect().await.unwrap();
    assert_eq!(connection.snapshot().state, State::Connected);
    other.close().await;
    connection.close().await;
    let _ = std::fs::remove_dir_all(agent_dir);
}

#[tokio::test]
async fn provider_tokens_are_sent_for_auth_provider() {
    let (_server, url) = spawn_http(&["--token", "secret"]).await;
    let agent_dir = scratch("provider");
    let config = json!({"auth": {"provider": "acme"}});
    let token: yapi_core::mcp::http::ProviderToken = Arc::new(|provider: String| {
        Box::pin(async move { (provider == "acme").then(|| "secret".to_owned()) })
    });
    let connection = http_connection(&url, config.clone(), &agent_dir, Some(token));
    assert!(connection.oauth_url().is_none());
    assert_eq!(echo(&connection).await.unwrap()["content"][0]["text"], "hi");
    connection.close().await;

    // Without the provider's token, the server needs `/login <provider>`.
    let connection = http_connection(&url, config, &agent_dir, None);
    let error = connection.client().await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "MCP server \"demo\" requires sign-in. Run /login acme to sign in."
    );
    assert_eq!(connection.snapshot().state, State::NeedsAuth);
    assert!(!agent_dir.join("mcp-auth.json").exists());
}

/// Credentials pi stored are read and written back unchanged.
#[tokio::test]
async fn reads_and_keeps_credentials_pi_stored() {
    let text = pi_state_file();
    let states: serde_json::Map<String, Value> = serde_json::from_str(&text).unwrap();
    let url = states
        .keys()
        .next()
        .unwrap()
        .strip_prefix("mcp__demo|")
        .unwrap()
        .to_owned();
    let agent_dir = scratch("pi-state");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("mcp-auth.json"), &text).unwrap();
    let store = yapi_core::mcp::sign_in::CredentialStore::new(&agent_dir);
    assert_eq!(
        store.tokens("demo", &url).await.unwrap().unwrap()["access_token"],
        "access-1"
    );
    let server = store.for_server("demo", &url);
    server
        .save(server.load().await.unwrap().unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(agent_dir.join("mcp-auth.json")).unwrap(),
        text
    );
    let _ = std::fs::remove_dir_all(agent_dir);
}
