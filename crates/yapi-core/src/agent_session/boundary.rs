//! pi's boundary events: `turn_end`, where extensions may stage session
//! entries and ask for one more response, and `agent_before_settle`, the
//! same before a run settles.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::hooks::TurnDecision;
use yapi_types::event::AgentEvent;
use yapi_types::message::{AssistantMessage, Content, Message, StopReason, ToolResultMessage};
use yapi_types::session::FileEntry;
use yapi_types::settings::QueueMode;
use yapi_types::sync::lock;

use crate::compaction::estimate_projected_context_tokens;
use crate::session::{SessionManager, build_projection};

use super::AgentSession;

/// pi's boundary events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// `turn_end`.
    TurnEnd,
    /// `agent_before_settle`.
    BeforeSettle,
}

impl Boundary {
    /// The event's type.
    pub fn kind(self) -> &'static str {
        match self {
            Boundary::TurnEnd => "turn_end",
            Boundary::BeforeSettle => "agent_before_settle",
        }
    }

    /// The boundary an event type names.
    pub fn parse(kind: &str) -> Option<Boundary> {
        match kind {
            "turn_end" => Some(Boundary::TurnEnd),
            "agent_before_settle" => Some(Boundary::BeforeSettle),
            _ => None,
        }
    }
}

const INVALID_REPLACEMENT: &str =
    "Context edit replacement must be null or contain string/array content";

/// pi's `_applyBoundaryDrafts`: appends the entries `drafts` describe to
/// `manager`, and returns them.
fn apply_drafts(manager: &mut SessionManager, drafts: &[Value]) -> Result<Vec<FileEntry>, String> {
    let mut appended = Vec::new();
    for draft in drafts {
        let text = |key: &str| draft[key].as_str().unwrap_or_default().to_owned();
        let optional = |key: &str| Some(draft[key].clone()).filter(|value| !value.is_null());
        let id = match draft["type"].as_str() {
            Some("custom") => manager.append_custom_entry(&text("customType"), optional("data")),
            Some("custom_message") => {
                let content = serde_json::from_value::<Content>(draft["content"].clone())
                    .map_err(|err| err.to_string())?;
                manager.append_custom_message(
                    &text("customType"),
                    content,
                    draft["display"] == true,
                    optional("details"),
                )
            }
            Some("context_edit") => {
                let replacement = match &draft["replacement"] {
                    Value::Null => None,
                    Value::Object(replacement) => Some(
                        serde_json::from_value::<Content>(
                            replacement.get("content").cloned().unwrap_or_default(),
                        )
                        .map_err(|_| INVALID_REPLACEMENT)?,
                    ),
                    _ => return Err(INVALID_REPLACEMENT.into()),
                };
                manager.append_context_edit(&text("targetId"), replacement)
            }
            Some("compaction") => {
                let branch = manager.branch_path(None);
                let tokens_before =
                    estimate_projected_context_tokens(&build_projection(&branch), &branch).tokens;
                manager.append_compaction(
                    text("summary"),
                    draft["firstKeptEntryId"].as_str().map(str::to_owned),
                    tokens_before,
                    optional("details"),
                    Some(true),
                    serde_json::from_value(draft["usage"].clone()).ok(),
                )
            }
            // pi appends nothing for other drafts.
            _ => continue,
        }
        .map_err(|err| err.to_string())?;
        appended.extend(manager.entry(&id).cloned());
    }
    Ok(appended)
}

impl AgentSession {
    /// The session as it would be with `drafts` appended.
    fn preview_with(&self, drafts: &[Value]) -> Result<SessionManager, String> {
        let mut preview = self.with_session(|session| session.preview());
        apply_drafts(&mut preview, drafts)?;
        Ok(preview)
    }

    /// Queued messages the next turn would take, then extension messages
    /// waiting for the turn to end; pi's `_getPendingBoundaryMessages`.
    fn pending_boundary_messages(&self) -> Vec<Message> {
        let settings = self.settings();
        let peek = |queue: &std::sync::Mutex<std::collections::VecDeque<Message>>, mode| {
            let queue = lock(queue);
            let count = if mode == Some(QueueMode::All) {
                queue.len()
            } else {
                1
            };
            queue.iter().take(count).cloned().collect::<Vec<_>>()
        };
        let mut pending = peek(&self.inner.steering, settings.steering_mode);
        if pending.is_empty() {
            pending = peek(&self.inner.follow_up, settings.follow_up_mode);
        }
        pending.extend(lock(&self.inner.pending_custom).iter().cloned());
        pending
    }

    /// Whether `llm`, the model context of a session, can be continued at
    /// `boundary`, counting queued and waiting messages.
    fn can_continue(&self, boundary: Boundary, llm: &[Message]) -> bool {
        let final_assistant = matches!(llm.last(), Some(Message::Assistant(_)));
        let has_context = llm
            .iter()
            .any(|message| !matches!(message, Message::System(_)));
        (has_context && !final_assistant)
            || !lock(&self.inner.pending_custom).is_empty()
            || match boundary {
                Boundary::TurnEnd => self.has_queued(),
                Boundary::BeforeSettle => final_assistant && self.has_queued(),
            }
    }

    /// pi's `BoundaryContextPreview` at `boundary` with `drafts` staged:
    /// the model context entry by entry, as messages and as the provider
    /// sees them, the pending messages, and whether the run can continue.
    pub fn boundary_context(&self, boundary: Boundary, drafts: &[Value]) -> Result<Value, String> {
        let preview = self.preview_with(drafts)?;
        let branch = preview.branch_path(None);
        let projection = build_projection(&branch);
        let llm = crate::messages::convert_to_llm(projection.messages.clone());
        let entries: Vec<Value> = projection
            .entries
            .iter()
            .map(|entry| json!({"sourceEntry": entry.source, "messages": entry.messages}))
            .collect();
        Ok(json!({
            "contextEntries": entries,
            "contextMessages": projection.messages,
            "llmMessages": llm,
            "pendingMessages": self.pending_boundary_messages(),
            "canContinue": self.can_continue(boundary, &llm),
        }))
    }

