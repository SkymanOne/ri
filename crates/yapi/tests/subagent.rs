//! The `subagent` example extension runs another yapi as its subagent. The
//! parent and the child talk to separate mock providers, so each sees its
//! requests in a fixed order however the two processes interleave.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use yapi_mock::{Cassette, Interaction, MockServer, RequestMatch, Response};

/// An Anthropic Messages reply streamed as `blocks`, ending with `stop`.
fn reply(blocks: &[Value], stop: &str, delay_ms: u64) -> Interaction {
    let event = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");
    let mut chunks = vec![event(
        "message_start",
        json!({"type": "message_start", "message": {"id": "msg", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 1}}}),
    )];
    for (index, block) in blocks.iter().enumerate() {
        let (start, delta) = match block["type"].as_str() {
            Some("text") => (
                json!({"type": "text", "text": ""}),
                json!({"type": "text_delta", "text": block["text"]}),
            ),
            _ => (
                json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}),
                json!({"type": "input_json_delta", "partial_json": block["input"].to_string()}),
            ),
        };
        chunks.push(event(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": start}),
        ));
        chunks.push(event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index, "delta": delta}),
        ));
        chunks.push(event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    chunks.push(event(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {"output_tokens": 5}}),
    ));
    chunks.push(event("message_stop", json!({"type": "message_stop"})));
    Interaction {
        request: RequestMatch {
            method: "POST".into(),
            path: "/v1/messages".into(),
        },
        response: Response {
            status: 200,
            headers: [("content-type".to_owned(), "text/event-stream".to_owned())]
                .into_iter()
                .collect(),
            chunks,
            body_base64: None,
            chunk_delay_ms: delay_ms,
        },
    }
}

fn text(text: &str) -> Interaction {
    reply(&[json!({"type": "text", "text": text})], "end_turn", 0)
}

fn subagent(input: Value) -> Interaction {
    let call = json!({"type": "tool_use", "id": "toolu_1", "name": "subagent", "input": input});
    reply(&[call], "tool_use", 0)
}

async fn server(interactions: Vec<Interaction>) -> MockServer {
    MockServer::local(Cassette { interactions }).await.unwrap()
}

/// A home whose models send `anthropic` to `parent` and the `child`
/// provider's model to `child`.
fn home(name: &str, parent: &MockServer, child: &MockServer) -> PathBuf {
    let home = common::scratch(name);
    std::fs::create_dir_all(home.join("agent")).unwrap();
    let models = json!({"providers": {
        "anthropic": {"baseUrl": parent.url()},
        "child": {"baseUrl": child.url(), "api": "anthropic-messages", "apiKey": "mock",
            "models": [{"id": "claude-sonnet-4-5"}]},
    }});
    std::fs::write(home.join("agent/models.json"), models.to_string()).unwrap();
    home
}

/// yapi with the example loaded, on the parent's model.
fn yapi(home: &Path, args: &[&str]) -> Command {
    let example = common::repo().join("crates/yapi-ext/tests/fixtures/subagent.wasm");
    yapi_with(home, &example, args)
}

/// yapi with `extension` loaded, on the parent's model.
fn yapi_with(home: &Path, extension: &Path, args: &[&str]) -> Command {
    let mut command = common::yapi(home);
    command
        .args([
            "--no-session",
            "--model",
            "anthropic/claude-sonnet-4-5",
            "-e",
        ])
        .arg(extension)
        .args(args)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("PI_OFFLINE", "1");
    command
}

