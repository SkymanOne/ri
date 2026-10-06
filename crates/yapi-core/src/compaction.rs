//! Compaction: summarize older context so a long session fits the model.
//!
//! Port of `packages/coding-agent/src/core/compaction/compaction.ts` and
//! `compaction/utils.ts` in pi `v1.0.0`. The functions here are pure apart from
//! the summary request; the agent session decides when to compact and records the
//! result.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_ai::api::Apis;
use yapi_ai::errors::{is_retryable_assistant_error, retry_delay_ms};
use yapi_ai::registry::Auth;
use yapi_ai::stream::{CacheRetention, Request, StreamOptions};
use yapi_types::message::{
    AssistantMessage, Content, ContentBlock, Message, StopReason, SystemMessage, ThinkingLevel,
    Usage, UserMessage,
};
use yapi_types::model::Model;
use yapi_types::session::FileEntry;
use yapi_types::settings::Settings;

use crate::messages::convert_to_llm;
use crate::session::{ProjectedEntry, Projection, entry_messages};
use crate::time::{now_ms, uuid_v7};

/// When and how much to compact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Automatic compaction is on.
    pub enabled: bool,
    /// Tokens kept free for the response; compaction starts when less is left.
    pub reserve_tokens: u64,
    /// Recent tokens kept verbatim.
    pub keep_recent_tokens: u64,
}

impl CompactionSettings {
    /// The settings for `model`: its override, the ordinary setting, the default.
    pub fn resolve(settings: &Settings, model: Option<&Model>) -> CompactionSettings {
        let compaction = settings.compaction.as_ref();
        let key = model.map(|model| format!("{}/{}", model.provider, model.id));
        let overrides = key.as_ref().and_then(|key| {
            compaction
                .and_then(|c| c.model_overrides.as_ref())
                .and_then(|overrides| overrides.get(key))
        });
        CompactionSettings {
            enabled: compaction.and_then(|c| c.enabled).unwrap_or(true),
            reserve_tokens: overrides
                .and_then(|o| o.reserve_tokens)
                .or_else(|| compaction.and_then(|c| c.reserve_tokens))
                .unwrap_or(16384),
            keep_recent_tokens: overrides
                .and_then(|o| o.keep_recent_tokens)
                .or_else(|| compaction.and_then(|c| c.keep_recent_tokens))
                .unwrap_or(20000),
        }
    }
}

/// Retry policy for summary requests, from `settings.retry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries are on.
    pub enabled: bool,
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// Delay before the first retry; doubles each time.
    pub base_delay_ms: u64,
    /// Cap on one delay.
    pub max_delay_ms: u64,
}

impl RetryPolicy {
    /// The policy in `settings`, with pi's defaults.
    pub fn resolve(settings: &Settings) -> RetryPolicy {
        let retry = settings.retry.as_ref();
        RetryPolicy {
            enabled: retry.and_then(|r| r.enabled).unwrap_or(true),
            max_retries: retry.and_then(|r| r.max_retries).unwrap_or(3),
            base_delay_ms: retry.and_then(|r| r.base_delay_ms).unwrap_or(2000),
            max_delay_ms: retry
                .and_then(|r| r.max_agent_delay_ms)
                .unwrap_or(yapi_ai::errors::DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
        }
    }

    /// The delay before retry `attempt` (1-based).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        retry_delay_ms(self.base_delay_ms, Some(self.max_delay_ms), attempt)
    }
}

/// Tokens a response's usage says the context held.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    match usage.total_tokens {
        Some(total) if total > 0 => total,
        _ => usage.input + usage.output + usage.cache_read + usage.cache_write,
    }
}

fn assistant_usage(message: &Message) -> Option<&Usage> {
    match message {
        Message::Assistant(assistant)
            if !matches!(
                assistant.stop_reason,
                StopReason::Aborted | StopReason::Error
            ) && calculate_context_tokens(&assistant.usage) > 0 =>
        {
            Some(&assistant.usage)
        }
        _ => None,
    }
}

const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn blocks_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => yapi_types::js::len(&text.text),
            ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum()
}

fn content_chars(content: &Content) -> usize {
    match content {
        Content::Text(text) => yapi_types::js::len(text),
        Content::Blocks(blocks) => blocks_chars(blocks),
    }
}

