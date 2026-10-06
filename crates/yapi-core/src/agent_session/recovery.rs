//! What happens after a run: automatic retries, overflow recovery and
//! compaction, and manual compaction.

use std::sync::atomic::Ordering;

use tokio_util::sync::CancellationToken;
use yapi_ai::errors::{is_context_overflow, is_recoverable_length, is_retryable_assistant_error};
use yapi_types::event::AgentEvent;
use yapi_types::event::{CompactionReason, CompactionResult, SummarySource};
use yapi_types::message::{AssistantMessage, Message, StopReason, ThinkingLevel};
use yapi_types::model::Model;
use yapi_types::session::FileEntry;
use yapi_types::sync::lock;

use crate::compaction::{
    CompactionSettings, Preparation, RetryPolicy, Summarizer, SummaryRetry,
    calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
    estimate_tokens, prepare_compaction, should_compact,
};
use crate::session::build_projection;
use crate::time::parse_iso;

use super::{AgentSession, Compacting};

/// What the compaction check decided.
enum CompactionCheck {
    None,
    Overflow { will_retry: bool },
    OverflowFailed(String),
    Threshold,
}

impl AgentSession {
    /// Cancels the wait before an automatic retry; the failed response stands.
    pub fn abort_retry(&self) {
        let cancel = lock(&self.inner.retry_cancel).clone();
        if let Some(cancel) = cancel {
            // The retry's end is reported before this returns, as pi reports
            // it before answering `abort_retry`; the waiting retry then finds
            // nothing left to report.
            self.finish_cancelled_retry();
            cancel.cancel();
        }
    }

