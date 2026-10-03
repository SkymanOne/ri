//! Points where the session steers the loop.

use futures_util::future::BoxFuture;
use ri_ai::registry::Auth;
use ri_types::event::{AgentEvent, ToolResult};
use ri_types::message::{AssistantMessage, Message, ThinkingLevel, ToolCall, ToolResultMessage};
use ri_types::model::Model;
use serde_json::Value;

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
    pub content: Option<Vec<ri_types::message::ContentBlock>>,
    /// New details.
    pub details: Option<Value>,
    /// New error flag.
    pub is_error: Option<bool>,
    /// New terminate flag.
    pub terminate: Option<bool>,
}

/// A completed turn.
#[derive(Debug)]
pub struct Turn<'a> {
    /// The response.
    pub message: &'a AssistantMessage,
    /// Results of its tool calls.
    pub tool_results: &'a [ToolResultMessage],
    /// The transcript after the turn.
    pub messages: &'a [Message],
}

/// What happens after a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnDecision {
    /// Default: continue while there are tool calls or queued messages.
    #[default]
    Default,
    /// Run another turn even without tool calls.
    Continue,
    /// Stop now.
    End,
}

/// Changes for the next request.
#[derive(Clone, Debug, Default)]
pub struct RequestUpdate {
    /// Replaces the transcript.
    pub messages: Option<Vec<Message>>,
    /// Switches the model.
    pub model: Option<Model>,
    /// Switches the thinking level.
    pub thinking_level: Option<ThinkingLevel>,
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

    /// Completes a tool result message before it is emitted, for example
    /// with the calls the tool made.
    fn complete_tool_result(&self, _message: &mut ToolResultMessage) {}

    /// Decides what follows a turn.
    fn finish_turn<'a>(&'a self, _turn: Turn<'a>) -> BoxFuture<'a, TurnDecision> {
        Box::pin(async { TurnDecision::Default })
    }

    /// Adjusts the transcript, model or thinking level before each request.
    fn prepare_request<'a>(
        &'a self,
        _messages: &'a [Message],
    ) -> BoxFuture<'a, Option<RequestUpdate>> {
        Box::pin(async { None })
    }

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