/// Estimated tokens of one message: characters / 4, rounded up; images count
/// 4800 characters.
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::System(system) => {
            let sections: usize = system
                .sections
                .iter()
                .flatten()
                .filter_map(|(_, text)| text.as_deref())
                .map(yapi_types::js::len)
                .sum();
            let tools = system
                .tools_added
                .as_ref()
                .and_then(|tools| yapi_types::json::to_string(tools).ok())
                .map_or(0, |json| yapi_types::js::len(&json));
            content_chars(&system.content) + sections + tools
        }
        Message::User(user) => content_chars(&user.content),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => yapi_types::js::len(&text.text),
                ContentBlock::Thinking(thinking) => yapi_types::js::len(&thinking.thinking),
                ContentBlock::ToolCall(call) => {
                    yapi_types::js::len(&call.name)
                        + yapi_types::json::to_string(&call.arguments)
                            .map_or(0, |json| yapi_types::js::len(&json))
                }
                ContentBlock::Image(_) => 0,
            })
            .sum(),
        Message::Custom(custom) => content_chars(&custom.content),
        Message::ToolResult(result) => blocks_chars(&result.content),
        Message::BashExecution(bash) => {
            yapi_types::js::len(&bash.command) + yapi_types::js::len(&bash.output)
        }
        Message::BranchSummary(summary) => yapi_types::js::len(&summary.summary),
        Message::CompactionSummary(summary) => yapi_types::js::len(&summary.summary),
    };
    chars.div_ceil(4) as u64
}

/// Estimated context size: the last valid usage plus estimates for what follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextEstimate {
    /// Total tokens.
    pub tokens: u64,
    /// Tokens from the last usage.
    pub usage_tokens: u64,
    /// Estimated tokens after it.
    pub trailing_tokens: u64,
    /// Index of the message whose usage was used.
    pub last_usage_index: Option<usize>,
}

/// Estimates context tokens from the last valid usage and the messages after it.
pub fn estimate_context_tokens(messages: &[Message]) -> ContextEstimate {
    let usage = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| assistant_usage(message).map(|usage| (index, usage)));
    match usage {
        None => {
            let tokens = messages.iter().map(estimate_tokens).sum();
            ContextEstimate {
                tokens,
                usage_tokens: 0,
                trailing_tokens: tokens,
                last_usage_index: None,
            }
        }
        Some((index, usage)) => {
            let usage_tokens = calculate_context_tokens(usage);
            let trailing: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
            ContextEstimate {
                tokens: usage_tokens + trailing,
                usage_tokens,
                trailing_tokens: trailing,
                last_usage_index: Some(index),
            }
        }
    }
}

/// Estimates projected context without trusting usage recorded before a later
/// context edit or compaction.
pub fn estimate_projected_context_tokens(
    projection: &Projection<'_>,
    branch: &[&FileEntry],
) -> ContextEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    if let Some(usage_index) = estimate.last_usage_index {
        let mut next = 0;
        let mut usage_entry = None;
        for entry in &projection.entries {
            next += entry.messages.len();
            if usage_index < next {
                usage_entry = entry.source.meta().map(|meta| meta.id.as_str());
                break;
            }
        }
        let usage_position = usage_entry.and_then(|id| {
            branch
                .iter()
                .position(|entry| entry.meta().is_some_and(|meta| meta.id == id))
        });
        let invalidating = branch.iter().rposition(|entry| {
            matches!(entry, FileEntry::ContextEdit(_) | FileEntry::Compaction(_))
        });
        let after = match (usage_position, invalidating) {
            (Some(usage), Some(invalid)) => usage > invalid,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if after {
            return estimate;
        }
    }
    let system = yapi_ai::transcript::current_system_message(&projection.messages);
    let mut tokens = system
        .map(|system| estimate_tokens(&Message::System(system)))
        .unwrap_or(0);
    tokens += projection
        .messages
        .iter()
        .filter(|message| !matches!(message, Message::System(_)))
        .map(estimate_tokens)
        .sum::<u64>();
    ContextEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// Whether the context leaves less than the reserve free.
pub fn should_compact(tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
    settings.enabled && (tokens as i128) > context_window as i128 - settings.reserve_tokens as i128
}

fn is_cut_point(message: &Message) -> bool {
    matches!(
        message,
        Message::User(_)
            | Message::Assistant(_)
            | Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_)
    )
}

fn is_turn_start(message: &Message) -> bool {
    matches!(
        message,
        Message::User(_)
            | Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_)
    )
}

