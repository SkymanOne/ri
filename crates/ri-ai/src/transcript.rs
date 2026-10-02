//! The transcript a provider receives: messages in which system messages carry the
//! prompt and tool declarations.
//!
//! The first message, when it is a system message, is the initial prompt. Later
//! system messages patch sections and add or remove tools. Providers that cannot
//! take system messages mid-conversation get them collapsed into the first one.

use indexmap::IndexMap;
use ri_types::message::{
    AssistantMessage, ContentBlock, Message, StopReason, SystemMessage, TextContent,
    ToolDeclaration, ToolResultMessage,
};
use ri_types::model::Model;

/// The leading system message, if the transcript starts with one.
pub fn initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    match messages.first() {
        Some(Message::System(system)) => Some(system),
        _ => None,
    }
}

fn system_messages(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|message| match message {
        Message::System(system) => Some(system),
        _ => None,
    })
}

/// Tools active at the end of the transcript, in declaration order.
pub fn current_tools(messages: &[Message]) -> Vec<ToolDeclaration> {
    let mut tools: IndexMap<String, ToolDeclaration> = IndexMap::new();
    for system in system_messages(messages) {
        for tool in system.tools_removed.iter().flatten() {
            tools.shift_remove(&tool.name);
        }
        for tool in system.tools_added.iter().flatten() {
            tools.insert(tool.name.clone(), tool.clone());
        }
    }
    tools.into_values().collect()
}

/// Every tool ever declared, with its latest declaration.
pub fn declared_tools(messages: &[Message]) -> Vec<ToolDeclaration> {
    let mut tools: IndexMap<String, ToolDeclaration> = IndexMap::new();
    for system in system_messages(messages) {
        for tool in system.tools_added.iter().flatten() {
            tools.insert(tool.name.clone(), tool.clone());
        }
    }
    tools.into_values().collect()
}

/// Whether two declarations serialize identically.
pub fn declarations_equal(left: &ToolDeclaration, right: &ToolDeclaration) -> bool {
    ri_types::json::to_string(left).ok() == ri_types::json::to_string(right).ok()
}

/// Whether a tool name is declared twice with different declarations.
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: IndexMap<&str, &ToolDeclaration> = IndexMap::new();
    for system in system_messages(messages) {
        for tool in system.tools_added.iter().flatten() {
            if let Some(previous) = declared.insert(&tool.name, tool)
                && !declarations_equal(previous, tool)
            {
                return true;
            }
        }
    }
    false
}

/// All system messages folded into one: their text joined, sections merged (a
/// removed then re-added section moves to the end), and the current tools. `None`
/// when the transcript has no system message and no tools.
pub fn current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content = Vec::new();
    let mut sections: IndexMap<String, String> = IndexMap::new();
    let mut timestamp = None;
    for system in system_messages(messages) {
        timestamp.get_or_insert(system.timestamp);
        let text = system.content.text("\n");
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in system.sections.iter().flatten() {
            match value {
                None => {
                    sections.shift_remove(name);
                }
                Some(value) => {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: ri_types::message::Content::Text(content.join("\n\n")),
        sections: (!sections.is_empty()).then(|| {
            sections
                .into_iter()
                .map(|(name, text)| (name, Some(text)))
                .collect()
        }),
        timestamp: timestamp.unwrap_or(0),
        tools_added: (!tools.is_empty()).then_some(tools),
        tools_removed: None,
    })
}

/// The transcript as a provider sees it: unchanged when it accepts mid-conversation
/// system messages, otherwise with every system message collapsed into the first.
pub fn resolve_transcript(messages: &[Message], supports_mid_convo_system: bool) -> Vec<Message> {
    if supports_mid_convo_system {
        return messages.to_vec();
    }
    let head = current_system_message(messages);
    head.map(Message::System)
        .into_iter()
        .chain(
            messages
                .iter()
                .filter(|message| !matches!(message, Message::System(_)))
                .cloned(),
        )
        .collect()
}

const NON_VISION_USER_IMAGE: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE: &str = "(tool image omitted: model does not support images)";

fn replace_images(content: &[ContentBlock], placeholder: &str) -> Vec<ContentBlock> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        if matches!(block, ContentBlock::Image(_)) {
            if !previous_was_placeholder {
                result.push(text_block(placeholder));
            }
            previous_was_placeholder = true;
            continue;
        }
        previous_was_placeholder =
            matches!(block, ContentBlock::Text(text) if text.text == placeholder);
        result.push(block.clone());
    }
    result
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

