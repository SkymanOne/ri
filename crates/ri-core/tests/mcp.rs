//! The MCP client against the test server in `tests/fixtures/mcp`, over stdio
//! and streamable HTTP. Needs `python3` on `PATH`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ri_core::mcp::http::{HttpOptions, HttpTransport};
use ri_core::mcp::stdio::{StdioOptions, StdioTransport};
use ri_core::mcp::{ClientOptions, McpClient, McpError, RequestOptions, Root, Transport};
use serde_json::{Value, json};

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
        .list_resources(RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(resources[0]["uri"], "test://notes/readme");
    let read = client
        .read_resource("test://notes/readme", RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(read["contents"][0]["text"], "Read me first.");
    let templates = client
        .list_resource_templates(RequestOptions::default())
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
    let client = McpClient::connect(options(), stdio(vec![("RI_TEST".into(), "visible".into())]))
        .await
        .unwrap();
    exercise(&client).await;
    let env = client
        .call_tool("env", json!({"name": "RI_TEST"}), RequestOptions::default())
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
        command: "ri-no-such-command".into(),
        cwd: std::env::temp_dir(),
        ..StdioOptions::default()
    })));
    let error = McpClient::connect(options(), transport)
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "spawn ri-no-such-command ENOENT");
}

#[tokio::test]
async fn http_server() {
    let mut server = tokio::process::Command::new("python3")
        .arg(server_script())
        .arg("--http")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = server.stdout.take().unwrap();
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
    let url = lines.next_line().await.unwrap().unwrap();
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