fn is_compaction(entry: &ProjectedEntry<'_>) -> bool {
    matches!(entry.source, FileEntry::Compaction(_))
}

fn starts_turn(entry: &ProjectedEntry<'_>) -> bool {
    !is_compaction(entry) && entry.messages.iter().any(is_turn_start)
}

/// Where to cut the projected entries `start..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CutPoint {
    first_kept: usize,
    turn_start: Option<usize>,
    split_turn: bool,
}

fn find_cut_point(
    entries: &[ProjectedEntry<'_>],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
) -> CutPoint {
    let cut_points: Vec<usize> = (start..end)
        .filter(|&index| {
            !is_compaction(&entries[index]) && entries[index].messages.iter().any(is_cut_point)
        })
        .collect();
    let Some(&first) = cut_points.first() else {
        return CutPoint {
            first_kept: start,
            turn_start: None,
            split_turn: false,
        };
    };
    let mut accumulated = 0;
    let mut exceeded = false;
    let mut cut = first;
    for index in (start..end).rev() {
        let tokens: u64 = entries[index].messages.iter().map(estimate_tokens).sum();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            exceeded = true;
            cut = cut_points
                .iter()
                .copied()
                .find(|&candidate| candidate >= index)
                .unwrap_or(cut_points[cut_points.len() - 1]);
            break;
        }
    }

    // A recovery attempt and its omission edits are invisible after the last
    // visible input; move past them only when the whole suffix is such an attempt.
    let suffix = &entries[(cut + 1).min(end)..end];
    let intrinsically_visible = |entry: &ProjectedEntry<'_>| {
        !matches!(entry.source, FileEntry::ContextEdit(_))
            && !entry_messages(entry.source).is_empty()
    };
    let omitted =
        |entry: &ProjectedEntry<'_>| intrinsically_visible(entry) && entry.messages.is_empty();
    let omitted_ids: Vec<&str> = suffix
        .iter()
        .filter(|entry| omitted(entry))
        .filter_map(|entry| entry.source.meta().map(|meta| meta.id.as_str()))
        .collect();
    let external_replacement = suffix.iter().any(|entry| match entry.source {
        FileEntry::ContextEdit(edit) => {
            edit.replacement.is_some() && !omitted_ids.contains(&edit.target_id.as_str())
        }
        _ => false,
    });
    let recovery_suffix = exceeded && !external_replacement && suffix.iter().any(|entry| {
        matches!(entry.source, FileEntry::Message(m) if matches!(m.message, Message::Assistant(_)))
            && omitted(entry)
    })
        && suffix.iter().all(|entry| {
            !is_compaction(entry) && (!intrinsically_visible(entry) || omitted(entry))
        });
    if recovery_suffix {
        cut += 1;
    }

    while cut > start {
        let previous = &entries[cut - 1];
        if is_compaction(previous) || !previous.messages.is_empty() {
            break;
        }
        cut -= 1;
    }
    let turn_start = if entries.get(cut).is_some_and(starts_turn) {
        None
    } else {
        (start..=cut.min(end.saturating_sub(1)))
            .rev()
            .find(|&index| starts_turn(&entries[index]))
    };
    CutPoint {
        first_kept: cut,
        turn_start,
        split_turn: turn_start.is_some(),
    }
}

/// Files read and changed by tool calls, for the summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOperations {
    /// Read paths.
    pub read: BTreeSet<String>,
    /// Written paths.
    pub written: BTreeSet<String>,
    /// Edited paths.
    pub edited: BTreeSet<String>,
}

impl FileOperations {
    fn add(&mut self, tool: &str, arguments: &serde_json::Map<String, Value>) {
        let Some(path) = arguments
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return;
        };
        let set = match tool {
            "read" => &mut self.read,
            "write" => &mut self.written,
            "edit" => &mut self.edited,
            _ => return,
        };
        set.insert(path.to_owned());
    }

    /// Records the file tools a message called.
    pub fn extract(&mut self, message: &Message) {
        match message {
            Message::ToolResult(result) => {
                for call in result.nested_calls.iter().flat_map(|nested| &nested.calls) {
                    if let Some(arguments) = &call.arguments {
                        self.add(&call.name, arguments);
                    }
                }
            }
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        self.add(&call.name, &call.arguments);
                    }
                }
            }
            _ => {}
        }
    }

    /// Paths only read, and paths changed, each sorted.
    pub fn lists(&self) -> (Vec<String>, Vec<String>) {
        let modified: BTreeSet<&String> = self.edited.iter().chain(&self.written).collect();
        let read = self
            .read
            .iter()
            .filter(|path| !modified.contains(path))
            .cloned()
            .collect();
        (read, modified.into_iter().cloned().collect())
    }
}

