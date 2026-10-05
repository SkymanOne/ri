//! The Anthropic Messages API against recorded and hand-written cassettes.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::net::SocketAddr;
use std::path::Path;

use serde_json::Value;
use yapi_ai::api::anthropic::AnthropicMessages;
use yapi_ai::catalog::builtin_models;
use yapi_ai::stream::{Provider, Request, StreamEvent, StreamOptions};
use yapi_mock::{Cassette, Interaction, MockServer, RequestMatch, Response};
use yapi_types::event::AssistantMessageEvent;
use yapi_types::message::{Content, Message, StopReason, ThinkingLevel, UserMessage};
use yapi_types::model::Model;

fn cassette(name: &str) -> Cassette {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/cassettes/anthropic-messages")
        .join(name);
    Cassette::load(&path).unwrap()
}

fn model(base_url: &str) -> Model {
    let mut model = builtin_models("anthropic")
        .into_iter()
        .find(|model| model.id == "claude-sonnet-4-5")
        .unwrap();
    model.base_url = base_url.to_owned();
    model
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: Content::Text(text.into()),
        timestamp: 1,
    })
}

async fn run(server: &MockServer, reasoning: Option<ThinkingLevel>) -> Vec<StreamEvent> {
    let request = Request {
        model: model(&server.url()),
        messages: vec![user("Say hello")],
        options: StreamOptions {
            api_key: Some("test-key".into()),
            reasoning,
            ..StreamOptions::default()
        },
    };
    let mut stream = AnthropicMessages.stream(request);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

#[tokio::test]
async fn streams_text() {
    let server = MockServer::start(any_port(), cassette("text.json"))
        .await
        .unwrap();
    let events = run(&server, Some(ThinkingLevel::Medium)).await;

    let StreamEvent::Start(start) = &events[0] else {
        panic!("first event is {:?}", events[0])
    };
    assert_eq!(start.stop_reason, StopReason::Pending);
    let updates: Vec<&AssistantMessageEvent> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Update { event, .. } => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates,
        [
            &AssistantMessageEvent::TextStart { content_index: 0 },
            &AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hello".into()
            },
            &AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: " from the mock.".into()
            },
            &AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Hello from the mock.".into()
            },
        ]
    );
    let StreamEvent::Done(done) = events.last().unwrap() else {
        panic!("last event is {:?}", events.last())
    };
    assert_eq!(
        yapi_types::json::to_string(done).unwrap(),
        format!(
            r#"{{"content":[{{"type":"text","text":"Hello from the mock."}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{{"input":12,"output":6,"cacheRead":0,"cacheWrite":0,"totalTokens":18,"cost":{{"input":0.000036,"output":0.00009,"cacheRead":0,"cacheWrite":0,"total":0.000126}},"cacheWrite1h":0}},"stopReason":"stop","timestamp":{},"responseId":"msg_mock_01","rawStopReason":"end_turn"}}"#,
            done.timestamp
        )
    );

    // The request matches what pi sends for the same prompt.
    let requests = server.finish().unwrap();
    let request = &requests[0];
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.query.as_deref(), Some("beta=true"));
    assert_eq!(request.headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        request.headers["anthropic-beta"],
        "interleaved-thinking-2025-05-14"
    );
    assert_eq!(request.headers["x-api-key"], yapi_mock::REDACTED);
    assert_eq!(
        request.body,
        r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":[{"type":"text","text":"Say hello","cache_control":{"type":"ephemeral"}}]}],"max_tokens":64000,"stream":true,"thinking":{"type":"enabled","budget_tokens":8192,"display":"summarized"}}"#
    );
}

#[tokio::test]
async fn reports_http_errors_like_the_sdk() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#;
    let server = MockServer::start(
        any_port(),
        Cassette {
            interactions: vec![Interaction {
                request: RequestMatch {
                    method: "POST".into(),
                    path: "/v1/messages".into(),
                },
                response: Response {
                    status: 400,
                    headers: Default::default(),
                    chunks: vec![body.into()],
                    body_base64: None,
                    chunk_delay_ms: 0,
                },
            }],
        },
    )
    .await
    .unwrap();
    let events = run(&server, None).await;
    assert_eq!(events.len(), 1);
    let StreamEvent::Error(error) = &events[0] else {
        panic!("expected an error, got {:?}", events[0])
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(
        error.error_message.as_deref(),
        Some(&*format!("400 {body}"))
    );
    let request: Value = serde_json::from_str(&server.finish().unwrap()[0].body).unwrap();
    assert_eq!(request["thinking"], serde_json::json!({"type": "disabled"}));
}

#[tokio::test]
async fn missing_key_fails_before_sending() {
    let server = MockServer::start(any_port(), Cassette::default())
        .await
        .unwrap();
    let request = Request {
        model: model(&server.url()),
        messages: vec![user("hi")],
        options: StreamOptions::default(),
    };
    let message = AnthropicMessages.stream(request).result().await.unwrap();
    assert_eq!(
        message.error_message.as_deref(),
        Some("No API key for provider: anthropic")
    );
    assert!(server.finish().unwrap().is_empty());
}
