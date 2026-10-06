//! The Rust SDK's example extensions (`guest/examples`) in sessions driven by
//! the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{
    cli_source, custom_entries, engine, options, scratch, session, session_with_tools, text_of,
};
use serde_json::json;
use yapi_ai::faux::{Faux, Response};
use yapi_core::agent_session::{AgentSession, TreeNavigation};
use yapi_core::extensions::{Mode, NoUi};
use yapi_ext::ExtensionHost;
use yapi_types::message::Message;
use yapi_types::session::FileEntry;

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.wasm")
}

#[tokio::test(flavor = "multi_thread")]
async fn native_extensions_register_and_run() {
    let dir = scratch("native");
    let host = ExtensionHost::load_native(&engine(), options(&dir), &cli_source(&example()))
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let flags = host.flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].name, "shout-suffix");
    assert!(flags[0].takes_value);
    let mut values = serde_json::Map::new();
    values.insert("shout-suffix".into(), json!("!!"));
    host.set_flags(values).await.unwrap();

    let faux = Faux::new([
        Response::tool_call("call-1", "shout", json!({"text": "hello"})),
        Response::tool_call("call-2", "shout", json!({"text": ""})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    assert!(session.active_tool_names().contains(&"shout".to_owned()));
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None)
        .await;
    assert_eq!(
        custom_entries(&session, "hello-started"),
        [json!({"mode": "print"})]
    );

    session.prompt("shout hello", Vec::new()).await.unwrap();
    let results: Vec<String> = session
        .messages()
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .map(text_of)
        .collect();
    assert_eq!(results, ["HELLO!!", "Nothing to shout"]);
    assert_eq!(session.extensions()[0].commands()[0].name, "hello");
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.wasm"))
}

async fn load(dir: &Path, name: &str) -> Arc<ExtensionHost> {
    let host = ExtensionHost::load_native(&engine(), options(dir), &cli_source(&fixture(name)))
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

/// The text of every tool result, in order.
fn tool_results(session: &AgentSession) -> Vec<String> {
    session
        .messages()
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .map(text_of)
        .collect()
}

const BLOCKED: &str = "Dangerous command blocked. Start yapi with --allow-dangerous to allow it.";

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_blocks_dangerous_commands() {
    // The commands stay harmless if the gate lets them through.
    let dir = scratch("permission-gate");
    let host = load(&dir, "permission-gate").await;
    let faux = Faux::new([
        Response::tool_call(
            "call-1",
            "bash",
            json!({"command": "rm -rf ./nothing-here"}),
        ),
        Response::tool_call("call-2", "bash", json!({"command": "sudo true"})),
        Response::tool_call("call-3", "bash", json!({"command": "echo safe"})),
        Response::text("done"),
        Response::tool_call("call-4", "bash", json!({"command": "chmod 777 ./missing"})),
        Response::text("done"),
    ]);
    let session = session_with_tools(&faux, &dir, host.for_session(), &["bash"]);
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None)
        .await;
    session.prompt("run them", Vec::new()).await.unwrap();
    let results = tool_results(&session);
    assert_eq!(results[..2], [BLOCKED, BLOCKED]);
    assert!(results[2].contains("safe"), "{results:?}");

    let mut values = serde_json::Map::new();
    values.insert("allow-dangerous".into(), json!(true));
    host.set_flags(values).await.unwrap();
    session.prompt("again", Vec::new()).await.unwrap();
    let results = tool_results(&session);
    assert_ne!(results[3], BLOCKED, "the flag lets the command run");
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_paths_block_writes() {
    let dir = scratch("protected-paths");
    let host = load(&dir, "protected-paths").await;
    let faux = Faux::new([
        Response::tool_call(
            "call-1",
            "write",
            json!({"path": ".env", "content": "KEY=1"}),
        ),
        Response::tool_call(
            "call-2",
            "write",
            json!({"path": "notes.txt", "content": "hi"}),
        ),
        Response::text("done"),
    ]);
    let session = session_with_tools(&faux, &dir, host.for_session(), &["write"]);
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None)
        .await;
    session.prompt("write", Vec::new()).await.unwrap();
    assert_eq!(tool_results(&session)[0], "Path \".env\" is protected");
    assert!(!dir.join(".env").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "hi"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn todo_keeps_a_list_per_branch() {
    let dir = scratch("todo");
    let host = load(&dir, "todo").await;
    let todo = |id: &str, params: serde_json::Value| Response::tool_call(id, "todo", params);
    let faux = Faux::new([
        todo("call-1", json!({"action": "add", "text": "buy milk"})),
        todo("call-2", json!({"action": "add", "text": "write docs"})),
        todo("call-3", json!({"action": "toggle", "id": 1})),
        todo("call-4", json!({"action": "toggle", "id": 9})),
        todo("call-5", json!({"action": "list"})),
        Response::text("done"),
        todo("call-6", json!({"action": "list"})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None)
        .await;
    session.prompt("plan", Vec::new()).await.unwrap();
    assert_eq!(
        tool_results(&session),
        [
            "Added todo #1: buy milk",
            "Added todo #2: write docs",
            "Todo #1 completed",
            "Todo #9 not found",
            "[x] #1: buy milk\n[ ] #2: write docs",
        ]
    );

    // Back on the branch where only the first item existed, the list is
    // rebuilt from that point.
    let first = session.with_session(|file| {
        file.entries()
            .find_map(|entry| match entry {
                FileEntry::Message(entry) => match &entry.message {
                    Message::ToolResult(result) if result.tool_call_id == "call-1" => {
                        Some(entry.meta.id.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap()
    });
    session
        .navigate_tree(&first, TreeNavigation::default())
        .await
        .unwrap();
    session.prompt("list", Vec::new()).await.unwrap();
    assert_eq!(tool_results(&session).last().unwrap(), "[ ] #1: buy milk");
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_status_reports_uncommitted_files() {
    let dir = scratch("repo-status");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    let host = load(&dir, "repo-status").await;
    let faux = Faux::new([
        Response::tool_call("call-1", "repo_status", json!({})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None)
        .await;
    session.prompt("status", Vec::new()).await.unwrap();
    let report = &tool_results(&session)[0];
    assert!(report.starts_with("On main, "), "{report}");
    assert!(report.contains("a.txt"), "{report}");
}
