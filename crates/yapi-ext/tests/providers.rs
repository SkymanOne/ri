//! Providers that pi extensions register: their own streams, sign-ins and
//! model refreshes, as the session uses them.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{cli_source, engine, options, scratch};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use yapi_ai::api::Apis;
use yapi_ai::registry::ModelRegistry;
use yapi_ai::stream::{EventStream, Request, StreamEvent, StreamOptions};
use yapi_core::agent_session::{AgentSession, Resources, SessionConfig};
use yapi_core::session::SessionManager;
use yapi_core::settings::SettingsManager;
use yapi_ext::ExtensionHost;
use yapi_types::event::AssistantMessageEvent;
use yapi_types::message::{ContentBlock, Message, StopReason, ThinkingLevel};
use yapi_types::model::Model;

const EXTENSION: &str = r#"
import { createAssistantMessageEventStream, getCurrentSystemPrompt, registerApiProvider } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const zero = () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } });

function streamSimple(model, context, options) {
	if (model.id === "throws") throw new Error("No stream for you");
	const events = createAssistantMessageEventStream();
	const output = { role: "assistant", content: [], api: model.api, provider: model.provider, model: model.id, usage: zero(), stopReason: "pending", timestamp: Date.now() };
	(async () => {
		events.push({ type: "start", partial: output });
		const text = model.id === "options"
			? [options.apiKey, options.reasoning, options.sessionId, getCurrentSystemPrompt(context.messages), context.messages.length].join("|")
			: "Hello";
		output.content.push({ type: "text", text: "" });
		events.push({ type: "text_start", contentIndex: 0, partial: output });
		output.content[0].text = text.slice(0, 3);
		output.usage.output = 1;
		events.push({ type: "text_delta", contentIndex: 0, delta: text.slice(0, 3), partial: output });
		if (model.id === "slow") {
			await new Promise((resolve) => options.signal.addEventListener("abort", resolve, { once: true }));
			output.stopReason = "aborted";
			output.errorMessage = "Request was aborted";
			events.push({ type: "error", reason: "aborted", error: output });
			return;
		}
		await new Promise((resolve) => setTimeout(resolve, 1));
		output.content[0].text = text;
		events.push({ type: "text_delta", contentIndex: 0, delta: text.slice(3), partial: output });
		events.push({ type: "text_end", contentIndex: 0, content: text, partial: output });
		output.usage = { ...zero(), input: 3, output: 2, totalTokens: 5 };
		output.stopReason = "stop";
		events.push({ type: "done", reason: "stop", message: output });
	})();
	return events;
}

export default function (pi: ExtensionAPI) {
	pi.registerProvider("echo", {
		baseUrl: "http://127.0.0.1:9",
		apiKey: "echo-key",
		api: "echo-api",
		models: ["hello", "options", "slow", "throws"].map((id) => ({
			id,
			name: id,
			reasoning: true,
			input: ["text"],
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 1000,
			maxTokens: 100,
		})),
		streamSimple,
	});
	registerApiProvider({ api: "relay-api", stream: streamSimple, streamSimple });
}
"#;

async fn load(dir: &Path) -> Arc<ExtensionHost> {
    let path = dir.join("providers.ts");
    std::fs::write(&path, EXTENSION).unwrap();
    let host = ExtensionHost::load(&engine(), options(dir), &[cli_source(&path)])
        .await
        .unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host
}

/// The registry and wire APIs of the extensions' providers, as startup
/// builds them.
fn registry_and_apis(host: &Arc<ExtensionHost>) -> (ModelRegistry, Apis) {
    let mut registry = ModelRegistry::builtin();
    let mut apis = Apis::default();
    for provider in host.providers() {
        registry.register_config(
            &provider.name,
            serde_json::from_value(provider.config).unwrap(),
        );
        if let Some(stream) = provider.stream {
            apis.register_for(&provider.name, stream);
        }
    }
    for api in host.apis() {
        apis.register(api);
    }
    (registry, apis)
}

fn request(model: Model, cancel: CancellationToken) -> Request {
    Request {
        model,
        messages: Vec::new(),
        options: StreamOptions {
            api_key: Some("echo-key".into()),
            cancel,
            ..StreamOptions::default()
        },
    }
}

async fn next(stream: &mut EventStream) -> StreamEvent {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("an event in time")
        .expect("an event before the end")
}

