//! Messages stored in sessions and sent to providers.
//!
//! Mirrors `packages/ai/src/types.ts` and `packages/coding-agent/src/core/messages.ts`
//! in pi `v1.0.0`. Fields are declared in the order pi's providers and agent loop
//! build them; see the crate docs for why existing files are not re-serialized from
//! these types.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::present;

/// A message in a session or agent context, tagged by `role`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(ToolResultMessage),
    /// Output of a user `!command`.
    BashExecution(BashExecutionMessage),
    /// Extension message; in message entries only for sessions migrated from v2.
    Custom(CustomMessage),
    /// Built from a `branch_summary` entry when assembling context; not stored.
    BranchSummary(BranchSummaryMessage),
    /// Built from a `compaction` entry when assembling context; not stored.
    CompactionSummary(CompactionSummaryMessage),
}

/// System prompt state at one point in the transcript. The first system message is
/// the prompt; later ones patch it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub content: Content,
    /// Named prompt sections in render order; `None` removes a section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<IndexMap<String, Option<String>>>,
    /// Unix time in milliseconds.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<ToolDeclaration>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<ToolReference>>,
}

/// A tool as declared to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    /// `false` or a provider-specific configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<Value>,
}

/// A tool referenced by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Content,
    /// Unix time in milliseconds.
    pub timestamp: u64,
}

/// A model response. Providers append `responseId` and the fields after it once the
/// response arrives, so they follow `timestamp`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    /// Provider-native effort level used for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Unix time in milliseconds.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Concrete model reported by the provider when it differs from `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Redacted provider and runtime diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<Value>>,
    /// Whether the provider reported an explicit end of turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// pi thinking level the agent loop requested; set last by the loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

/// Handle to a response the provider finishes later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
}

/// Result of one tool call. `nestedCalls` is attached after the result is built, so it
/// follows `timestamp`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    /// Usage of the tool itself, outside the main context accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub is_error: bool,
    /// Unix time in milliseconds.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedToolCalls>,
}

/// Bounded record of the tool calls a tool made, for example from a codemode script.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCall>,
    /// False when calls were dropped, arguments omitted, or calls were unfinished.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCall {
    pub id: String,
    pub name: String,
    /// Omitted when over the size limit; `argumentsBytes` then gives the size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<u64>,
    pub status: NestedToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedToolCallStatus {
    Ok,
    Error,
    Unfinished,
}

/// Output of a user `!command`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    /// Absent when the command was cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// Unix time in milliseconds.
    pub timestamp: u64,
    /// Set for `!!command`: shown to the user, not sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: Content,
    pub display: bool,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    /// Unix time in milliseconds.
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: Option<String>,
    /// Unix time in milliseconds.
    pub timestamp: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    /// Unix time in milliseconds.
    pub timestamp: u64,
}

/// Message content: plain text or a list of blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A content block, tagged by `type`. Which kinds a role allows follows pi's types:
/// assistants use text, thinking and tool calls; users and tool results use text
/// and images.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
    Image(ImageContent),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    /// OpenAI Responses and Google message metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    /// Opaque reasoning replay data; may be empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// Content redacted by provider safety filters; the payload is in the signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    /// Google thought signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// OpenAI Responses tool namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64-encoded image data.
    pub data: String,
    pub mime_type: String,
}

/// Token usage and cost of one request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Reasoning tokens, already included in `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// Absent in sessions written by early pi versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    pub cost: Cost,
    /// Part of `cacheWrite` with one-hour retention; Anthropic only.
    #[serde(
        default,
        rename = "cacheWrite1h",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_1h: Option<u64>,
}

/// Cost in US dollars.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// pi thinking level, from `off` to `max`, ordered by effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// Every level, in order.
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    /// The name pi uses in JSON and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    /// Parses a name from [`ThinkingLevel::as_str`].
    pub fn parse(name: &str) -> Option<ThinkingLevel> {
        ThinkingLevel::ALL
            .into_iter()
            .find(|level| level.as_str() == name)
    }
}

impl Message {
    /// Unix time in milliseconds.
    pub fn timestamp(&self) -> u64 {
        match self {
            Message::System(message) => message.timestamp,
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
            Message::BashExecution(message) => message.timestamp,
            Message::Custom(message) => message.timestamp,
            Message::BranchSummary(message) => message.timestamp,
            Message::CompactionSummary(message) => message.timestamp,
        }
    }
}

impl Usage {
    /// The sum of two usages, as pi's `combineUsage`: the optional counts are
    /// present when either side has them.
    pub fn combine(&self, other: &Usage) -> Usage {
        let optional = |a: Option<u64>, b: Option<u64>| {
            (a.is_some() || b.is_some()).then(|| a.unwrap_or(0) + b.unwrap_or(0))
        };
        Usage {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_write: self.cache_write + other.cache_write,
            cache_write_1h: optional(self.cache_write_1h, other.cache_write_1h),
            reasoning: optional(self.reasoning, other.reasoning),
            total_tokens: Some(self.total_tokens.unwrap_or(0) + other.total_tokens.unwrap_or(0)),
            cost: Cost {
                input: self.cost.input + other.cost.input,
                output: self.cost.output + other.cost.output,
                cache_read: self.cost.cache_read + other.cost.cache_read,
                cache_write: self.cost.cache_write + other.cost.cache_write,
                total: self.cost.total + other.cost.total,
            },
        }
    }
}

impl Content {
    /// The text blocks joined by `separator`; images and other blocks are skipped.
    pub fn text(&self, separator: &str) -> String {
        match self {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => blocks_text(blocks, separator),
        }
    }

    /// The content as blocks; a string becomes one text block.
    pub fn into_blocks(self) -> Vec<ContentBlock> {
        match self {
            Content::Text(text) => vec![ContentBlock::text(text)],
            Content::Blocks(blocks) => blocks,
        }
    }
}

impl ContentBlock {
    /// A text block without a signature.
    pub fn text(text: impl Into<String>) -> ContentBlock {
        ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })
    }
}

/// The text blocks of `blocks` joined by `separator`.
pub fn blocks_text(blocks: &[ContentBlock], separator: &str) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

impl SystemMessage {
    /// The prompt text: content, then each present section, separated by blank lines.
    pub fn text(&self) -> String {
        let mut parts = vec![self.content.text("\n")];
        for text in self
            .sections
            .iter()
            .flatten()
            .filter_map(|(_, text)| text.clone())
        {
            parts.push(text);
        }
        parts.retain(|part| !part.is_empty());
        parts.join("\n\n")
    }

    /// How a later system message reads as a mid-conversation update.
    pub fn render_update(&self) -> String {
        let mut parts = Vec::new();
        let text = self.content.text("\n");
        if !text.is_empty() {
            parts.push(text);
        }
        for (name, value) in self.sections.iter().flatten() {
            parts.push(match value {
                None => format!("Removed system prompt section \"{name}\"."),
                Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
            });
        }
        parts.join("\n\n")
    }
}
