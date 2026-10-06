//! Regression tests for the JS runtime component's Node shims and dispatch.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::sync::Arc;

use serde_json::{Value, json};
use yapi_ext::{Instance, NoBridge, Options};

/// Loads extension `main` (TypeScript) in a fresh instance and returns it
/// with what loading reported.
async fn load(name: &str, main: &str) -> (Instance, Value) {
    let dir = common::scratch(&format!("guest-{name}"));
    let path = dir.join("main.ts");
    std::fs::write(&path, main).unwrap();
    let instance = Instance::start(
        &common::engine(),
        Options::new(dir.clone()),
        Arc::new(NoBridge),
    )
    .await
    .unwrap();
    let loaded = instance
        .call(
            "load",
            &json!({"cwd": dir, "extensions": [{"id": 1, "path": path}]}),
        )
        .await
        .unwrap();
    (instance, loaded["extensions"][0].clone())
}

/// The description of the `probe` command extension `main` registers.
async fn probe(name: &str, main: &str) -> String {
    let (_instance, extension) = load(name, main).await;
    assert_eq!(extension.get("error"), None, "{extension}");
    extension["commands"][0]["description"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Every Node built-in imports by its bare name, as in Node, except the ones
/// Node loads only with the `node:` prefix.
#[tokio::test(flavor = "multi_thread")]
async fn imports_node_builtins_by_bare_name() {
    let main = r#"
import qs from "querystring";
import { ReadableStream } from "stream/web";
import { text } from "stream/consumers";
import { isPromise } from "util/types";
import { lookup } from "dns/promises";
import win32 from "path/win32";
import inspector from "inspector/promises";
import constants from "constants";
import sys from "sys";
import wasi from "wasi";
import nodeConsole from "console";
import { createRequire, isBuiltin } from "node:module";
export default async function (pi) {
	const require = createRequire(import.meta.url);
	const failure = (load) => {
		try {
			load();
			return "loaded";
		} catch (error) {
			return error.message.split("\n")[0];
		}
	};
	const sqlite = await import("sqlite").then(() => "loaded", (error) => error.message.split("\n")[0]);
	pi.registerCommand("probe", {
		description: [
			qs.stringify({ a: [1, 2] }),
			typeof ReadableStream,
			typeof text,
			isPromise(Promise.resolve()),
			typeof lookup,
			typeof win32.join,
			typeof inspector.Session,
			constants.F_OK,
			typeof sys.format,
			typeof wasi.WASI,
			typeof nodeConsole.log,
			isBuiltin("querystring"),
			isBuiltin("test"),
			isBuiltin("node:test"),
			failure(() => require("node:test")),
			failure(() => require("test")),
			sqlite,
		].join("|"),
		handler: async () => {},
	});
}
"#;
    assert_eq!(
        probe("builtins", main).await,
        "a=1&a=2|function|function|true|function|function|function|0|function|function|function|true|false|true|loaded|Cannot find module 'test'|Cannot find module 'sqlite'"
    );
}