/// Rewrites a transcript for `model`:
/// - images become placeholders when the model is text-only;
/// - thinking from other models becomes text, or is dropped when empty or redacted;
/// - tool-call ids are normalized for the target API and tool results follow;
/// - failed and aborted assistant messages are dropped;
/// - tool calls without a result get an error result before the next turn, and system
///   messages between a call and its results move after the results.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&dyn Fn(&str) -> String>,
    now_ms: u64,
) -> Vec<Message> {
    let mut id_map: IndexMap<String, String> = IndexMap::new();
    let text_only = !model.accepts_images();

    let transformed: Vec<Message> = messages
        .iter()
        .map(|message| match message {
            Message::User(user) if text_only => {
                let mut user = user.clone();
                if let ri_types::message::Content::Blocks(blocks) = &user.content {
                    user.content = ri_types::message::Content::Blocks(replace_images(
                        blocks,
                        NON_VISION_USER_IMAGE,
                    ));
                }
                Message::User(user)
            }
            Message::ToolResult(result) => {
                let mut result = result.clone();
                if text_only {
                    result.content = replace_images(&result.content, NON_VISION_TOOL_IMAGE);
                }
                if let Some(id) = id_map.get(&result.tool_call_id) {
                    result.tool_call_id = id.clone();
                }
                Message::ToolResult(result)
            }
            Message::Assistant(assistant) => Message::Assistant(Box::new(transform_assistant(
                assistant,
                model,
                normalize_tool_call_id,
                &mut id_map,
            ))),
            other => other.clone(),
        })
        .collect();

    let mut result = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    let close = |result: &mut Vec<Message>,
                 pending: &mut Vec<(String, String)>,
                 answered: &mut Vec<String>,
                 held: &mut Vec<Message>| {
        for (id, name) in pending.drain(..) {
            if !answered.contains(&id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![text_block("No result provided")],
                    details: None,
                    usage: None,
                    is_error: true,
                    timestamp: now_ms,
                    nested_calls: None,
                }));
            }
        }
        answered.clear();
        result.append(held);
    };

    for message in transformed {
        match &message {
            Message::Assistant(assistant) => {
                close(&mut result, &mut pending, &mut answered, &mut held);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let calls: Vec<(String, String)> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    pending = calls;
                    answered.clear();
                }
                result.push(message);
            }
            Message::ToolResult(tool_result) => {
                answered.push(tool_result.tool_call_id.clone());
                result.push(message);
            }
            Message::System(_) if !pending.is_empty() => held.push(message),
            Message::User(_) => {
                close(&mut result, &mut pending, &mut answered, &mut held);
                result.push(message);
            }
            _ => result.push(message),
        }
    }
    close(&mut result, &mut pending, &mut answered, &mut held);
    result
}

