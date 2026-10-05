//! A scripted provider for tests: replays canned responses as streams.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use yapi_types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, ThinkingContent, ToolCall,
};

use crate::stream::{EventStream, Provider, Request, StreamEvent, new_output, now_ms};

/// A canned response.
#[derive(Clone, Debug, Default)]
pub struct Response {
    /// Content blocks, streamed in order.
    pub content: Vec<ContentBlock>,
    /// Final stop reason; `toolUse` when the content has tool calls, else `stop`.
    pub stop_reason: Option<StopReason>,
    /// Error message for `error` and `aborted`.
    pub error_message: Option<String>,
}

impl Response {
    /// A text response.
    pub fn text(text: &str) -> Response {
        Response {
            content: vec![text_block(text)],
            ..Response::default()
        }
    }

    /// A response with one tool call.
    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Response {
        Response {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: arguments.as_object().cloned().unwrap_or_else(Map::new),
                thought_signature: None,
                namespace: None,
            })],
            ..Response::default()
        }
    }

    /// A failed response.
    pub fn error(message: &str) -> Response {
        Response {
            stop_reason: Some(StopReason::Error),
            error_message: Some(message.to_owned()),
            ..Response::default()
        }
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::text(text)
}

/// Replays responses in order and records the requests it receives. When the
/// script runs out it answers with an error.
#[derive(Clone, Debug, Default)]
pub struct Faux {
    script: Arc<Mutex<VecDeque<Response>>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Faux {
    /// A provider that will give `responses`, one per request.
    pub fn new(responses: impl IntoIterator<Item = Response>) -> Faux {
        Faux {
            script: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::default(),
        }
    }

    /// The transcripts received so far.
    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Provider for Faux {
    fn api(&self) -> &str {
        "faux"
    }

    fn stream(&self, request: Request) -> EventStream {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.messages.clone());
        }
        let response = self
            .script
            .lock()
            .ok()
            .and_then(|mut script| script.pop_front())
            .unwrap_or_else(|| Response::error("faux: no scripted response left"));
        let (sender, stream) = EventStream::channel();
        let mut output: AssistantMessage = new_output(&request.model, now_ms());
        output.response_id = Some("faux".into());
        let input = crate::transcript::estimate_context_tokens(&request.messages);
        output.usage.input = input;
        output.usage.total_tokens = Some(input);
        sender.send(StreamEvent::Start(output.clone()));

        for block in response.content {
            let (empty, delta) = match &block {
                ContentBlock::Text(text) => (text_block(""), text.text.clone()),
                ContentBlock::Thinking(thinking) => (
                    ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        ..thinking.clone()
                    }),
                    thinking.thinking.clone(),
                ),
                ContentBlock::ToolCall(call) => (
                    ContentBlock::ToolCall(ToolCall {
                        arguments: Map::new(),
                        ..call.clone()
                    }),
                    yapi_types::json::to_string(&call.arguments).unwrap_or_default(),
                ),
                ContentBlock::Image(_) => continue,
            };
            let index = sender.start(&mut output, empty);
            sender.delta(&mut output, index, &delta);
            output.content[index] = block;
            sender.end(&output, index);
        }

        let has_calls = output
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        output.stop_reason = response.stop_reason.unwrap_or(if has_calls {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        });
        let produced = crate::transcript::estimate_message_tokens(&Message::Assistant(Box::new(
            output.clone(),
        )));
        output.usage.output = produced;
        output.usage.total_tokens = Some(input + produced);
        match output.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                output.error_message = response.error_message;
                sender.send(StreamEvent::Error(output));
            }
            _ => sender.send(StreamEvent::Done(output)),
        }
        stream
    }
}