/// The file lists as tags appended to a summary.
pub fn format_file_operations(read: &[String], modified: &[String]) -> String {
    let mut sections = Vec::new();
    if !read.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read.join("\n")));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn truncate_for_summary(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_owned();
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        String::from_utf16_lossy(&units[..max]),
        units.len() - max
    )
}

/// The conversation as plain text, so the summarizer does not continue it.
/// Takes messages already converted for the model.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = match &user.content {
                    Content::Text(text) => text.clone(),
                    Content::Blocks(blocks) => yapi_types::message::blocks_text(blocks, ""),
                };
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking = Vec::new();
                let mut calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking(block) => thinking.push(block.thinking.as_str()),
                        ContentBlock::ToolCall(call) => {
                            let arguments: Vec<String> = call
                                .arguments
                                .iter()
                                .map(|(key, value)| {
                                    format!(
                                        "{key}={}",
                                        yapi_types::json::to_string(value).unwrap_or_default()
                                    )
                                })
                                .collect();
                            calls.push(format!("{}({})", call.name, arguments.join(", ")));
                        }
                        _ => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if assistant
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(_)))
                {
                    parts.push(format!(
                        "[Assistant]: {}",
                        yapi_types::message::blocks_text(&assistant.content, "\n")
                    ));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = yapi_types::message::blocks_text(&result.content, "");
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// System prompt of summary requests.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = "Update the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\nCreate a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\n## Original Request\n[What did the user ask for?]\n\n## Progress So Far\n- [Key decisions and work completed in these messages]\n\n## Context Needed to Continue\n- [Information from these messages needed to understand the later work]\n\nOnly summarize information explicitly present above. Do not infer or recreate later messages.";

/// What compaction will summarize and keep.
#[derive(Clone, Debug)]
pub struct Preparation {
    /// First entry kept verbatim.
    pub first_kept_entry_id: String,
    /// Messages summarized and dropped.
    pub messages_to_summarize: Vec<Message>,
    /// The start of a turn split by the cut, summarized separately.
    pub turn_prefix_messages: Vec<Message>,
    /// The cut falls inside a turn.
    pub is_split_turn: bool,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// The previous compaction's summary, to update.
    pub previous_summary: Option<String>,
    /// Files touched by the summarized messages.
    pub file_ops: FileOperations,
    /// The settings used.
    pub settings: CompactionSettings,
}

/// Plans a compaction of a branch; `None` when there is nothing to summarize.
pub fn prepare_compaction(
    branch: &[&FileEntry],
    settings: CompactionSettings,
) -> Option<Preparation> {
    if matches!(branch.last(), Some(FileEntry::Compaction(_))) {
        return None;
    }
    let projection = crate::session::build_projection(branch);
    let entries = &projection.entries;
    // The newest compaction is projected first; older ones contribute nothing.
    let previous = entries
        .iter()
        .position(|entry| is_compaction(entry) && !entry.messages.is_empty());
    let (previous_summary, start) = match previous {
        Some(index) => match entries[index].source {
            FileEntry::Compaction(compaction) => (Some(compaction.summary.clone()), index + 1),
            _ => (None, 0),
        },
        None => (None, 0),
    };
    let end = entries.len();
    let tokens_before = estimate_projected_context_tokens(&projection, branch).tokens;
    let cut = find_cut_point(entries, start, end, settings.keep_recent_tokens);
    let first_kept_entry_id = entries
        .get(cut.first_kept)?
        .source
        .meta()
        .map(|meta| meta.id.clone())
        .filter(|id| !id.is_empty())?;
    let history_end = if cut.split_turn {
        cut.turn_start.unwrap_or(cut.first_kept)
    } else {
        cut.first_kept
    };
    let summarizable = |range: std::ops::Range<usize>| -> Vec<Message> {
        entries[range]
            .iter()
            .filter(|entry| !is_compaction(entry))
            .flat_map(|entry| {
                entry
                    .messages
                    .iter()
                    .filter(|message| !matches!(message, Message::System(_)))
                    .cloned()
            })
            .collect()
    };
    let messages_to_summarize = summarizable(start..history_end.max(start));
    let turn_prefix_messages = match (cut.split_turn, cut.turn_start) {
        (true, Some(turn_start)) => summarizable(turn_start..cut.first_kept),
        _ => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }
    let mut file_ops = FileOperations::default();
    if let Some(index) = previous
        && let FileEntry::Compaction(compaction) = entries[index].source
        && compaction.from_hook != Some(true)
        && let Some(details) = &compaction.details
    {
        for path in details["readFiles"].as_array().into_iter().flatten() {
            if let Some(path) = path.as_str() {
                file_ops.read.insert(path.to_owned());
            }
        }
        for path in details["modifiedFiles"].as_array().into_iter().flatten() {
            if let Some(path) = path.as_str() {
                file_ops.edited.insert(path.to_owned());
            }
        }
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        file_ops.extract(message);
    }
    Some(Preparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut.split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

/// What summary requests need besides the messages.
#[derive(Clone)]
pub struct Summarizer<'a> {
    /// The model to summarize with.
    pub model: &'a Model,
    /// Wire APIs.
    pub apis: &'a Apis,
    /// Credentials for the model.
    pub auth: &'a Auth,
    /// The session's thinking level; used when the model reasons.
    pub thinking_level: ThinkingLevel,
    /// Retries for transient failures.
    pub retry: RetryPolicy,
    /// Cancels the requests.
    pub cancel: CancellationToken,
    /// Hears about retries.
    pub on_retry: Option<&'a (dyn Fn(SummaryRetry) + Sync)>,
}

/// What a summarizer reports while it retries; pi-ai's `RetryCallbacks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SummaryRetry {
    /// A failed request is retried after `delay_ms`.
    Scheduled {
        /// The retry's number, from 1.
        attempt: u32,
        /// How many retries the policy allows.
        max_attempts: u32,
        /// The wait before it.
        delay_ms: u64,
        /// Why the request failed.
        error_message: String,
    },
    /// The retried request starts.
    AttemptStart,
    /// Retrying ended, in success or not.
    Finished,
}

/// A summary and the usage of the requests that made it.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// The text.
    pub text: String,
    /// Usage of the summary requests.
    pub usage: Usage,
}

