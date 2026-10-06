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
use yapi_ai::auth::{AuthError, AuthPrompt, AuthRequest, Interaction, LoginOptions};
use yapi_ai::registry::{LoginKind, ModelRegistry};
use yapi_ai::stream::{EventStream, Request, StreamEvent, StreamOptions};
use yapi_core::agent_session::{AgentSession, Resources, SessionConfig};
use yapi_core::session::SessionManager;
use yapi_core::settings::SettingsManager;
use yapi_ext::ExtensionHost;
use yapi_types::auth::Credential;
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
	pi.registerProvider("corp", {
		baseUrl: "http://127.0.0.1:9",
		api: "echo-api",
		models: [{ id: "corp-1", name: "Corp 1", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }],
		oauth: {
			name: "Corp SSO",
			async login(callbacks) {
				callbacks.onAuth({ url: "https://corp.example/login", instructions: "Sign in there" });
				callbacks.onProgress?.("Waiting for the code");
				const code = await callbacks.onPrompt({ message: "Code:" });
				const team = await callbacks.onSelect({ message: "Team", options: [{ id: "a", label: "Team A" }, { id: "b", label: "Team B" }] });
				return { refresh: "refresh-1", access: `access-${code}`, expires: 1, team };
			},
			async refreshToken(credentials) {
				return { ...credentials, refresh: "refresh-2", access: `${credentials.access}-refreshed`, expires: Date.now() + 3600000 };
			},
			getApiKey: (credentials) => `key:${credentials.access}:${credentials.team}`,
		},
		streamSimple,
	});
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
/// builds them, with the agent directory in `dir`.
fn registry_and_apis(host: &Arc<ExtensionHost>, dir: &Path) -> (ModelRegistry, Apis) {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut registry = ModelRegistry::load(&agent_dir);
    let mut apis = Apis::default();
    for provider in host.providers() {
        registry.register_config(
            &provider.name,
            serde_json::from_value(provider.config).unwrap(),
        );
        if let Some(stream) = provider.stream {
            apis.register_for(&provider.name, stream);
        }
        if let Some(oauth) = provider.oauth {
            registry.register_oauth(&provider.name, oauth);
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
    let (registry, apis) = registry_and_apis(&host, &dir);
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
    let (registry, apis) = registry_and_apis(&host, &dir);

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
    let (registry, apis) = registry_and_apis(&host, &dir);
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
    let (registry, apis) = registry_and_apis(&host, &dir);
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

#[tokio::test(flavor = "multi_thread")]
async fn oauth_providers_sign_in_store_and_refresh() {
    let dir = scratch("provider-oauth");
    let host = load(&dir).await;
    let (registry, _) = registry_and_apis(&host, &dir);
    assert_eq!(registry.provider_name("corp"), "Corp SSO");
    let flow = registry.oauth_flow("corp").unwrap();
    assert_eq!(flow.name(), "Corp SSO");
    assert!(!flow.is_subscription());
    assert!(!registry.has_auth("corp"));

    // The sign-in reaches the user through the interaction.
    let (interaction, mut requests) = Interaction::new(CancellationToken::new());
    let ui = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(request) = requests.recv().await {
            match request {
                AuthRequest::Notify(event) => seen.push(format!("{event:?}")),
                AuthRequest::Prompt { prompt, reply, .. } => {
                    let answer = match &prompt {
                        AuthPrompt::Select { options, .. } => options[1].id.clone(),
                        _ => "123".to_owned(),
                    };
                    seen.push(format!("{prompt:?}"));
                    let _ = reply.send(answer);
                }
            }
        }
        seen
    });
    let credential = registry
        .login(
            "corp",
            LoginKind::OAuth,
            &interaction,
            &LoginOptions::default(),
        )
        .await
        .unwrap();
    drop(interaction);
    let seen = ui.await.unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert!(seen[0].contains("https://corp.example/login") && seen[0].contains("Sign in there"));
    assert!(seen[1].contains("Waiting for the code"));
    assert!(seen[2].starts_with("Text") && seen[2].contains("Code:"));
    assert!(seen[3].starts_with("Select") && seen[3].contains("Team B"));
    let Credential::OAuth(stored) = &credential else {
        panic!("not OAuth: {credential:?}");
    };
    assert_eq!(stored.access, "access-123");

    // auth.json holds it as pi stores OAuth credentials.
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("agent").join("auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        file["corp"],
        json!({"type": "oauth", "refresh": "refresh-1", "access": "access-123", "expires": 1, "team": "b"})
    );
    assert!(registry.has_auth("corp"));

    // The expired token refreshes before a request, and the extension
    // derives the key.
    let model = registry.find("corp", "corp-1").unwrap().clone();
    let auth = registry.auth(&model).await;
    assert_eq!(auth.error, None);
    assert_eq!(auth.api_key.as_deref(), Some("key:access-123-refreshed:b"));
    let Some(Credential::OAuth(refreshed)) = registry.store().get("corp") else {
        panic!("no stored credential");
    };
    assert_eq!(refreshed.refresh, "refresh-2");

    // Cancelling the sign-in while it waits for the user stops it.
    let cancel = CancellationToken::new();
    let (interaction, mut requests) = Interaction::new(cancel.clone());
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            if let AuthRequest::Prompt { reply, .. } = request {
                cancel.cancel();
                // Keep the question open: only the cancellation ends it.
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(reply);
            }
        }
    });
    let options = LoginOptions::default();
    let result = registry
        .login("corp", LoginKind::OAuth, &interaction, &options)
        .await;
    assert_eq!(result.unwrap_err(), AuthError::Cancelled);
}