    /// The model whose limits apply to a response: the current one when it made it.
    fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        self.model()
            .filter(|model| model.is(&message.provider, &message.model))
    }

    fn is_retryable(&self, message: &AssistantMessage) -> bool {
        let window = self
            .model_for_message(message)
            .or_else(|| self.model())
            .map_or(0, |model| model.context_window);
        !is_context_overflow(message, window) && is_retryable_assistant_error(message)
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::resolve(lock(&self.inner.settings).settings())
    }

    /// Whether the run that just ended will be retried, for `agent_end`.
    pub(super) fn will_retry_after(&self, messages: &[Message]) -> bool {
        let cancelled = lock(&self.inner.cancel)
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);
        let policy = self.retry_policy();
        if cancelled
            || !policy.enabled
            || lock(&self.inner.recovery).retry_attempt >= policy.max_retries
        {
            return false;
        }
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(self.is_retryable(assistant)),
                _ => None,
            })
            .unwrap_or(false)
    }

    pub(super) fn finish_cancelled_retry(&self) {
        let attempt = std::mem::take(&mut lock(&self.inner.recovery).retry_attempt);
        if attempt > 0 {
            self.emit(&AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".into()),
            });
        }
    }

    /// Drops a failed or truncated attempt from the model's context while keeping
    /// it in the session history.
    fn omit_recovery_attempt(&self, entry_ids: &[String]) {
        for id in entry_ids {
            let entry = self.with_session(|session| {
                let edit = session.append_context_edit(id, None).ok()?;
                session.entry(&edit).cloned()
            });
            if let Some(entry) = entry {
                self.emit(&AgentEvent::EntryAppended { entry });
            }
        }
    }

    async fn prepare_retry(
        &self,
        message: &AssistantMessage,
        entry_id: Option<String>,
        cancel: &CancellationToken,
    ) -> bool {
        let policy = self.retry_policy();
        if !policy.enabled {
            return false;
        }
        let attempt = {
            let mut recovery = lock(&self.inner.recovery);
            if recovery.retry_attempt >= policy.max_retries {
                return false;
            }
            recovery.retry_attempt += 1;
            recovery.retry_attempt
        };
        let delay_ms = policy.delay_ms(attempt);
        self.emit(&AgentEvent::AutoRetryStart {
            attempt,
            max_attempts: policy.max_retries,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "Unknown error".into()),
        });
        self.omit_recovery_attempt(&entry_id.into_iter().collect::<Vec<_>>());
        let retry = cancel.child_token();
        *lock(&self.inner.retry_cancel) = Some(retry.clone());
        let waited = tokio::select! {
            () = retry.cancelled() => {
                self.finish_cancelled_retry();
                false
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
        };
        *lock(&self.inner.retry_cancel) = None;
        waited
    }

    pub(super) async fn handle_post_run(&self, cancel: &CancellationToken) -> bool {
        let (last, tool_results) = {
            let mut recovery = lock(&self.inner.recovery);
            (
                recovery.last_assistant.take(),
                std::mem::take(&mut recovery.last_tool_results),
            )
        };
        if cancel.is_cancelled() {
            self.finish_cancelled_retry();
            return false;
        }
        let Some((message, entry_id)) = last else {
            return self.has_queued();
        };
        if self.is_retryable(&message)
            && self.prepare_retry(&message, entry_id.clone(), cancel).await
        {
            return !cancel.is_cancelled();
        }
        if cancel.is_cancelled() {
            self.finish_cancelled_retry();
            return false;
        }
        if message.stop_reason == StopReason::Error {
            let attempt = std::mem::take(&mut lock(&self.inner.recovery).retry_attempt);
            if attempt > 0 {
                self.emit(&AgentEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: message.error_message.clone(),
                });
            }
        }
        if self
            .check_compaction(&message, entry_id, tool_results, cancel)
            .await
        {
            return !cancel.is_cancelled();
        }
        !cancel.is_cancelled() && self.has_queued()
    }

    /// pi's `_checkCompaction`: overflow recovery, then threshold compaction.
    async fn check_compaction(
        &self,
        message: &AssistantMessage,
        entry_id: Option<String>,
        tool_results: Vec<String>,
        cancel: &CancellationToken,
    ) -> bool {
        let Some(model) = self.model() else {
            return false;
        };
        let settings =
            CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
        if !settings.enabled || message.stop_reason == StopReason::Aborted {
            return false;
        }
        let message_model = self.model_for_message(message);
        let context_window = message_model.as_ref().unwrap_or(&model).context_window;
        let overflow_attempted = lock(&self.inner.recovery).overflow_recovery_attempted;
        let check = self.with_session(|session| {
            let branch = session.branch_path(None);
            let latest_compaction = branch.iter().rev().find_map(|entry| match entry {
                FileEntry::Compaction(compaction) => Some(compaction),
                _ => None,
            });
            let compaction_time =
                latest_compaction.and_then(|compaction| parse_iso(&compaction.meta.timestamp));
            if compaction_time.is_some_and(|time| message.timestamp <= time) {
                return CompactionCheck::None;
            }
            let projection = build_projection(&branch);
            let entry_id = entry_id.as_deref();
            let projected = entry_id.is_none_or(|id| {
                projection.entries.iter().any(|entry| {
                    entry.source.meta().is_some_and(|meta| meta.id == id)
                        && entry
                            .messages
                            .iter()
                            .any(|message| matches!(message, Message::Assistant(_)))
                })
            });
            let after: Vec<&FileEntry> = entry_id
                .and_then(|id| {
                    branch
                        .iter()
                        .position(|entry| entry.meta().is_some_and(|meta| meta.id == id))
                })
                .map(|index| branch[index + 1..].to_vec())
                .unwrap_or_default();
            let post_edit = after
                .iter()
                .any(|entry| matches!(entry, FileEntry::ContextEdit(_)));
            let latest_edit = after.iter().rev().find_map(|entry| match entry {
                FileEntry::ContextEdit(edit) if Some(edit.target_id.as_str()) == entry_id => {
                    Some(edit)
                }
                _ => None,
            });
            let retained = entry_id.is_none()
                || (!after
                    .iter()
                    .any(|entry| matches!(entry, FileEntry::Compaction(_)))
                    && latest_edit.is_none_or(|edit| edit.replacement.is_some()));
            let usage_matches = projected && !post_edit;
            let explicit = message.stop_reason == StopReason::Error && is_context_overflow(message, 0);
            let same_model = message_model.is_some();
            let overflow = same_model
                && ((explicit && retained)
                    || (usage_matches && is_context_overflow(message, context_window)));
            let length = message_model.as_ref().is_some_and(|model| {
                projected && is_recoverable_length(message, model.max_tokens)
            });
            if overflow || length {
                let will_retry = message.stop_reason != StopReason::Stop;
                if !will_retry {
                    return CompactionCheck::Overflow { will_retry: false };
                }
                if overflow_attempted {
                    return CompactionCheck::OverflowFailed(if overflow {
                        "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.".into()
                    } else {
                        "Truncated response recovery failed after one compact-and-retry attempt.".into()
                    });
                }
                return CompactionCheck::Overflow { will_retry: true };
            }
            let has_edits = projection
                .entries
                .iter()
                .any(|entry| matches!(entry.source, FileEntry::ContextEdit(_)));
            let direct = calculate_context_tokens(&message.usage);
            let tokens = if has_edits {
                estimate_projected_context_tokens(&projection, &branch).tokens
            } else if message.stop_reason == StopReason::Error || direct == 0 {
                let estimate = estimate_context_tokens(&projection.messages);
                if let Some(index) = estimate.last_usage_index
                    && let Message::Assistant(usage_message) = &projection.messages[index]
                    && compaction_time.is_some_and(|time| usage_message.timestamp <= time)
                {
                    return CompactionCheck::None;
                }
                estimate.tokens
            } else {
                direct
            };
            if should_compact(tokens, context_window, &settings) {
                CompactionCheck::Threshold
            } else {
                CompactionCheck::None
            }
        });
        match check {
            CompactionCheck::None => false,
            CompactionCheck::Threshold => {
                self.run_auto_compaction(CompactionReason::Threshold, false, cancel)
                    .await
            }
            CompactionCheck::OverflowFailed(error_message) => {
                self.emit_compaction_end(
                    CompactionReason::Overflow,
                    Err(Some(error_message)),
                    false,
                );
                false
            }
            CompactionCheck::Overflow { will_retry } => {
                if will_retry {
                    lock(&self.inner.recovery).overflow_recovery_attempted = true;
                    let ids: Vec<String> = entry_id.into_iter().chain(tool_results).collect();
                    self.omit_recovery_attempt(&ids);
                }
                self.run_auto_compaction(CompactionReason::Overflow, will_retry, cancel)
                    .await
            }
        }
    }

    /// A summarizer for `model` with the session's credentials and retry
    /// policy, reporting retries as `source` summaries.
    pub(super) async fn summarizer<'a>(
        &'a self,
        model: &'a Model,
        thinking_level: ThinkingLevel,
        source: SummarySource,
        reason: Option<CompactionReason>,
        cancel: CancellationToken,
    ) -> Summarizer<'a> {
        Summarizer {
            model,
            apis: &self.inner.apis,
            auth: self.registry().auth(model).await,
            thinking_level,
            retry: self.retry_policy(),
            cancel,
            on_retry: Box::new(move |retry| self.emit_summary_retry(retry, source, reason)),
        }
    }

    /// What manual and automatic compaction share: summarizes `preparation`
    /// and records the compaction with the estimate after it, unless `cancel`
    /// fired meanwhile.
    async fn run_compaction(
        &self,
        model: &Model,
        preparation: &Preparation,
        custom_instructions: Option<&str>,
        reason: CompactionReason,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, String> {
        let summarizer = self
            .summarizer(
                model,
                self.thinking_level(),
                SummarySource::Compaction,
                Some(reason),
                cancel.clone(),
            )
            .await;
        let mut result = summarizer.compact(preparation, custom_instructions).await?;
        // As pi, a summary cut short by an abort is not recorded.
        if cancel.is_cancelled() {
            return Err("Compaction cancelled".to_owned());
        }
        let estimate = self.with_session(|session| {
            let _ = session.append_compaction(
                result.summary.clone(),
                Some(result.first_kept_entry_id.clone()),
                result.tokens_before,
                result.details.clone(),
                Some(false),
                result.usage.clone(),
            );
            session
                .build_context()
                .messages
                .iter()
                .map(estimate_tokens)
                .sum()
        });
        result.estimated_tokens_after = Some(estimate);
        Ok(result)
    }

    /// pi's `compaction_end` event: with the result, or aborted (`Err(None)`),
    /// or failed with an error message.
    fn emit_compaction_end(
        &self,
        reason: CompactionReason,
        outcome: Result<CompactionResult, Option<String>>,
        will_retry: bool,
    ) {
        let (result, aborted, error_message) = match outcome {
            Ok(result) => (Some(result), false, None),
            Err(None) => (None, true, None),
            Err(Some(error)) => (None, false, Some(error)),
        };
        self.emit(&AgentEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        });
    }

    /// pi's `summarization_retry_*` events for a summary request's retries.
    fn emit_summary_retry(
        &self,
        retry: SummaryRetry,
        source: SummarySource,
        reason: Option<CompactionReason>,
    ) {
        self.emit(&match retry {
            SummaryRetry::Scheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => AgentEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            },
            SummaryRetry::AttemptStart => {
                AgentEvent::SummarizationRetryAttemptStart { source, reason }
            }
            SummaryRetry::Finished => AgentEvent::SummarizationRetryFinished,
        });
    }

    async fn run_auto_compaction(
        &self,
        reason: CompactionReason,
        will_retry: bool,
        cancel: &CancellationToken,
    ) -> bool {
        let Some(model) = self.model() else {
            return false;
        };
        let settings =
            CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
        let Some(preparation) =
            self.with_session(|session| prepare_compaction(&session.branch_path(None), settings))
        else {
            return false;
        };
        self.emit(&AgentEvent::CompactionStart { reason });
        let compacting = Compacting::start(&self.inner.compacting);
        let outcome = self
            .run_compaction(&model, &preparation, None, reason, cancel)
            .await;
        drop(compacting);
        self.inner.idle.notify_waiters();
        match outcome {
            Ok(result) => {
                self.emit_compaction_end(reason, Ok(result), will_retry);
                will_retry || self.has_queued()
            }
            Err(message) => {
                let error_message = (!cancel.is_cancelled()).then(|| match reason {
                    CompactionReason::Overflow => {
                        format!("Context overflow recovery failed: {message}")
                    }
                    _ => format!("Auto-compaction failed: {message}"),
                });
                self.emit_compaction_end(reason, Err(error_message), false);
                false
            }
        }
    }

    /// Compacts the session now, optionally focused by `custom_instructions`.
    /// Aborts a running response first.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, String> {
        self.abort();
        self.wait_for_idle().await;
        let cancel = CancellationToken::new();
        *lock(&self.inner.cancel) = Some(cancel.clone());
        let compacting = Compacting::start(&self.inner.compacting);
        self.inner.manual_compaction.store(true, Ordering::SeqCst);
        self.emit(&AgentEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });
        let outcome = async {
            let model = self
                .model()
                .ok_or_else(|| crate::auth_guidance::no_model_selected(&self.inner.docs))?;
            let settings =
                CompactionSettings::resolve(lock(&self.inner.settings).settings(), Some(&model));
            let preparation = self.with_session(|session| {
                let branch = session.branch_path(None);
                match prepare_compaction(&branch, settings) {
                    Some(preparation) => Ok(preparation),
                    None if matches!(branch.last(), Some(FileEntry::Compaction(_))) => {
                        Err("Already compacted".to_owned())
                    }
                    None => Err("Nothing to compact (session too small)".to_owned()),
                }
            })?;
            self.run_compaction(
                &model,
                &preparation,
                custom_instructions,
                CompactionReason::Manual,
                &cancel,
            )
            .await
        }
        .await;
        *lock(&self.inner.cancel) = None;
        self.inner.manual_compaction.store(false, Ordering::SeqCst);
        drop(compacting);
        // Waiters for idle run once this returns, after its own outcome.
        self.inner.idle.notify_waiters();
        let reason = CompactionReason::Manual;
        if cancel.is_cancelled() {
            self.emit_compaction_end(reason, Err(None), false);
            // pi reports the summarizer's error, such as an aborted request.
            return Err(outcome
                .err()
                .unwrap_or_else(|| "Compaction cancelled".into()));
        }
        match &outcome {
            Ok(result) => self.emit_compaction_end(reason, Ok(result.clone()), false),
            Err(message) => self.emit_compaction_end(
                reason,
                Err(Some(format!("Compaction failed: {message}"))),
                false,
            ),
        }
        outcome
    }
}