/// The error for a response that cannot become a checkpoint.
pub fn summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response
                .error_message
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or("Unknown error")
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

impl Summarizer<'_> {
    async fn complete_once(
        &self,
        prompt: &str,
        max_tokens: u64,
        session_id: &str,
    ) -> AssistantMessage {
        let messages = vec![
            Message::System(SystemMessage {
                content: Content::Text(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
                sections: None,
                timestamp: 0,
                tools_added: None,
                tools_removed: None,
            }),
            Message::User(UserMessage {
                content: Content::Blocks(vec![ContentBlock::text(prompt)]),
                timestamp: now_ms(),
            }),
        ];
        let reasoning = (self.model.reasoning && self.thinking_level != ThinkingLevel::Off)
            .then_some(self.thinking_level);
        let options = StreamOptions {
            reasoning,
            max_tokens: Some(max_tokens),
            session_id: Some(session_id.to_owned()),
            cache_retention: Some(CacheRetention::None),
            cancel: self.cancel.clone(),
            ..StreamOptions::default()
        };
        let mut request = Request {
            model: self.model.clone(),
            messages,
            options,
        };
        let stream = match self.auth.clone().apply(&mut request) {
            Ok(()) => self.apis.stream(request),
            Err(message) => {
                yapi_ai::api::failed_stream(&request.model, &request.options.cancel, message)
            }
        };
        match stream.result().await {
            Some(message) => message,
            None => {
                let mut message = yapi_ai::stream::new_output(self.model, now_ms());
                message.stop_reason = StopReason::Error;
                message.error_message = Some("Stream ended without a result".into());
                message
            }
        }
    }

    /// One summary request with retries on transient failures, reported as
    /// pi-ai's `retryAssistantCall` reports them.
    async fn complete(&self, prompt: &str, max_tokens: u64) -> AssistantMessage {
        let session_id = uuid_v7();
        let max_attempts = if self.retry.enabled {
            self.retry.max_retries
        } else {
            0
        };
        let notify = |retry: SummaryRetry| {
            if let Some(on_retry) = self.on_retry {
                on_retry(retry);
            }
        };
        let mut attempt = 0;
        loop {
            let response = self.complete_once(prompt, max_tokens, &session_id).await;
            if response.stop_reason != StopReason::Error
                || attempt >= max_attempts
                || !is_retryable_assistant_error(&response)
            {
                if attempt > 0 {
                    notify(SummaryRetry::Finished);
                }
                return response;
            }
            attempt += 1;
            let delay_ms = self.retry.delay_ms(attempt);
            notify(SummaryRetry::Scheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message: response
                    .error_message
                    .clone()
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "Unknown error".into()),
            });
            tokio::select! {
                () = self.cancel.cancelled() => {
                    notify(SummaryRetry::Finished);
                    let mut response = response;
                    response.stop_reason = StopReason::Aborted;
                    response.error_message = None;
                    return response;
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
            }
            notify(SummaryRetry::AttemptStart);
        }
    }

    async fn summarize(
        &self,
        prompt: &str,
        max_tokens: u64,
        label: &str,
    ) -> Result<Summary, String> {
        // As in pi, a request started after the abort fails with its kind,
        // while one aborted mid-stream counts as finished; the caller then
        // sees the abort.
        if self.cancel.is_cancelled() {
            return Err(format!("{label} failed: This operation was aborted"));
        }
        let response = self.complete(prompt, max_tokens).await;
        if response.stop_reason == StopReason::Aborted {
            return Ok(Summary {
                text: yapi_types::message::blocks_text(&response.content, "\n"),
                usage: response.usage,
            });
        }
        if let Some(failure) = summarization_failure(&response, label) {
            return Err(failure);
        }
        if response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            return Err(format!("{label} attempted to call a tool"));
        }
        Ok(Summary {
            text: yapi_types::message::blocks_text(&response.content, "\n"),
            usage: response.usage,
        })
    }

    fn max_tokens(&self, fraction: f64, reserve_tokens: u64) -> u64 {
        let budget = (fraction * reserve_tokens as f64).floor() as u64;
        if self.model.max_tokens > 0 {
            budget.min(self.model.max_tokens)
        } else {
            budget
        }
    }

    /// Summarizes `messages`, updating `previous` when given.
    pub async fn generate_summary(
        &self,
        messages: &[Message],
        reserve_tokens: u64,
        custom_instructions: Option<&str>,
        previous: Option<&str>,
    ) -> Result<Summary, String> {
        let mut base = if previous.is_some() {
            format!(
                "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n{UPDATE_SUMMARIZATION_INSTRUCTIONS}"
            )
        } else {
            SUMMARIZATION_PROMPT.to_owned()
        };
        if let Some(instructions) = custom_instructions.filter(|text| !text.is_empty()) {
            base = format!("{base}\n\nAdditional focus: {instructions}");
        }
        let conversation = serialize_conversation(&convert_to_llm(messages.to_vec()));
        let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
        if let Some(previous) = previous {
            prompt += &format!("<previous-summary>\n{previous}\n</previous-summary>\n\n");
        }
        prompt += &base;
        self.summarize(
            &prompt,
            self.max_tokens(0.8, reserve_tokens),
            "Summarization",
        )
        .await
    }

    async fn turn_prefix_summary(
        &self,
        messages: &[Message],
        reserve_tokens: u64,
    ) -> Result<Summary, String> {
        let conversation = serialize_conversation(&convert_to_llm(messages.to_vec()));
        let prompt = format!(
            "# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
        );
        self.summarize(
            &prompt,
            self.max_tokens(0.5, reserve_tokens),
            "Turn prefix summarization",
        )
        .await
    }

    /// Generates the compaction summary for a preparation.
    pub async fn compact(
        &self,
        preparation: &Preparation,
        custom_instructions: Option<&str>,
    ) -> Result<yapi_types::event::CompactionResult, String> {
        let reserve = preparation.settings.reserve_tokens;
        let previous = preparation.previous_summary.as_deref();
        let (mut summary, usage) =
            if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
                let history = if preparation.messages_to_summarize.is_empty() {
                    None
                } else {
                    Some(
                        self.generate_summary(
                            &preparation.messages_to_summarize,
                            reserve,
                            custom_instructions,
                            previous,
                        )
                        .await?,
                    )
                };
                let prefix = self
                    .turn_prefix_summary(&preparation.turn_prefix_messages, reserve)
                    .await?;
                let history_text = match &history {
                    Some(history) => history.text.clone(),
                    None => previous.unwrap_or("No prior history.").to_owned(),
                };
                let usage = match history {
                    Some(history) => combine_usage(&history.usage, &prefix.usage),
                    None => prefix.usage,
                };
                (
                    format!(
                        "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                        prefix.text
                    ),
                    usage,
                )
            } else {
                let result = self
                    .generate_summary(
                        &preparation.messages_to_summarize,
                        reserve,
                        custom_instructions,
                        previous,
                    )
                    .await?;
                (result.text, result.usage)
            };
        let (read, modified) = preparation.file_ops.lists();
        summary += &format_file_operations(&read, &modified);
        Ok(yapi_types::event::CompactionResult {
            summary,
            first_kept_entry_id: preparation.first_kept_entry_id.clone(),
            tokens_before: preparation.tokens_before,
            estimated_tokens_after: None,
            usage: Some(usage),
            details: Some(json!({"readFiles": read, "modifiedFiles": modified})),
        })
    }
}

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// The message an entry contributes to a branch summary; tool results are
/// skipped because the calls carry their context.
fn branch_entry_message(entry: &FileEntry) -> Option<Message> {
    match entry {
        FileEntry::Message(entry) if matches!(entry.message, Message::ToolResult(_)) => None,
        FileEntry::Message(_) | FileEntry::CustomMessage(_) | FileEntry::BranchSummary(_) => {
            entry_messages(entry).into_iter().next()
        }
        FileEntry::Compaction(compaction) => Some(Message::CompactionSummary(
            yapi_types::message::CompactionSummaryMessage {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
                timestamp: crate::time::parse_iso(&compaction.meta.timestamp).unwrap_or_default(),
            },
        )),
        _ => None,
    }
}

