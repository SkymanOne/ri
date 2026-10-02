//! Session message roles beyond the provider's four, and their conversion.

use ri_types::message::{
    BashExecutionMessage, Content, ContentBlock, Message, TextContent, UserMessage,
};

/// Wraps a compaction summary when it is sent to the model.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Closes [`COMPACTION_SUMMARY_PREFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Wraps a branch summary when it is sent to the model.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// Closes [`BRANCH_SUMMARY_PREFIX`].
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// How a user `!command` reads to the model.
pub fn bash_execution_text(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text += "(no output)";
    } else {
        text += &format!("```\n{}\n```", message.output);
    }
    if message.cancelled {
        text += "\n\n(command cancelled)";
    } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
        text += &format!("\n\nCommand exited with code {code}");
    }
    if message.truncated
        && let Some(path) = &message.full_output_path
    {
        text += &format!("\n\n[Output truncated. Full output: {path}]");
    }
    text
}

fn user_text(text: String, timestamp: u64) -> Message {
    Message::User(UserMessage {
        content: Content::Blocks(vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })]),
        timestamp,
    })
}

/// Converts session messages to provider roles: bash executions, custom messages
/// and summaries become user messages; `!!` executions are dropped.
pub fn convert_to_llm(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::BashExecution(bash) => (bash.exclude_from_context != Some(true))
                .then(|| user_text(bash_execution_text(&bash), bash.timestamp)),
            Message::Custom(custom) => Some(Message::User(UserMessage {
                content: match custom.content {
                    Content::Text(text) => Content::Blocks(vec![ContentBlock::Text(TextContent {
                        text,
                        text_signature: None,
                    })]),
                    blocks => blocks,
                },
                timestamp: custom.timestamp,
            })),
            Message::BranchSummary(summary) => Some(user_text(
                format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    summary.summary
                ),
                summary.timestamp,
            )),
            Message::CompactionSummary(summary) => Some(user_text(
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    summary.summary
                ),
                summary.timestamp,
            )),
            other => Some(other),
        })
        .collect()
}
