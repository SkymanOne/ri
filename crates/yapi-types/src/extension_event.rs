//! Events pi delivers to extension handlers, in the shape and key order of
//! pi's emit sites.
//!
//! Mirrors the event interfaces in `packages/coding-agent/src/core/extensions/types.ts`
//! in pi `v1.0.0`. Loop events (`agent_start`, `message_update`,
//! `tool_execution_*` and the like) reach extensions as the fields of their
//! [`AgentEvent`](crate::event::AgentEvent). Three events are built where they
//! are emitted: `before_provider_request` and `before_provider_headers` pass
//! one field through a chain of handlers, and pi writes the `data` of
//! `provider_stream_event` before its `type`.
#![allow(
    missing_docs,
    reason = "variants mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use std::path::Path;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use crate::event::CompactionReason;
use crate::message::{ContentBlock, ImageContent, Message, ThinkingLevel, Usage};
use crate::model::Model;
use crate::rpc::StreamingBehavior;
use crate::session::FileEntry;

/// A pi event for extension handlers. Absent optional fields are left out,
/// as pi leaves them undefined.
#[derive(Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ExtensionEvent<'a> {
    ResourcesDiscover {
        cwd: &'a Path,
        reason: &'a str,
    },
    SessionStart {
        reason: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_session_file: Option<&'a str>,
    },
    SessionInfoChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
    },
    SessionBeforeSwitch {
        reason: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_session_file: Option<&'a str>,
    },
    SessionBeforeFork {
        entry_id: &'a str,
        /// `at` or `before`.
        position: &'a str,
    },
    SessionBeforeCompact {
        /// pi's `CompactionPreparation`, which yapi-core defines.
        preparation: Value,
        branch_entries: &'a [&'a FileEntry],
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<&'a str>,
        reason: CompactionReason,
        will_retry: bool,
    },
    SessionCompact {
        compaction_entry: &'a FileEntry,
        from_extension: bool,
        reason: CompactionReason,
        will_retry: bool,
    },
    SessionCompactFailed {
        reason: CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<&'a str>,
        aborted: bool,
        will_retry: bool,
        from_extension: bool,
    },
    SessionShutdown {
        reason: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_session_file: Option<&'a str>,
    },
    SessionBeforeTree {
        preparation: TreePreparation<'a>,
    },
    SessionTree {
        new_leaf_id: Option<&'a str>,
        old_leaf_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary_entry: Option<&'a FileEntry>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from_extension: Option<bool>,
    },
    Context {
        messages: &'a [Message],
    },
    ContextWithSystem {
        messages: &'a [Message],
    },
    AfterProviderResponse {
        status: u16,
        headers: &'a IndexMap<String, String>,
    },
    BeforeAgentStart {
        prompt: &'a str,
        images: &'a [ImageContent],
        system_prompt: &'a str,
        system_prompt_options: Value,
    },
    AgentBeforeSettle {
        outcome: &'a str,
    },
    AgentSettled,
    UiPromptStart(UiPrompt<'a>),
    UiPromptEnd(UiPrompt<'a>),
    TurnEnd {
        turn_index: u64,
        message: &'a Message,
        tool_results: &'a [Message],
        message_entry_id: &'a str,
        tool_result_entry_ids: &'a [String],
        outcome: &'a str,
    },
    MessageEnd {
        /// The message as the previous handler left it.
        message: &'a Value,
    },
    ModelSelect {
        model: &'a Model,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_model: Option<&'a Model>,
        source: &'a str,
    },
    ThinkingLevelSelect {
        level: ThinkingLevel,
        previous_level: ThinkingLevel,
    },
    UserBash {
        command: &'a str,
        exclude_from_context: bool,
        cwd: &'a Path,
    },
    Input {
        text: &'a str,
        images: &'a [ImageContent],
        source: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    ToolCall {
        tool_name: &'a str,
        tool_call_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<&'a str>,
        input: &'a Value,
    },
    ToolResult {
        tool_name: &'a str,
        tool_call_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<&'a str>,
        input: &'a Value,
        content: &'a [ContentBlock],
        /// `null` when the tool gave none.
        details: Option<&'a Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        structured_content: Option<&'a Value>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<&'a Usage>,
    },
}

impl ExtensionEvent<'_> {
    /// The event as the JSON document handlers receive.
    pub fn to_value(&self) -> Value {
        // Only a path that is not UTF-8 fails to serialize; such an event is
        // `null` and reaches no handler.
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// pi's `UIPromptEvent`: a blocking extension dialog opened or closed.
#[derive(Debug, Serialize)]
pub struct UiPrompt<'a> {
    /// Always `ui_prompt`.
    pub reason: &'a str,
    /// `select`, `confirm`, `input`, `editor` or `custom`.
    pub kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
}

/// pi's `TreePreparation`: what navigating the session tree would
/// summarize.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreePreparation<'a> {
    pub target_id: &'a str,
    pub old_leaf_id: Option<&'a str>,
    pub common_ancestor_id: Option<&'a str>,
    pub entries_to_summarize: &'a [FileEntry],
    pub user_wants_summary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<&'a str>,
    /// Left out unless set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub replace_instructions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    #[test]
    fn optional_fields_are_left_out_in_pi_order() {
        let failed = |error_message| {
            json::stringify(
                &ExtensionEvent::SessionCompactFailed {
                    reason: CompactionReason::Overflow,
                    error_message,
                    aborted: false,
                    will_retry: false,
                    from_extension: true,
                }
                .to_value(),
            )
        };
        assert_eq!(
            failed(Some("boom")),
            r#"{"type":"session_compact_failed","reason":"overflow","errorMessage":"boom","aborted":false,"willRetry":false,"fromExtension":true}"#
        );
        assert_eq!(
            failed(None),
            r#"{"type":"session_compact_failed","reason":"overflow","aborted":false,"willRetry":false,"fromExtension":true}"#
        );
    }

    #[test]
    fn tree_preparation_sets_only_given_options() {
        let tree = |custom_instructions, replace_instructions, label| {
            json::stringify(
                &ExtensionEvent::SessionBeforeTree {
                    preparation: TreePreparation {
                        target_id: "b",
                        old_leaf_id: Some("a"),
                        common_ancestor_id: None,
                        entries_to_summarize: &[],
                        user_wants_summary: true,
                        custom_instructions,
                        replace_instructions,
                        label,
                    },
                }
                .to_value(),
            )
        };
        assert_eq!(
            tree(None, false, None),
            r#"{"type":"session_before_tree","preparation":{"targetId":"b","oldLeafId":"a","commonAncestorId":null,"entriesToSummarize":[],"userWantsSummary":true}}"#
        );
        assert_eq!(
            tree(Some("focus"), true, Some("l")),
            r#"{"type":"session_before_tree","preparation":{"targetId":"b","oldLeafId":"a","commonAncestorId":null,"entriesToSummarize":[],"userWantsSummary":true,"customInstructions":"focus","replaceInstructions":true,"label":"l"}}"#
        );
    }

    #[test]
    fn ui_prompt_follows_the_tag() {
        let prompt = UiPrompt {
            reason: "ui_prompt",
            kind: "select",
            title: None,
        };
        assert_eq!(
            json::stringify(&ExtensionEvent::UiPromptEnd(prompt).to_value()),
            r#"{"type":"ui_prompt_end","reason":"ui_prompt","kind":"select"}"#
        );
    }
}