/// Messages of the abandoned branch, newest first until `token_budget` (0 for
/// none), and the files touched across all of it.
pub fn prepare_branch_entries(
    entries: &[FileEntry],
    token_budget: u64,
) -> (Vec<Message>, FileOperations) {
    let mut file_ops = FileOperations::default();
    for entry in entries {
        if let FileEntry::BranchSummary(summary) = entry
            && summary.from_hook != Some(true)
            && let Some(details) = &summary.details
        {
            for path in details["readFiles"].as_array().into_iter().flatten() {
                if let Some(path) = path.as_str() {
                    file_ops.read.insert(path.to_owned());
                }
            }
            for path in details["modifiedFiles"].as_array().into_iter().flatten() {
                if let Some(path) = path.as_str() {
                    file_ops.edited.insert(path.to_owned());
                }
            }
        }
    }
    let mut messages: Vec<Message> = Vec::new();
    let mut total = 0;
    for entry in entries.iter().rev() {
        let Some(message) = branch_entry_message(entry) else {
            continue;
        };
        file_ops.extract(&message);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total + tokens > token_budget {
            // Summaries are worth keeping when there is still some room.
            if matches!(
                entry,
                FileEntry::Compaction(_) | FileEntry::BranchSummary(_)
            ) && (total as f64) < token_budget as f64 * 0.9
            {
                messages.insert(0, message);
            }
            break;
        }
        messages.insert(0, message);
        total += tokens;
    }
    (messages, file_ops)
}

