//! A native extension built with the Rust SDK (`guest/examples/hello`) in a
//! session driven by the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{cli_source, custom_entries, engine, options, scratch, session, text_of};
use ri_ai::faux::{Faux, Response};
use ri_core::extensions::{Mode, NoUi};
use ri_ext::ExtensionHost;
use ri_types::message::Message;
use serde_json::json;

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
    session.bind_extensions(Arc::new(NoUi), Mode::Print).await;
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
