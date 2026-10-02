//! Streaming events, in the shape pi's JSON and RPC modes print them.
//!
//! Mirrors `AssistantMessageEvent` in `packages/ai/src/types.ts`, `AgentEvent` in
//! `packages/agent/src/types.ts` and `AgentSessionEvent` with `toJsonEvent` in
//! `packages/coding-agent/src/modes/json-event.ts` in pi `v1.0.0`. pi's in-process
//! events also carry the live partial message; on the wire it is replaced by the
//! cumulative usage, and `toolcall_start` gains the call's id and tool name.
#![allow(
    missing_docs,
    reason = "variants mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{ContentBlock, Message, ThinkingLevel, ToolCall, ToolResultMessage, Usage};
use crate::session::FileEntry;

/// One step of a streamed assistant message, as carried by `message_update`.
///
/// `contentIndex` addresses the message's `content`. Text and thinking blocks start
/// empty and grow by deltas until the authoritative `*_end`; tool-call arguments
/// stream as JSON text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantMessageEvent {
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        content_index: usize,
        /// The finished call, as its content block (with `type: "toolCall"`).
        #[serde(with = "tool_call_block")]
        tool_call: ToolCall,
    },
}

/// Serializes tool results as tagged messages.
mod tool_result_messages {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::message::{Message, ToolResultMessage};

    pub fn serialize<S: Serializer>(
        results: &[ToolResultMessage],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let messages: Vec<Message> = results.iter().cloned().map(Message::ToolResult).collect();
        messages.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<ToolResultMessage>, D::Error> {
        Vec::<Message>::deserialize(deserializer)?
            .into_iter()
            .map(|message| match message {
                Message::ToolResult(result) => Ok(result),
                _ => Err(serde::de::Error::custom("expected a toolResult message")),
            })
            .collect()
    }
}

/// Serializes a tool call as its tagged content block.
mod tool_call_block {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::message::{ContentBlock, ToolCall};

    pub fn serialize<S: Serializer>(call: &ToolCall, serializer: S) -> Result<S::Ok, S::Error> {
        ContentBlock::ToolCall(call.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolCall, D::Error> {
        match ContentBlock::deserialize(deserializer)? {
            ContentBlock::ToolCall(call) => Ok(call),
            _ => Err(serde::de::Error::custom("expected a toolCall block")),
        }
    }
}

/// What a tool returns: content for the model, details for the UI and session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    /// Text and images sent to the model.
    pub content: Vec<ContentBlock>,
    /// Tool-specific data, stored with the result but not sent to the model.
    #[serde(
        default,
        deserialize_with = "crate::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Ends the run after this batch when every result in it asks to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

/// Why a compaction ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// What a compaction produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// An agent session event, one JSON line in JSON mode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
        will_retry: bool,
    },
    /// The session is idle: no run, retry or queued work remains.
    AgentSettled,
    TurnStart,
    TurnEnd {
        message: Message,
        /// Tool results, as messages (with `role: "toolResult"`).
        #[serde(with = "tool_result_messages")]
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Message,
    },
    MessageUpdate {
        usage: Usage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: ToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    EntryAppended {
        entry: FileEntry,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    BashExecutionUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        delta: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn message_update_matches_pi_json_mode() {
        let line = r#"{"type":"message_update","usage":{"input":12,"output":6,"cacheRead":0,"cacheWrite":0,"totalTokens":18,"cost":{"input":0.000036,"output":0.00009,"cacheRead":0,"cacheWrite":0,"total":0.000126},"cacheWrite1h":0},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}"#;
        let event: AgentEvent = serde_json::from_str(line).unwrap();
        assert_eq!(json::to_string(&event).unwrap(), line);
    }

    #[test]
    fn toolcall_start_carries_id_and_name() {
        let event = AssistantMessageEvent::ToolcallStart {
            content_index: 1,
            id: "call_1".into(),
            tool_name: "read".into(),
        };
        assert_eq!(
            json::to_string(&event).unwrap(),
            r#"{"type":"toolcall_start","contentIndex":1,"id":"call_1","toolName":"read"}"#
        );
    }

    #[test]
    fn unit_variants_are_bare_types() {
        assert_eq!(
            json::to_string(&AgentEvent::AgentSettled).unwrap(),
            r#"{"type":"agent_settled"}"#
        );
    }
}
