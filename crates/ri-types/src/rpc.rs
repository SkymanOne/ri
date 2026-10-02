//! RPC mode: commands read from stdin and the responses written to stdout, as
//! in `modes/rpc/rpc-types.ts` of pi `v1.0.0`. Events use the JSON mode shapes
//! in [`crate::event`].
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{ImageContent, ThinkingLevel};
use crate::session::FileEntry;
use crate::settings::QueueMode;

/// How a prompt sent while the agent runs is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    /// Delivered after the current tool calls.
    Steer,
    /// Delivered when the run would otherwise end.
    FollowUp,
}

/// What became of a prompt once it passed preflight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptDisposition {
    /// A run started.
    Started,
    /// Queued behind the running agent.
    Queued,
    /// Consumed without a run, such as by an extension command.
    Handled,
}

/// A command line on stdin, without its `id`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RpcCommand {
    Prompt {
        message: String,
        #[serde(default)]
        images: Vec<ImageContent>,
        #[serde(default)]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        message: String,
        #[serde(default)]
        images: Vec<ImageContent>,
    },
    FollowUp {
        message: String,
        #[serde(default)]
        images: Vec<ImageContent>,
    },
    Abort,
    ClearQueue,
    NewSession {
        #[serde(default)]
        parent_session: Option<String>,
    },
    GetState,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    GetAvailableModels,
    SetThinkingLevel {
        level: ThinkingLevel,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    SetSteeringMode {
        mode: QueueMode,
    },
    SetFollowUpMode {
        mode: QueueMode,
    },
    Compact {
        #[serde(default)]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    SetAutoRetry {
        enabled: bool,
    },
    AbortRetry,
    Bash {
        command: String,
        #[serde(default)]
        exclude_from_context: Option<bool>,
    },
    AbortBash,
    GetSessionStats,
    ExportHtml {
        #[serde(default)]
        output_path: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    Fork {
        entry_id: String,
    },
    Clone,
    GetForkMessages,
    GetEntries {
        #[serde(default)]
        since: Option<String>,
    },
    GetTree,
    GetLastAssistantText,
    SetSessionName {
        name: String,
    },
    GetMessages,
    GetCommands,
    /// Any other `type`.
    #[serde(other)]
    Unknown,
}

/// A response line: `data` is serialized JSON, or absent.
pub fn response_line(
    id: Option<&Value>,
    command: &str,
    outcome: Result<Option<&str>, &str>,
) -> String {
    let mut line = String::from("{");
    if let Some(id) = id
        && let Ok(id) = crate::json::to_string(id)
    {
        line.push_str("\"id\":");
        line.push_str(&id);
        line.push(',');
    }
    line.push_str("\"type\":\"response\",\"command\":");
    line.push_str(&crate::json::to_string(command).unwrap_or_else(|_| "\"\"".into()));
    match outcome {
        Ok(data) => {
            line.push_str(",\"success\":true");
            if let Some(data) = data {
                line.push_str(",\"data\":");
                line.push_str(data);
            }
        }
        Err(error) => {
            line.push_str(",\"success\":false,\"error\":");
            line.push_str(&crate::json::to_string(error).unwrap_or_else(|_| "\"\"".into()));
        }
    }
    line.push('}');
    line
}

/// `get_state` data.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

/// Token totals in [`SessionStats`].
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// How full the context window is; `tokens` and `percent` are `null` while
/// unknown.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// `get_session_stats` data.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: TokenTotals,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

/// What a user `!` command produced: the `bash` response data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    /// Combined, sanitized output; the tail when truncated.
    pub output: String,
    /// The exit code; absent when cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The command was cancelled.
    pub cancelled: bool,
    /// The output was truncated.
    pub truncated: bool,
    /// The full output, when it was too long to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// A fork point in `get_fork_messages` data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkMessage {
    pub entry_id: String,
    pub text: String,
}

/// Where a resource was found, as pi's `SourceInfo`: `source` is `auto` for
/// discovered files, `cli` for command-line paths and the package for
/// package resources; `scope` is `user`, `project` or `temporary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

/// Where a [`SlashCommand`] comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandSource {
    Extension,
    Prompt,
    Skill,
}

/// A command a prompt can invoke, in `get_commands` data.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: CommandSource,
    pub source_info: SourceInfo,
}

