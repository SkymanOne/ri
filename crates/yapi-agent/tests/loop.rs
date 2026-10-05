//! The agent loop against the faux provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{AgentContext, AgentHooks, ExecutionMode, LoopConfig, Tool, UpdateSink, run};
use yapi_ai::faux::{Faux, Response};
use yapi_ai::stream::{Provider, StreamOptions};
use yapi_types::event::{AgentEvent, ToolResult};
use yapi_types::message::{
    Content, ContentBlock, Message, ThinkingLevel, ToolDeclaration, UserMessage,
};
use yapi_types::model::Model;

struct Echo {
    declaration: ToolDeclaration,
}

impl Echo {
    fn new() -> Echo {
        Echo {
            declaration: ToolDeclaration {
                name: "echo".into(),
                description: "Echo text".into(),
                parameters: json!({"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}),
                constrained_sampling: None,
            },
        }
    }
}

impl Tool for Echo {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        _call_id: String,
        args: Value,
        _cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let text = args["text"].as_str().unwrap_or_default().to_owned();
            updates(ToolResult {
                content: vec![ContentBlock::text("working")],
                ..ToolResult::default()
            });
            Ok(ToolResult {
                content: vec![ContentBlock::text(text)],
                ..ToolResult::default()
            })
        })
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<AgentEvent>>,
}

impl AgentHooks for Recorder {
    fn on_event<'a>(&'a self, event: &'a AgentEvent) -> BoxFuture<'a, ()> {
        self.events.lock().unwrap().push(event.clone());
        Box::pin(async {})
    }
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id":"faux-1","name":"Faux","api":"faux","provider":"faux","baseUrl":"","reasoning":false,
        "input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":100000,"maxTokens":1000
    }))
    .unwrap()
}

fn event_types(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            let json: Value = serde_json::to_value(event).unwrap();
            let mut kind = json["type"].as_str().unwrap().to_owned();
            if let Some(role) = json["message"]["role"].as_str() {
                kind = format!("{kind}:{role}");
            }
            if let Some(inner) = json["assistantMessageEvent"]["type"].as_str() {
                kind = format!("{kind}:{inner}");
            }
            kind
        })
        .collect()
}

#[tokio::test]
async fn runs_tools_and_declares_them() {
    let faux = Faux::new([
        Response::tool_call("call_1", "echo", json!({"text": "hi"})),
        Response::text("done"),
    ]);
    let provider = faux.clone();
    let config = LoopConfig {
        model: model(),
        thinking_level: ThinkingLevel::Off,
        stream: Arc::new(move |request| provider.stream(request)),
        options: StreamOptions::default(),
        tool_execution: ExecutionMode::Parallel,
    };
    let mut context = AgentContext {
        messages: Vec::new(),
        tools: vec![Arc::new(Echo::new())],
    };
    let hooks = Recorder::default();
    let prompt = Message::User(UserMessage {
        content: Content::Text("go".into()),
        timestamp: 1,
    });
    let added = run(vec![prompt], &mut context, config, &hooks).await;

    assert_eq!(
        event_types(&hooks.events.lock().unwrap()),
        [
            "agent_start",
            "turn_start",
            "message_start:system",
            "message_end:system",
            "message_start:user",
            "message_end:user",
            "message_start:assistant",
            "message_update:toolcall_start",
            "message_update:toolcall_delta",
            "message_update:toolcall_end",
            "message_end:assistant",
            "tool_execution_start",
            "tool_execution_update",
            "tool_execution_end",
            "message_start:toolResult",
            "message_end:toolResult",
            "turn_end:assistant",
            "turn_start",
            "message_start:assistant",
            "message_update:text_start",
            "message_update:text_delta",
            "message_update:text_end",
            "message_end:assistant",
            "turn_end:assistant",
            "agent_end",
        ]
    );
    // system (tool declaration), user, assistant, tool result, assistant
    assert_eq!(added.len(), 5);
    let Message::System(system) = &added[0] else {
        panic!("first message is not the tool declaration")
    };
    assert_eq!(system.tools_added.as_ref().unwrap()[0].name, "echo");
    let Message::ToolResult(result) = &added[3] else {
        panic!("expected a tool result")
    };
    assert_eq!(result.content, vec![ContentBlock::text("hi")]);
    // The second request carries the tool result.
    assert_eq!(faux.requests()[1].len(), 4);
}

#[tokio::test]
async fn reports_unknown_tools_and_bad_arguments() {
    let faux = Faux::new([
        Response {
            content: vec![
                Response::tool_call("a", "missing", json!({}))
                    .content
                    .remove(0),
                Response::tool_call("b", "echo", json!({"text": 5}))
                    .content
                    .remove(0),
                Response::tool_call("c", "echo", json!({}))
                    .content
                    .remove(0),
            ],
            ..Response::default()
        },
        Response::text("ok"),
    ]);
    let provider = faux.clone();
    let config = LoopConfig {
        model: model(),
        thinking_level: ThinkingLevel::Off,
        stream: Arc::new(move |request| provider.stream(request)),
        options: StreamOptions::default(),
        tool_execution: ExecutionMode::Parallel,
    };
    let mut context = AgentContext {
        messages: Vec::new(),
        tools: vec![Arc::new(Echo::new())],
    };
    let hooks = Recorder::default();
    let added = run(Vec::new(), &mut context, config, &hooks).await;
    let results: Vec<(String, bool)> = added
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((
                yapi_types::message::blocks_text(&result.content, "\n"),
                result.is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(results[0], ("Tool missing not found".into(), true));
    // A number is coerced to the string the schema asks for.
    assert_eq!(results[1], ("5".into(), false));
    assert!(
        results[2]
            .0
            .starts_with("Validation failed for tool \"echo\"")
    );
    assert!(results[2].1);
}
