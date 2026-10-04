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

const IMAGES_BLOCKED: &str = "Image reading is disabled.";

/// pi's `blockImages` filter: images in user and tool result messages become
/// a notice, once for each run of images.
pub fn block_images(messages: Vec<Message>) -> Vec<Message> {
    fn filter(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
        let mut out: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
        for block in blocks {
            let block = match block {
                ContentBlock::Image(_) => ContentBlock::Text(TextContent {
                    text: IMAGES_BLOCKED.to_owned(),
                    text_signature: None,
                }),
                other => other,
            };
            let repeated = matches!(
                (&block, out.last()),
                (ContentBlock::Text(text), Some(ContentBlock::Text(previous)))
                    if text.text == IMAGES_BLOCKED && previous.text == IMAGES_BLOCKED
            );
            if !repeated {
                out.push(block);
            }
        }
        out
    }
    let has_image = |blocks: &[ContentBlock]| {
        blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    };
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                if let Content::Blocks(blocks) = &mut user.content
                    && has_image(blocks)
                {
                    *blocks = filter(std::mem::take(blocks));
                }
                Message::User(user)
            }
            Message::ToolResult(mut result) => {
                if has_image(&result.content) {
                    result.content = filter(std::mem::take(&mut result.content));
                }
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_images_as_pi() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call",
            "toolName": "read",
            "content": [
                {"type": "image", "data": "AA==", "mimeType": "image/png"},
                {"type": "image", "data": "AA==", "mimeType": "image/png"},
                {"type": "text", "text": "after"},
                {"type": "image", "data": "AA==", "mimeType": "image/png"}
            ],
            "isError": false,
            "timestamp": 0
        }))
        .unwrap();
        let Message::ToolResult(result) = &block_images(vec![message])[0] else {
            panic!("not a tool result");
        };
        let texts: Vec<&str> = result
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.text.as_str(),
                _ => "image",
            })
            .collect();
        assert_eq!(texts, [IMAGES_BLOCKED, "after", IMAGES_BLOCKED]);
    }
}