fn text(event: &StreamEvent) -> String {
    match event {
        StreamEvent::Done(message) | StreamEvent::Error(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        other => panic!("not a final event: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_events_as_the_extension_emits_them() {
    let dir = scratch("provider-stream");
    let host = load(&dir).await;
    let (registry, apis) = registry_and_apis(&host);
    let model = registry.find("echo", "hello").unwrap().clone();
    assert_eq!(model.api, "echo-api");

    let mut stream = apis.stream(request(model, CancellationToken::new()));
    let StreamEvent::Start(start) = next(&mut stream).await else {
        panic!("no start event");
    };
    assert_eq!(
        (start.provider.as_str(), start.model.as_str()),
        ("echo", "hello")
    );
    // Updates carry the usage of the live message as they are forwarded.
    let mut updates = Vec::new();
    let done = loop {
        match next(&mut stream).await {
            StreamEvent::Update { event, .. } => updates.push(event),
            other => break other,
        }
    };
    let delta = |delta: &str| AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: delta.into(),
    };
    assert_eq!(
        updates,
        [
            AssistantMessageEvent::TextStart { content_index: 0 },
            delta("Hel"),
            delta("lo"),
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Hello".into()
            },
        ]
    );
    let StreamEvent::Done(message) = &done else {
        panic!("not done: {done:?}");
    };
    assert_eq!(text(&done), "Hello");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.usage.total_tokens, Some(5));
    assert!(stream.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn aborts_and_failures_end_the_stream_with_an_error() {
    let dir = scratch("provider-abort");
    let host = load(&dir).await;
    let (registry, apis) = registry_and_apis(&host);

    // The first events arrive while the extension still waits.
    let cancel = CancellationToken::new();
    let slow = registry.find("echo", "slow").unwrap().clone();
    let mut stream = apis.stream(request(slow, cancel.clone()));
    assert!(matches!(next(&mut stream).await, StreamEvent::Start(_)));
    assert!(matches!(
        next(&mut stream).await,
        StreamEvent::Update { .. }
    ));
    assert!(matches!(
        next(&mut stream).await,
        StreamEvent::Update { .. }
    ));
    cancel.cancel();
    let StreamEvent::Error(message) = next(&mut stream).await else {
        panic!("no error event");
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request was aborted")
    );

    // A stream that throws ends with pi's setup error.
    let throws = registry.find("echo", "throws").unwrap().clone();
    let message = apis
        .stream(request(throws, CancellationToken::new()))
        .result()
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("No stream for you"));
}

#[tokio::test(flavor = "multi_thread")]
async fn registered_apis_stream_models_of_any_provider() {
    let dir = scratch("provider-api");
    let host = load(&dir).await;
    let (registry, apis) = registry_and_apis(&host);
    let mut model = registry.find("echo", "hello").unwrap().clone();
    model.provider = "elsewhere".into();
    model.api = "relay-api".into();
    let message = apis
        .stream(request(model.clone(), CancellationToken::new()))
        .result()
        .await
        .unwrap();
    assert_eq!(message.error_message, None);
    assert_eq!(message.api, "relay-api");

    // Another API stays unknown.
    model.api = "other-api".into();
    let message = apis
        .stream(request(model, CancellationToken::new()))
        .result()
        .await
        .unwrap();
    assert_eq!(
        message.error_message.as_deref(),
        Some("No API provider registered for api: other-api")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_runs_on_an_extension_provider() {
    let dir = scratch("provider-session");
    let host = load(&dir).await;
    let (registry, apis) = registry_and_apis(&host);
    let model = registry.find("echo", "options").unwrap().clone();
    assert!(registry.has_auth("echo"));
    let session = AgentSession::new(SessionConfig {
        cwd: dir.clone(),
        agent_dir: dir.join("agent"),
        settings: SettingsManager::in_memory(),
        registry,
        apis,
        session: SessionManager::in_memory(&dir),
        model: Some(model),
        thinking_level: ThinkingLevel::High,
        tools: Vec::new(),
        extensions: host.for_session(),
        include_extension_tools: true,
        allowed_tools: None,
        excluded_tools: Vec::new(),
        docs: yapi_core::docs::Locations::default(),
        resources: Resources {
            custom_prompt: Some("Be brief.".into()),
            ..Resources::default()
        },
    });
    session.prompt("hi", Vec::new()).await.unwrap();
    let Some(Message::Assistant(reply)) = session.messages().last().cloned() else {
        panic!("no reply: {:?}", session.messages());
    };
    assert_eq!(reply.error_message, None);
    let session_id = session.with_session(|file| file.id().to_owned());
    // The key, thinking level, session id, prompt and transcript reached the stream.
    let ContentBlock::Text(text) = &reply.content[0] else {
        panic!("no text: {reply:?}");
    };
    let parts: Vec<&str> = text.text.split('|').collect();
    assert_eq!(parts[..3], ["echo-key", "high", session_id.as_str()]);
    assert!(parts[3].starts_with("Be brief."), "{}", parts[3]);
    assert_eq!(parts[4], "2");
    assert_eq!(json!(reply.thinking_level), json!("high"));
}