fn transform_assistant(
    assistant: &AssistantMessage,
    model: &Model,
    normalize_tool_call_id: Option<&dyn Fn(&str) -> String>,
    id_map: &mut IndexMap<String, String>,
) -> AssistantMessage {
    let same_model = assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id;
    let mut content = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking(thinking) => {
                if thinking.redacted == Some(true) {
                    if same_model {
                        content.push(block.clone());
                    }
                } else if same_model
                    && thinking
                        .thinking_signature
                        .as_deref()
                        .is_some_and(|signature| !signature.is_empty())
                {
                    content.push(block.clone());
                } else if thinking.thinking.trim().is_empty() {
                } else if same_model {
                    content.push(block.clone());
                } else {
                    content.push(text_block(&thinking.thinking));
                }
            }
            ContentBlock::Text(text) => {
                if same_model {
                    content.push(block.clone());
                } else {
                    content.push(text_block(&text.text));
                }
            }
            ContentBlock::ToolCall(call) => {
                let mut call = call.clone();
                if !same_model {
                    call.thought_signature = None;
                    if let Some(normalize) = normalize_tool_call_id {
                        let id = normalize(&call.id);
                        if id != call.id {
                            id_map.insert(call.id.clone(), id.clone());
                            call.id = id;
                        }
                    }
                }
                content.push(ContentBlock::ToolCall(call));
            }
            ContentBlock::Image(_) => content.push(block.clone()),
        }
    }
    AssistantMessage {
        content,
        ..assistant.clone()
    }
}

const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Length in UTF-16 code units, as JavaScript counts it.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn tokens(chars: usize) -> u64 {
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

fn content_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => js_len(&text.text),
            ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum()
}

/// Rough token count of one message: characters / 4, images at 4800 characters.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            let tools = |json: Option<String>| json.map_or(0, |json| tokens(js_len(&json)));
            tokens(js_len(&system.text()))
                + tools(
                    system
                        .tools_added
                        .as_ref()
                        .filter(|tools| !tools.is_empty())
                        .and_then(|tools| ri_types::json::to_string(tools).ok()),
                )
                + tools(
                    system
                        .tools_removed
                        .as_ref()
                        .filter(|tools| !tools.is_empty())
                        .and_then(|tools| ri_types::json::to_string(tools).ok()),
                )
        }
        Message::User(user) => match &user.content {
            ri_types::message::Content::Text(text) => tokens(js_len(text)),
            ri_types::message::Content::Blocks(blocks) => tokens(content_chars(blocks)),
        },
        Message::ToolResult(result) => tokens(content_chars(&result.content)),
        Message::Assistant(assistant) => tokens(
            assistant
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text(text) => js_len(&text.text),
                    ContentBlock::Thinking(thinking) => js_len(&thinking.thinking),
                    ContentBlock::ToolCall(call) => {
                        js_len(&call.name)
                            + ri_types::json::to_string(&call.arguments)
                                .map_or(0, |json| js_len(&json))
                    }
                    ContentBlock::Image(_) => 0,
                })
                .sum(),
        ),
        // Other roles are converted to user messages before they reach a provider.
        _ => 0,
    }
}