/// What summarizing a branch produced.
#[derive(Clone, Debug, PartialEq)]
pub enum BranchSummary {
    /// A summary to record, with its usage and file lists.
    Done {
        /// The text.
        summary: String,
        /// Usage of the request, if one was made.
        usage: Option<Usage>,
        /// Files only read.
        read_files: Vec<String>,
        /// Files changed.
        modified_files: Vec<String>,
    },
    /// The request was cancelled.
    Aborted,
}

impl Summarizer<'_> {
    /// Summarizes abandoned branch entries (chronological). `replace` makes
    /// `custom_instructions` the whole instruction instead of an addition.
    pub async fn branch_summary(
        &self,
        entries: &[FileEntry],
        custom_instructions: Option<&str>,
        replace: bool,
        reserve_tokens: u64,
    ) -> Result<BranchSummary, String> {
        let window = if self.model.context_window > 0 {
            self.model.context_window
        } else {
            128_000
        };
        let (messages, file_ops) =
            prepare_branch_entries(entries, window.saturating_sub(reserve_tokens));
        if messages.is_empty() {
            return Ok(BranchSummary::Done {
                summary: "No content to summarize".into(),
                usage: None,
                read_files: Vec::new(),
                modified_files: Vec::new(),
            });
        }
        let conversation = serialize_conversation(&convert_to_llm(messages));
        let instructions = match custom_instructions.filter(|text| !text.is_empty()) {
            Some(custom) if replace => custom.to_owned(),
            Some(custom) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
            None => BRANCH_SUMMARY_PROMPT.to_owned(),
        };
        let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}");
        let max_tokens = if self.model.max_tokens > 0 {
            self.model.max_tokens.min(4096)
        } else {
            4096
        };
        let response = self.complete(&prompt, max_tokens).await;
        if response.stop_reason == StopReason::Aborted {
            return Ok(BranchSummary::Aborted);
        }
        if let Some(failure) = summarization_failure(&response, "Branch summarization") {
            return Err(failure);
        }
        if response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            return Err("Branch summarization attempted to call a tool".into());
        }
        let (read_files, modified_files) = file_ops.lists();
        let summary = format!(
            "{BRANCH_SUMMARY_PREAMBLE}{}{}",
            yapi_types::message::blocks_text(&response.content, "\n"),
            format_file_operations(&read_files, &modified_files)
        );
        Ok(BranchSummary::Done {
            summary,
            usage: Some(response.usage),
            read_files,
            modified_files,
        })
    }
}

