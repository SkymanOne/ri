//! Lines of a session file: one header, then entries forming a tree.
//!
//! Mirrors `packages/coding-agent/src/core/session-manager.ts` in pi `v1.0.0`. Each
//! line is one JSON object; [`crate::json::to_string`] plus `\n` writes it.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{Content, Message, Usage};
use crate::present;

/// Session file format version that pi `v1.0.0` writes.
pub const CURRENT_VERSION: u32 = 3;

/// One line of a session file, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileEntry {
    /// The first line.
    Session(SessionHeader),
    Message(MessageEntry),
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    ModelChange(ModelChangeEntry),
    Usage(UsageEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    /// Extension state; not part of the model context.
    Custom(CustomEntry),
    /// Extension message; part of the model context.
    CustomMessage(CustomMessageEntry),
    ContextEdit(ContextEditEntry),
    Label(LabelEntry),
    SessionInfo(SessionInfoEntry),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// Absent in v1 files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    /// ISO 8601 time.
    pub timestamp: String,
    pub cwd: String,
    /// Path of the session this one was forked or branched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// v1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// v1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// v1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// v1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_from: Option<String>,
}

/// Tree position shared by every entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryMeta {
    /// 8 hex digits, or a UUID when a collision fallback was needed.
    pub id: String,
    /// `None` for the root entry.
    pub parent_id: Option<String>,
    /// ISO 8601 time.
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub message: Message,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub thinking_level: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub provider: String,
    pub model_id: String,
}

/// Usage outside model responses, such as cache warming.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub summary: String,
    /// `Some(None)` when nothing is kept; `None` in migrated v1 files whose kept index
    /// pointed past the end.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_kept_entry_id: Option<Option<String>>,
    pub tokens_before: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Produced by an extension hook rather than pi's summarizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// System prompt checkpoint at the cut; always a system message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<Box<Message>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub from_id: String,
    pub summary: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// pi writes the tree position last for this entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    pub custom_type: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    #[serde(flatten)]
    pub meta: EntryMeta,
}

/// pi writes the tree position last for this entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    pub custom_type: String,
    pub content: Content,
    pub display: bool,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
    #[serde(flatten)]
    pub meta: EntryMeta,
}

/// Replaces or removes an earlier entry's contribution to the model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub target_id: String,
    /// `None` removes the target from the context.
    pub replacement: Option<Replacement>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Replacement {
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    pub target_id: String,
    /// Absent when the label was cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub meta: EntryMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