/// The text of every message the request in `body` sends.
fn sent(body: &str) -> String {
    serde_json::from_str::<Value>(body).unwrap()["messages"].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn foreground_subagents_answer_the_tool_call() {
    let parent = server(vec![
        subagent(json!({"task": "Count to four", "model": "child/claude-sonnet-4-5"})),
        text("The subagent counted."),
    ])
    .await;
    let child = server(vec![text("1 2 3 4")]).await;
    let home = home("subagent-foreground", &parent, &child);
    let output = tokio::task::spawn_blocking(move || {
        yapi(&home, &["-p", "Delegate the counting"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "The subagent counted."
    );
    let child = child.finish().unwrap();
    assert!(sent(&child[0].body).contains("Count to four"));
    let parent = parent.finish().unwrap();
    assert!(
        sent(&parent[1].body).contains("1 2 3 4"),
        "{}",
        parent[1].body
    );
}

/// RPC mode with its events read on a thread.
struct Rpc {
    child: Child,
    events: mpsc::Receiver<Value>,
}

impl Rpc {
    fn start(home: &Path) -> Rpc {
        let mut child = yapi(home, &["--mode", "rpc"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(event) = serde_json::from_str(&line.unwrap()) else {
                    continue;
                };
                if sender.send(event).is_err() {
                    return;
                }
            }
        });
        Rpc { child, events }
    }

    fn send(&mut self, command: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
    }

    /// Waits for the event `matches` accepts.
    fn wait(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let event = self
                .events
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("no {what}"));
            if matches(&event) {
                return event;
            }
        }
    }

    fn agent_end(&self) -> Value {
        self.wait("agent_end", |event| event["type"] == "agent_end")
    }

    fn stop(mut self) {
        drop(self.child.stdin.take());
        self.child.wait().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn background_subagents_report_in_a_new_turn() {
    let parent = server(vec![
        subagent(json!({"task": "Count to four", "background": true, "model": "child/claude-sonnet-4-5"})),
        text("Started it."),
        text("The subagent counted."),
    ])
    .await;
    let child = server(vec![text("1 2 3 4")]).await;
    let home = home("subagent-background", &parent, &child);
    tokio::task::spawn_blocking(move || {
        let mut rpc = Rpc::start(&home);
        rpc.send(json!({"type": "prompt", "message": "Delegate the counting"}));
        rpc.agent_end();
        // The answer arrives as a message that starts the next turn.
        let end = rpc.agent_end();
        assert!(end.to_string().contains("Subagent finished:"), "{end}");
        assert!(end.to_string().contains("The subagent counted."), "{end}");
        rpc.stop();
    })
    .await
    .unwrap();
    assert!(sent(&child.finish().unwrap()[0].body).contains("Count to four"));
    let parent = parent.finish().unwrap();
    assert!(
        sent(&parent[1].body).contains("running in the background"),
        "{}",
        parent[1].body
    );
    assert!(sent(&parent[2].body).contains("1 2 3 4"));
}

#[tokio::test(flavor = "multi_thread")]
async fn aborting_the_run_kills_the_subagent() {
    let task = format!("Count slowly {}", std::process::id());
    let parent = server(vec![subagent(
        json!({"task": task, "model": "child/claude-sonnet-4-5"}),
    )])
    .await;
    // The child's model never finishes answering.
    let child = server(vec![reply(
        &[json!({"type": "text", "text": "1"})],
        "end_turn",
        600_000,
    )])
    .await;
    let home = home("subagent-abort", &parent, &child);
    let requests = || child.requests().len();
    let running = |task: &str| {
        Command::new("pgrep")
            .args(["-f", task])
            .output()
            .unwrap()
            .status
            .success()
    };
    let mut rpc = Rpc::start(&home);
    rpc.send(json!({"type": "prompt", "message": "Delegate the counting"}));
    for _ in 0..300 {
        if requests() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(requests(), 1, "the subagent never asked its model");
    assert!(running(&task));
    rpc.send(json!({"type": "abort"}));
    let rpc = tokio::task::spawn_blocking(move || {
        rpc.agent_end();
        rpc
    })
    .await
    .unwrap();
    for _ in 0..50 {
        if !running(&task) {
            rpc.stop();
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the subagent outlived the aborted run");
}

/// Pi's `RpcClient` runs yapi in RPC mode and drives it from a Pi extension.
#[tokio::test(flavor = "multi_thread")]
async fn pi_extensions_drive_subagents_with_rpc_client() {
    let delegate = json!({"type": "tool_use", "id": "toolu_1", "name": "delegate", "input": {"task": "Count to four"}});
    let parent = server(vec![
        reply(&[delegate], "tool_use", 0),
        text("The subagent counted."),
    ])
    .await;
    let child = server(vec![text("1 2 3 4")]).await;
    let home = home("subagent-rpc-client", &parent, &child);
    let extension = home.join("delegate.ts");
    std::fs::write(
        &extension,
        r#"import { RpcClient } from "@earendil-works/pi-coding-agent";

export default function (pi) {
	pi.registerTool({
		name: "delegate",
		label: "Delegate",
		description: "Delegates a task to a subagent",
		parameters: { type: "object", properties: { task: { type: "string" } }, required: ["task"] },
		async execute(_id, params) {
			const client = new RpcClient({ model: "child/claude-sonnet-4-5", args: ["--no-session"] });
			await client.start();
			try {
				const events = await client.promptAndWait(params.task);
				const ended = events.some((event) => event.type === "agent_end");
				const text = await client.getLastAssistantText();
				return { content: [{ type: "text", text: `${text} (ended: ${ended})` }] };
			} finally {
				await client.stop();
			}
		},
	});
}
"#,
    )
    .unwrap();
    let output = tokio::task::spawn_blocking(move || {
        yapi_with(&home, &extension, &["-p", "Delegate the counting"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "The subagent counted."
    );
    assert!(sent(&child.finish().unwrap()[0].body).contains("Count to four"));
    let parent = parent.finish().unwrap();
    assert!(
        sent(&parent[1].body).contains("1 2 3 4 (ended: true)"),
        "{}",
        parent[1].body
    );
}