/// One node of `get_tree` data, as pi's `SessionTreeNode`.
pub struct TreeNode<'a> {
    pub entry: &'a FileEntry,
    /// Indexes into the same node list.
    pub children: Vec<usize>,
    pub label: Option<&'a str>,
    pub label_timestamp: Option<&'a str>,
}

/// Serializes the trees under `roots` without recursion, so a long linear
/// session cannot exhaust the stack.
pub fn tree_json(nodes: &[TreeNode<'_>], roots: &[usize]) -> serde_json::Result<String> {
    enum Step {
        Open(usize),
        Close(usize),
        Comma,
    }
    let mut out = String::from("[");
    let mut stack: Vec<Step> = Vec::new();
    let push_list = |stack: &mut Vec<Step>, list: &[usize]| {
        for (index, &node) in list.iter().enumerate().rev() {
            stack.push(Step::Open(node));
            if index > 0 {
                stack.push(Step::Comma);
            }
        }
    };
    push_list(&mut stack, roots);
    while let Some(step) = stack.pop() {
        match step {
            Step::Comma => out.push(','),
            Step::Open(index) => {
                let node = &nodes[index];
                out.push_str("{\"entry\":");
                out.push_str(&crate::json::to_string(node.entry)?);
                out.push_str(",\"children\":[");
                stack.push(Step::Close(index));
                push_list(&mut stack, &node.children);
            }
            Step::Close(index) => {
                out.push(']');
                let node = &nodes[index];
                if let Some(label) = node.label {
                    out.push_str(",\"label\":");
                    out.push_str(&crate::json::to_string(label)?);
                }
                if let Some(timestamp) = node.label_timestamp {
                    out.push_str(",\"labelTimestamp\":");
                    out.push_str(&crate::json::to_string(timestamp)?);
                }
                out.push('}');
            }
        }
    }
    out.push(']');
    Ok(out)
}

/// An `extension_ui_response` line on stdin.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ExtensionUiResponse {
    pub id: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub confirmed: Option<bool>,
    #[serde(default)]
    pub cancelled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_like_pi() {
        let prompt: RpcCommand = serde_json::from_str(
            r#"{"type":"prompt","message":"hi","streamingBehavior":"followUp"}"#,
        )
        .unwrap();
        assert_eq!(
            prompt,
            RpcCommand::Prompt {
                message: "hi".into(),
                images: Vec::new(),
                streaming_behavior: Some(StreamingBehavior::FollowUp),
            }
        );
        let mode: RpcCommand =
            serde_json::from_str(r#"{"type":"set_steering_mode","mode":"one-at-a-time"}"#).unwrap();
        assert_eq!(
            mode,
            RpcCommand::SetSteeringMode {
                mode: QueueMode::OneAtATime
            }
        );
        let unknown: RpcCommand = serde_json::from_str(r#"{"type":"dance"}"#).unwrap();
        assert_eq!(unknown, RpcCommand::Unknown);
    }

    #[test]
    fn responses_match_pi() {
        let id = Value::from("7");
        assert_eq!(
            response_line(Some(&id), "abort", Ok(None)),
            r#"{"id":"7","type":"response","command":"abort","success":true}"#
        );
        assert_eq!(
            response_line(None, "cycle_model", Ok(Some("null"))),
            r#"{"type":"response","command":"cycle_model","success":true,"data":null}"#
        );
        assert_eq!(
            response_line(Some(&Value::from(3)), "x", Err("Unknown command: x")),
            r#"{"id":3,"type":"response","command":"x","success":false,"error":"Unknown command: x"}"#
        );
    }

    #[test]
    fn deep_trees_serialize_without_recursion() {
        let entry: FileEntry = serde_json::from_str(
            r#"{"type":"label","id":"a","parentId":null,"timestamp":"t","targetId":"b"}"#,
        )
        .unwrap();
        let depth = 200_000;
        let nodes: Vec<TreeNode<'_>> = (0..depth)
            .map(|index| TreeNode {
                entry: &entry,
                children: if index + 1 < depth {
                    vec![index + 1]
                } else {
                    Vec::new()
                },
                label: (index == 0).then_some("start"),
                label_timestamp: None,
            })
            .collect();
        let json = tree_json(&nodes, &[0]).unwrap();
        assert!(json.ends_with(&format!(
            "{}],\"label\":\"start\"}}]",
            "]}".repeat(depth - 1)
        )));
        let small = tree_json(&nodes[depth - 2..depth - 1], &[]).unwrap();
        assert_eq!(small, "[]");
    }
}
