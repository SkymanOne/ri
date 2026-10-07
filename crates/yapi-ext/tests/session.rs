//! pi extensions in a session driven by the faux provider: tools, event
//! handlers, commands and actions.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{cli_source, custom_entries, engine, options, scratch, session, text_of};
use serde_json::json;
use yapi_ai::faux::{Faux, Response};
use yapi_core::extensions::{Mode, NoUi};
use yapi_ext::ExtensionHost;
use yapi_types::message::Message;

const EXTENSION: &str = r#"
import { Type } from "typebox";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	let started = 0;
	pi.on("session_start", () => {
		started++;
		pi.appendEntry("started", { count: started, flag: pi.getFlag("mood") });
	});
	pi.registerFlag("mood", { type: "string", default: "calm", description: "Mood" });
	pi.registerTool({
		name: "greet",
		label: "Greet",
		description: "Greets someone",
		promptSnippet: "Greet people",
		parameters: Type.Object({ name: Type.String() }),
		async execute(_id, params: { name: string }, _signal, onUpdate) {
			onUpdate?.({ content: [{ type: "text", text: "working" }] });
			return { content: [{ type: "text", text: `Hello, ${params.name}!` }], details: { greeted: params.name } };
		},
	});
	pi.on("tool_call", (event) => {
		if (event.toolName === "greet" && event.input.name === "mallory") return { block: true, reason: "No greeting for mallory" };
	});
	pi.on("tool_result", (event) => {
		if (event.toolName === "greet") return { content: [...event.content, { type: "text", text: "(checked)" }] };
	});
	pi.on("before_agent_start", (event) => ({
		systemPrompt: `${event.systemPrompt}\n\nAlways be kind.`,
		message: { customType: "note", content: "Remember the user", display: false },
	}));
	pi.on("input", (event) => (event.text === "shout" ? { action: "transform", text: "SHOUT" } : undefined));
	pi.registerCommand("note", {
		description: "Adds a note",
		handler: async (args) => {
			pi.sendMessage({ customType: "note", content: `note: ${args}`, display: true });
		},
	});
}
"#;