/// Tokens the transcript occupies: the last successful assistant usage plus
/// estimates for the messages after it, or estimates throughout.
pub fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let mut latest_prefix_timestamp: Option<u64> = None;
    let mut last_usage: Option<(u64, usize)> = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let applies =
                latest_prefix_timestamp.is_none_or(|latest| assistant.timestamp >= latest);
            let usage = &assistant.usage;
            let context = usage
                .total_tokens
                .filter(|total| *total > 0)
                .unwrap_or(usage.input + usage.output + usage.cache_read + usage.cache_write);
            if applies
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                )
                && context > 0
            {
                last_usage = Some((context, index));
            }
        }
        let timestamp = message.timestamp();
        latest_prefix_timestamp =
            Some(latest_prefix_timestamp.map_or(timestamp, |latest| latest.max(timestamp)));
    }
    match last_usage {
        Some((usage_tokens, index)) => {
            usage_tokens
                + messages[index + 1..]
                    .iter()
                    .map(estimate_message_tokens)
                    .sum::<u64>()
        }
        None => messages.iter().map(estimate_message_tokens).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ri_types::message::{Content, ToolReference};
    use serde_json::json;

    fn system(value: serde_json::Value) -> Message {
        serde_json::from_value(value).unwrap()
    }

    fn tool(name: &str) -> serde_json::Value {
        json!({"name": name, "description": name, "parameters": {"type": "object"}})
    }

    #[test]
    fn collapses_system_messages() {
        let messages = vec![
            system(
                json!({"role":"system","content":"","sections":{"a":"A","b":"B"},"timestamp":5,"toolsAdded":[tool("read"),tool("bash")]}),
            ),
            system(json!({"role":"user","content":"hi","timestamp":6})),
            system(
                json!({"role":"system","content":"","sections":{"a":null},"timestamp":7,"toolsRemoved":[{"name":"read"}]}),
            ),
            system(
                json!({"role":"system","content":"note","sections":{"a":"A2"},"timestamp":8,"toolsAdded":[tool("read")]}),
            ),
        ];
        let collapsed = resolve_transcript(&messages, false);
        assert_eq!(collapsed.len(), 2);
        let Message::System(head) = &collapsed[0] else {
            panic!("head is not a system message")
        };
        assert_eq!(head.timestamp, 5);
        assert_eq!(head.content, Content::Text("note".into()));
        let names: Vec<_> = head.sections.as_ref().unwrap().keys().cloned().collect();
        assert_eq!(names, ["b", "a"]);
        assert_eq!(head.text(), "note\n\nB\n\nA2");
        let tools: Vec<_> = head
            .tools_added
            .as_ref()
            .unwrap()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(tools, ["bash", "read"]);
        assert_eq!(resolve_transcript(&messages, true), messages);
        let _ = ToolReference {
            name: String::new(),
        };
    }

    #[test]
    fn closes_unanswered_tool_calls_and_drops_failures() {
        let model: Model = serde_json::from_value(json!({
            "id":"m","name":"M","api":"anthropic-messages","provider":"p","baseUrl":"u","reasoning":false,
            "input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":1,"maxTokens":1
        }))
        .unwrap();
        let assistant = |stop: &str, calls: serde_json::Value| {
            system(
                json!({"role":"assistant","content":calls,"api":"openai-completions","provider":"o","model":"x",
                "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
                "stopReason":stop,"timestamp":1}),
            )
        };
        let messages = vec![
            assistant(
                "toolUse",
                json!([{"type":"thinking","thinking":"hmm","thinkingSignature":"sig"},{"type":"toolCall","id":"call|1","name":"read","arguments":{}}]),
            ),
            system(json!({"role":"system","content":"later","timestamp":2})),
            system(
                json!({"role":"user","content":[{"type":"image","data":"AA","mimeType":"image/png"}],"timestamp":3}),
            ),
            assistant("error", json!([])),
        ];
        let normalize = |id: &str| id.replace('|', "_");
        let out = transform_messages(&messages, &model, Some(&normalize), 9);
        let roles: Vec<_> = out
            .iter()
            .map(|m| match m {
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
                Message::System(_) => "system",
                Message::User(_) => "user",
                _ => "other",
            })
            .collect();
        assert_eq!(roles, ["assistant", "toolResult", "system", "user"]);
        let Message::Assistant(first) = &out[0] else {
            panic!()
        };
        assert_eq!(
            first.content[0],
            ContentBlock::Text(TextContent {
                text: "hmm".into(),
                text_signature: None
            })
        );
        let Message::ToolResult(result) = &out[1] else {
            panic!()
        };
        assert_eq!(result.tool_call_id, "call_1");
        assert!(result.is_error);
        let Message::User(user) = &out[3] else {
            panic!()
        };
        assert_eq!(
            user.content,
            Content::Blocks(vec![text_block(NON_VISION_USER_IMAGE)])
        );
    }

    #[test]
    fn estimates_like_pi() {
        let messages = vec![system(
            json!({"role":"user","content":"héllo wörld!","timestamp":1}),
        )];
        assert_eq!(estimate_context_tokens(&messages), 3);
    }
}
