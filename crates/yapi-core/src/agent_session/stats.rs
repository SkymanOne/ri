//! Context use, token and cost totals, and session statistics.

use yapi_types::message::{ContentBlock, Message, StopReason};
use yapi_types::session::FileEntry;

use crate::compaction::{calculate_context_tokens, estimate_projected_context_tokens};
use crate::session::build_projection;

use super::AgentSession;

/// How full the context window is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    /// Estimated context tokens; unknown right after a compaction.
    pub tokens: Option<u64>,
    /// The model's context window.
    pub context_window: u64,
}

impl ContextUsage {
    /// Percent of the window in use, when known.
    pub fn percent(&self) -> Option<f64> {
        self.tokens
            .map(|tokens| tokens as f64 / self.context_window as f64 * 100.0)
    }
}

/// Session-wide token and cost totals.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotals {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache read tokens.
    pub cache_read: u64,
    /// Cache write tokens.
    pub cache_write: u64,
    /// Cost in US dollars.
    pub cost: f64,
    /// Cache hit rate of the latest assistant message, in percent.
    pub cache_hit_rate: Option<f64>,
}

/// Message counts and per-model cost, as `/session` shows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    /// Message entries.
    pub total_messages: usize,
    /// User messages.
    pub user_messages: usize,
    /// Assistant messages.
    pub assistant_messages: usize,
    /// Tool calls in assistant messages.
    pub tool_calls: usize,
    /// Tool results.
    pub tool_results: usize,
    /// Cost and tokens by `provider/model`, costliest first; summaries and
    /// tool usage are grouped as `Tools/summaries`.
    pub breakdown: Vec<(String, f64, u64)>,
}

impl AgentSession {
    /// Context use of the current branch: `tokens` is `None` after a compaction
    /// until the model responds again. `None` without a model or window.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.model()?;
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }
        self.with_session(|session| {
            let branch = session.branch_path(None);
            let projection = build_projection(&branch);
            let latest_compaction = branch
                .iter()
                .rposition(|entry| matches!(entry, FileEntry::Compaction(_)));
            if let Some(compaction) = latest_compaction {
                let has_usage = |entry: &&FileEntry| {
                    let Some(id) = entry.meta().map(|meta| meta.id.as_str()) else {
                        return false;
                    };
                    projection.entries.iter().any(|projected| {
                        projected.source.meta().is_some_and(|meta| meta.id == id)
                            && projected.messages.iter().any(|message| {
                                matches!(message, Message::Assistant(assistant)
                                    if !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error)
                                        && calculate_context_tokens(&assistant.usage) > 0)
                            })
                    })
                };
                if !branch[compaction + 1..].iter().any(has_usage) {
                    return Some(ContextUsage {
                        tokens: None,
                        context_window,
                    });
                }
            }
            let tokens = estimate_projected_context_tokens(&projection, &branch).tokens;
            Some(ContextUsage {
                tokens: Some(tokens),
                context_window,
            })
        })
    }

    /// Token and cost totals over every entry of the session file, and the
    /// cache hit rate of the latest assistant message.
    pub fn usage_totals(&self) -> UsageTotals {
        self.with_session(|session| {
            let mut totals = UsageTotals::default();
            for entry in session.entries() {
                let usage = match entry {
                    FileEntry::Usage(entry) => Some(&entry.usage),
                    FileEntry::Message(entry) => match &entry.message {
                        Message::Assistant(assistant) => {
                            let usage = &assistant.usage;
                            let prompt = usage.input + usage.cache_read + usage.cache_write;
                            totals.cache_hit_rate = (prompt > 0)
                                .then(|| usage.cache_read as f64 / prompt as f64 * 100.0);
                            Some(usage)
                        }
                        Message::ToolResult(result) => result.usage.as_ref(),
                        _ => None,
                    },
                    FileEntry::Compaction(entry) => entry.usage.as_ref(),
                    FileEntry::BranchSummary(entry) => entry.usage.as_ref(),
                    _ => None,
                };
                if let Some(usage) = usage {
                    totals.input += usage.input;
                    totals.output += usage.output;
                    totals.cache_read += usage.cache_read;
                    totals.cache_write += usage.cache_write;
                    totals.cost += usage.cost.total;
                }
            }
            totals
        })
    }

    /// pi's `getSessionStats` counts and `getUsageCostBreakdown`, over every
    /// entry of the session file.
    pub fn session_stats(&self) -> SessionStats {
        self.with_session(|session| {
            let mut stats = SessionStats::default();
            let mut breakdown: Vec<(String, f64, u64)> = Vec::new();
            let mut add = |key: String, usage: &yapi_types::message::Usage| {
                let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
                match breakdown
                    .iter_mut()
                    .find(|(existing, _, _)| *existing == key)
                {
                    Some((_, cost, total)) => {
                        *cost += usage.cost.total;
                        *total += tokens;
                    }
                    None => breakdown.push((key, usage.cost.total, tokens)),
                }
            };
            for entry in session.entries() {
                match entry {
                    FileEntry::Usage(entry) => {
                        add(format!("{}/{}", entry.provider, entry.model), &entry.usage);
                    }
                    FileEntry::Compaction(entry) => {
                        if let Some(usage) = &entry.usage {
                            add("Tools/summaries".into(), usage);
                        }
                    }
                    FileEntry::BranchSummary(entry) => {
                        if let Some(usage) = &entry.usage {
                            add("Tools/summaries".into(), usage);
                        }
                    }
                    FileEntry::Message(entry) => {
                        stats.total_messages += 1;
                        match &entry.message {
                            Message::User(_) => stats.user_messages += 1,
                            Message::ToolResult(result) => {
                                stats.tool_results += 1;
                                if let Some(usage) = &result.usage {
                                    add("Tools/summaries".into(), usage);
                                }
                            }
                            Message::Assistant(assistant) => {
                                stats.assistant_messages += 1;
                                stats.tool_calls += assistant
                                    .content
                                    .iter()
                                    .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                                    .count();
                                let model = assistant
                                    .response_model
                                    .as_deref()
                                    .unwrap_or(&assistant.model);
                                add(format!("{}/{model}", assistant.provider), &assistant.usage);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            breakdown.retain(|(_, cost, tokens)| *cost > 0.0 || *tokens > 0);
            breakdown.sort_by(|a, b| b.1.total_cmp(&a.1));
            stats.breakdown = breakdown;
            stats
        })
    }
}