async fn extensions(dir: &Path) -> Arc<ExtensionHost> {
    let path = dir.join("ext.ts");
    std::fs::write(&path, EXTENSION).unwrap();
    let host = ExtensionHost::load(&engine(), options(dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_tools_and_handlers_run_in_the_session() {
    let dir = scratch("tools");
    let js = extensions(&dir).await;
    let mut values = serde_json::Map::new();
    values.insert("mood".into(), json!("cheerful"));
    js.set_flags(values).await.unwrap();
    let faux = Faux::new([
        Response::tool_call("call-1", "greet", json!({"name": "yapi"})),
        Response::tool_call("call-2", "greet", json!({"name": "mallory"})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, js.for_session());
    assert!(session.active_tool_names().contains(&"greet".to_owned()));
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    assert_eq!(
        custom_entries(&session, "started"),
        [json!({"count": 1, "flag": "cheerful"})]
    );

    session.prompt("hi", Vec::new()).await.unwrap();
    let messages = session.messages();
    let results: Vec<String> = messages
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .map(text_of)
        .collect();
    assert_eq!(
        results,
        ["Hello, yapi!|(checked)", "No greeting for mallory"]
    );
    let notes: Vec<String> = messages
        .iter()
        .filter(|message| matches!(message, Message::Custom(_)))
        .map(text_of)
        .collect();
    assert_eq!(notes, ["Remember the user"]);

    let requests = faux.requests();
    let Some(Message::System(system)) = requests[0].first() else {
        panic!("no system message: {:?}", requests[0]);
    };
    assert!(
        system.text().ends_with("\n\nAlways be kind."),
        "{}",
        system.text()
    );
    assert!(
        system.text().contains("- greet: Greet people"),
        "{}",
        system.text()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_input_and_new_sessions() {
    let dir = scratch("commands");
    let js = extensions(&dir).await;
    let faux = Faux::new([Response::text("ok")]);
    let first = session(&faux, &dir, js.for_session());
    first
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    first.prompt("/note buy milk", Vec::new()).await.unwrap();
    // The action runs on the runtime; wait for it to land.
    for _ in 0..100 {
        if !first.messages().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let notes: Vec<String> = first.messages().iter().map(text_of).collect();
    assert_eq!(notes, ["note: buy milk"]);

    first.prompt("shout", Vec::new()).await.unwrap();
    // Custom messages reach the provider as user messages too.
    let texts: Vec<String> = faux.requests()[0]
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.content.text("")),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"SHOUT".to_owned()), "{texts:?}");
    assert!(!texts.contains(&"shout".to_owned()), "{texts:?}");

    // A new session runs the factories again: its counter starts over.
    first.shutdown().await;
    let second = session(&Faux::new([]), &dir, js.for_session());
    second
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    assert_eq!(
        custom_entries(&second, "started"),
        [json!({"count": 1, "flag": "calm"})]
    );
}

const MODELS_EXTENSION: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("probe", {
		description: "Reads the model registry",
		handler: async (_args, ctx) => {
			const registry = ctx.modelRegistry;
			const refreshed = await registry.refresh({ allowNetwork: false });
			let classifyError;
			try {
				await registry.classify({ provider: "nope", id: "x" }, { questions: [] });
			} catch (error) {
				classifyError = error.message;
			}
			pi.appendEntry("probe", {
				classifiers: registry.getModelsOfType("classifier", "typesafe").map((model) => model.id),
				image: registry.findOfType("image", "openrouter", "black-forest-labs/flux.2-flex")?.name,
				missing: registry.getModelOfType("image", "openrouter", "nope") ?? null,
				name: registry.getProviderDisplayName("anthropic"),
				error: registry.getError() ?? null,
				available: Array.isArray(await registry.getAvailableOfType("classifier")),
				aborted: refreshed.aborted,
				errors: refreshed.errors.size,
				classifyError,
			});
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn model_registry_reads_typed_models() {
    let dir = scratch("models");
    let path = dir.join("models.ts");
    std::fs::write(&path, MODELS_EXTENSION).unwrap();
    let js = ExtensionHost::load(&engine(), options(&dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(js.errors().is_empty(), "{:?}", js.errors());
    let session = session(&Faux::new([]), &dir, js.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.prompt("/probe", Vec::new()).await.unwrap();
    // The command runs on the runtime; wait for its entry.
    for _ in 0..200 {
        if !custom_entries(&session, "probe").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        custom_entries(&session, "probe"),
        [json!({
            "classifiers": ["jev-latest"],
            "image": "Black Forest Labs: FLUX.2 Flex",
            "missing": null,
            "name": "Anthropic",
            "error": null,
            "available": true,
            "aborted": false,
            "errors": 0,
            "classifyError": "Unknown classifier model \"nope/x\"",
        })]
    );
}

const TREE_EXTENSION: &str = r#"
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("children", {
		description: "Reads the session tree",
		handler: async (_args, ctx) => {
			const manager = ctx.sessionManager;
			const first = manager.getEntries().find((entry) => entry.customType === "first");
			pi.appendEntry("children", {
				ofFirst: manager.getChildren(first.id).map((entry) => entry.customType),
				roots: manager.getChildren(null).map((entry) => entry.type),
				label: manager.getLabel(first.id) === undefined,
			});
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn session_manager_reads_children() {
    let dir = scratch("children");
    let path = dir.join("tree.ts");
    std::fs::write(&path, TREE_EXTENSION).unwrap();
    let js = ExtensionHost::load(&engine(), options(&dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(js.errors().is_empty(), "{:?}", js.errors());
    let session = session(&Faux::new([]), &dir, js.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    session.append_custom_entry("first", None).unwrap();
    session.append_custom_entry("second", None).unwrap();
    session.prompt("/children", Vec::new()).await.unwrap();
    // The command runs on the runtime; wait for its entry.
    for _ in 0..200 {
        if !custom_entries(&session, "children").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        custom_entries(&session, "children"),
        [json!({"ofFirst": ["second"], "roots": ["model_change"], "label": true})]
    );
}

/// Aborting the session aborts a running extension tool's `signal`, as Esc
/// does in Pi, so a tool that waits for it ends instead of blocking the run.
#[tokio::test(flavor = "multi_thread")]
async fn aborting_the_session_aborts_extension_tools() {
    let dir = scratch("abort-tool");
    let path = dir.join("wait.ts");
    std::fs::write(
        &path,
        r#"export default function (pi) {
	pi.registerTool({
		name: "wait", label: "Wait", description: "Waits", parameters: { type: "object", properties: {} },
		execute: (_id, _params, signal, onUpdate) =>
			new Promise((resolve) => {
				signal.addEventListener("abort", () => resolve({ content: [{ type: "text", text: "stopped" }] }));
				onUpdate({ content: [{ type: "text", text: "waiting" }] });
			}),
	});
}
"#,
    )
    .unwrap();
    let host = ExtensionHost::load(&engine(), options(&dir), &[cli_source(&path)])
        .await
        .unwrap();
    let faux = Faux::new([
        Response::tool_call("call-1", "wait", json!({})),
        Response::text("done"),
    ]);
    let session = session(&faux, &dir, host.for_session());
    session
        .bind_extensions(Arc::new(NoUi), Mode::Print, None, None)
        .await;
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = started.clone();
    session.subscribe(Box::new(move |event| {
        if matches!(
            event,
            yapi_types::event::AgentEvent::ToolExecutionUpdate { .. }
        ) {
            notify.notify_one();
        }
    }));
    let running = session.clone();
    let prompt = tokio::spawn(async move { running.prompt("wait", Vec::new()).await });
    started.notified().await;
    session.abort();
    tokio::time::timeout(std::time::Duration::from_secs(10), prompt)
        .await
        .expect("the tool ignored the abort")
        .unwrap()
        .unwrap();
}