    /// Whether the session as it is can be continued at `boundary`.
    fn can_continue_now(&self, boundary: Boundary) -> bool {
        let llm = crate::messages::convert_to_llm(self.messages());
        self.can_continue(boundary, &llm)
    }

    /// pi's `emitBoundary`: each extension sees the entries and the
    /// continuation earlier ones staged and may replace them. Entries that
    /// cannot be appended are reported, and when the last extension leaves
    /// them so, nothing is staged and the run does not continue.
    async fn emit_boundary(
        &self,
        base: Value,
        boundary: Boundary,
        cancel: CancellationToken,
    ) -> (Vec<Value>, bool) {
        let ctx = self.extension_context(cancel);
        let mut entries: Vec<Value> = Vec::new();
        let mut proceed = false;
        let mut valid = true;
        for extension in self.extensions_handling(boundary.kind()) {
            let mut event = base.clone();
            event["entries"] = Value::Array(entries.clone());
            event["continue"] = proceed.into();
            if let Some(result) = extension.handle(&ctx, &event).await {
                if let Some(next) = result["entries"].as_array() {
                    entries.clone_from(next);
                }
                if let Some(next) = result["continue"].as_bool() {
                    proceed = next;
                }
            }
            valid = match self.preview_with(&entries) {
                Ok(_) => true,
                Err(error) => {
                    ctx.ui.extension_error(
                        &extension.source().path,
                        boundary.kind(),
                        &format!("Invalid boundary entries: {error}"),
                        None,
                    );
                    false
                }
            };
        }
        if valid {
            (entries, proceed)
        } else {
            (Vec::new(), false)
        }
    }

    /// pi's `_commitBoundaryDrafts`: appends the staged entries and reports
    /// each; whether any were appended.
    fn commit_drafts(&self, drafts: &[Value]) -> bool {
        if drafts.is_empty() {
            return false;
        }
        let appended = self
            .with_session(|session| apply_drafts(session, drafts))
            .unwrap_or_default();
        for entry in &appended {
            self.emit(&AgentEvent::EntryAppended {
                entry: entry.clone(),
            });
        }
        !appended.is_empty()
    }

    fn invalid_continuation(&self, boundary: Boundary) {
        let (ui, _) = self.extension_binding();
        let kind = boundary.kind();
        ui.extension_error(
            "<boundary>",
            kind,
            &format!("{kind} requested continuation without runnable model context"),
            None,
        );
    }

    /// pi's `turn_end` boundary for the turn that produced `message` and
    /// `tool_results`: whether to continue, and the transcript when
    /// extensions appended entries.
    pub(super) async fn turn_end_boundary(
        &self,
        message: &AssistantMessage,
        tool_results: &[ToolResultMessage],
        turn_index: u64,
        cancel: CancellationToken,
    ) -> TurnDecision {
        let outcome = match message.stop_reason {
            StopReason::Aborted => "aborted",
            StopReason::Error => "error",
            _ => "completed",
        };
        *lock(&self.inner.outcome) = outcome;
        if !self.has_handlers("turn_end") {
            return TurnDecision::default();
        }
        let (entry_id, tool_result_ids) = {
            let recovery = lock(&self.inner.recovery);
            (
                recovery
                    .last_assistant
                    .as_ref()
                    .and_then(|(_, id)| id.clone()),
                recovery.turn_tool_results.clone(),
            )
        };
        let Some(entry_id) = entry_id else {
            let (ui, _) = self.extension_binding();
            ui.extension_error(
                "<boundary>",
                "turn_end",
                "turn_end could not resolve the persisted assistant entry ID",
                None,
            );
            return TurnDecision::default();
        };
        let tool_results: Vec<Message> = tool_results
            .iter()
            .cloned()
            .map(Message::ToolResult)
            .collect();
        let base = json!({
            "type": "turn_end",
            "turnIndex": turn_index,
            "message": Message::Assistant(Box::new(message.clone())),
            "toolResults": tool_results,
            "messageEntryId": entry_id,
            "toolResultEntryIds": tool_result_ids,
            "outcome": outcome,
        });
        let (entries, proceed) = self.emit_boundary(base, Boundary::TurnEnd, cancel).await;
        let messages = self.commit_drafts(&entries).then(|| self.messages());
        let continue_run = proceed && self.can_continue_now(Boundary::TurnEnd);
        if proceed && !continue_run {
            self.invalid_continuation(Boundary::TurnEnd);
        }
        TurnDecision {
            continue_run,
            messages,
        }
    }

    /// pi's `agent_before_settle` boundary once a run would settle: whether
    /// to run again.
    pub(super) async fn before_settle(&self, cancel: &CancellationToken) -> bool {
        if !self.has_handlers("agent_before_settle") {
            return self.has_queued();
        }
        let outcome = *lock(&self.inner.outcome);
        let base = json!({"type": "agent_before_settle", "outcome": outcome});
        let (entries, proceed) = self
            .emit_boundary(base, Boundary::BeforeSettle, cancel.clone())
            .await;
        self.commit_drafts(&entries);
        self.flush_pending_custom();
        if cancel.is_cancelled() {
            return false;
        }
        let should_continue = proceed || self.has_queued();
        if should_continue && !self.can_continue_now(Boundary::BeforeSettle) {
            if proceed {
                self.invalid_continuation(Boundary::BeforeSettle);
            }
            return false;
        }
        should_continue
    }
}
