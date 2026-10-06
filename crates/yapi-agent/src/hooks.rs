//! Points where the session steers the loop.

use futures_util::future::BoxFuture;
use serde_json::Value;
use yapi_ai::registry::Auth;
use yapi_types::event::{AgentEvent, ToolResult};
use yapi_types::message::{AssistantMessage, Message, ToolCall, ToolResultMessage};
use yapi_types::model::Model;

/// A tool call about to run.
#[derive(Debug)]
pub struct BeforeToolCall<'a> {
    /// The response that made the call.
    pub assistant_message: &'a AssistantMessage,
    /// The call as the model made it.
    pub tool_call: &'a ToolCall,
    /// Validated arguments.
    pub args: &'a Value,
    /// The transcript so far.
    pub messages: &'a [Message],
    /// The tool call that made this call, for calls a tool made.
    pub parent_tool_call_id: Option<&'a str>,
}

/// Blocks a tool call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    /// Shown to the model; "Tool execution was blocked" when absent.
    pub reason: Option<String>,
    /// Ends the run after this batch.
    pub terminate: bool,
}

/// A tool call that ran.
#[derive(Debug)]
pub struct AfterToolCall<'a> {
    /// The response that made the call.
    pub assistant_message: &'a AssistantMessage,
    /// The call as the model made it.
    pub tool_call: &'a ToolCall,
    /// Validated arguments.
    pub args: &'a Value,
    /// What the tool returned.
    pub result: &'a ToolResult,
    /// Whether the result is an error.
    pub is_error: bool,
    /// The tool call that made this call, for calls a tool made.
    pub parent_tool_call_id: Option<&'a str>,
}

/// Replacement fields for a tool result; `None` keeps the original.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultPatch {
    /// New content.
    pub content: Option<Vec<yapi_types::message::ContentBlock>>,
    /// New details.
    pub details: Option<Value>,
    /// New error flag.
    pub is_error: Option<bool>,
    /// New terminate flag.
    pub terminate: Option<bool>,
}

/// What the session decides as a turn ends; pi's `AgentTurnDecision`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnDecision {
    /// Ensures one more provider request when nothing else would make one.
    pub continue_run: bool,
    /// The transcript to continue from, when the session changed it.
    pub messages: Option<Vec<Message>>,
}

/// Hooks into the agent loop. Every method has a no-op default.
///
/// The loop awaits each hook before it continues, so a hook sees a consistent
/// transcript. Hooks never call back into the loop.
pub trait AgentHooks: Send + Sync {
    /// Receives every loop event, in order.
    fn on_event<'a>(&'a self, _event: &'a AgentEvent) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Rewrites the transcript before it is converted for the provider.
    fn transform_context(&self, messages: Vec<Message>) -> BoxFuture<'_, Vec<Message>> {
        Box::pin(async { messages })
    }

    /// Converts session messages to the four roles providers accept.
    fn convert_to_llm(&self, messages: Vec<Message>) -> Vec<Message> {
        messages
    }

    /// Credentials for a request to `model`, resolved fresh for each request.
    fn auth<'a>(&'a self, _model: &'a Model) -> BoxFuture<'a, Auth> {
        Box::pin(async { Auth::default() })
    }

    /// May block a validated call.
    fn before_tool_call<'a>(&'a self, _call: BeforeToolCall<'a>) -> BoxFuture<'a, Option<Block>> {
        Box::pin(async { None })
    }

    /// May patch a result.
    fn after_tool_call<'a>(
        &'a self,
        _call: AfterToolCall<'a>,
    ) -> BoxFuture<'a, Option<ResultPatch>> {
        Box::pin(async { None })
    }

    /// A turn finished: its assistant message and tool results are emitted
    /// and `turn_end` is next; pi's `finishTurn`. The decision is ignored
    /// after a failed or aborted response.
    fn finish_turn<'a>(
        &'a self,
        _message: &'a AssistantMessage,
        _tool_results: &'a [ToolResultMessage],
    ) -> BoxFuture<'a, TurnDecision> {
        Box::pin(async { TurnDecision::default() })
    }

    /// Completes a tool result message before it is emitted, for example
    /// with the calls the tool made.
    fn complete_tool_result(&self, _message: &mut ToolResultMessage) {}

    /// The tools for the next request, read before each turn; `None` keeps
    /// the current ones.
    fn current_tools(&self) -> Option<Vec<std::sync::Arc<dyn crate::Tool>>> {
        None
    }

    /// Messages the user queued to steer the current run; drained when read.
    fn steering_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        Box::pin(async { Vec::new() })
    }

    /// Messages to send once the run would otherwise stop; drained when read.
    fn follow_up_messages(&self) -> BoxFuture<'_, Vec<Message>> {
        Box::pin(async { Vec::new() })
    }
}