/// Entries from `old_leaf` back to the deepest common ancestor with `target`,
/// oldest first, and that ancestor.
pub fn collect_branch_entries(
    session: &crate::session::SessionManager,
    old_leaf: Option<&str>,
    target: &str,
) -> (Vec<FileEntry>, Option<String>) {
    let Some(old_leaf) = old_leaf else {
        return (Vec::new(), None);
    };
    let ids = |path: Vec<&FileEntry>| -> Vec<String> {
        path.iter()
            .filter_map(|entry| entry.meta().map(|meta| meta.id.clone()))
            .collect()
    };
    let old_path = ids(session.branch_path(Some(old_leaf)));
    let target_path = ids(session.branch_path(Some(target)));
    let common = target_path
        .iter()
        .rev()
        .find(|id| old_path.contains(id))
        .cloned();
    let mut entries = Vec::new();
    let mut current = Some(old_leaf.to_owned());
    while let Some(id) = current.filter(|id| Some(id) != common.as_ref()) {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        current = entry.meta().and_then(|meta| meta.parent_id.clone());
        entries.push(entry.clone());
    }
    entries.reverse();
    (entries, common)
}

/// Sum of two usages, as pi's `combineUsage`.
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let optional = |a: Option<u64>, b: Option<u64>| {
        (a.is_some() || b.is_some()).then(|| a.unwrap_or(0) + b.unwrap_or(0))
    };
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: optional(first.cache_write_1h, second.cache_write_1h),
        reasoning: optional(first.reasoning, second.reasoning),
        total_tokens: Some(first.total_tokens.unwrap_or(0) + second.total_tokens.unwrap_or(0)),
        cost: yapi_types::message::Cost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_conversations() {
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"role":"user","content":"hi","timestamp":1},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"plan"},
                {"type":"text","text":"Reading."},
                {"type":"toolCall","id":"c","name":"read","arguments":{"path":"a.txt","limit":null}}
            ],"api":"x","provider":"p","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":2},
            {"role":"toolResult","toolCallId":"c","toolName":"read","content":[{"type":"text","text":"x"}],"isError":false,"timestamp":3}
        ]))
        .unwrap();
        assert_eq!(
            serialize_conversation(&messages),
            "[User]: hi\n\n[Assistant thinking]: plan\n\n[Assistant]: Reading.\n\n[Assistant tool calls]: read(path=\"a.txt\", limit=null)\n\n[Tool result]: x"
        );
        let mut ops = FileOperations::default();
        for message in &messages {
            ops.extract(message);
        }
        assert_eq!(ops.lists(), (vec!["a.txt".to_owned()], vec![]));
    }

    #[test]
    fn thresholds() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 10,
        };
        assert!(should_compact(950, 1000, &settings));
        assert!(!should_compact(900, 1000, &settings));
        assert!(should_compact(1, 50, &settings));
    }
}
