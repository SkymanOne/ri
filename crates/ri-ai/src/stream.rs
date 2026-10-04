//! Streaming one assistant message from a provider.

use indexmap::IndexMap;
use ri_types::event::AssistantMessageEvent;
use ri_types::message::{AssistantMessage, Message, StopReason, ThinkingLevel, Usage};
use ri_types::model::Model;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A wire API: turns a model, a transcript and options into a streamed message.
///
/// Implementations never fail the call itself. Every outcome, including request
/// errors and cancellation, arrives on the stream, which ends with exactly one
/// [`StreamEvent::Done`] or [`StreamEvent::Error`].
pub trait Provider: Send + Sync {
    /// The wire API id this provider speaks, such as `anthropic-messages`.
    fn api(&self) -> &str;

    /// Starts streaming a response. Must be called inside a tokio runtime.
    fn stream(&self, request: Request) -> EventStream;
}

/// Everything one provider request needs.
#[derive(Clone, Debug)]
pub struct Request {
    /// The model to call.
    pub model: Model,
    /// The transcript; the first message may be the system prompt.
    pub messages: Vec<Message>,
    /// Request options.
    pub options: StreamOptions,
}

/// How long the provider should keep the prompt cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheRetention {
    /// No caching.
    None,
    /// The provider's default retention.
    Short,
    /// Extended retention where offered, such as Anthropic's one hour.
    Long,
}

/// Request options shared by every wire API.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    /// Credential for the provider; some providers take headers instead.
    pub api_key: Option<String>,
    /// Extra headers; `None` removes a header the provider would otherwise send.
    pub headers: IndexMap<String, Option<String>>,
    /// Thinking effort; `None` and `Off` disable thinking where the API allows.
    pub reasoning: Option<ThinkingLevel>,
    /// Output token limit; the model's limit when absent.
    pub max_tokens: Option<u64>,
    /// Sampling temperature, where the model and thinking mode allow it.
    pub temperature: Option<f64>,
    /// Stable id for provider-side session affinity and caching.
    pub session_id: Option<String>,
    /// Prompt cache retention; `PI_CACHE_RETENTION=long` or short when absent.
    pub cache_retention: Option<CacheRetention>,
    /// Token budgets per level for budget-based thinking.
    pub thinking_budgets: ThinkingBudgets,
    /// Retries for retryable HTTP errors before the first byte; 0 disables.
    pub max_retries: u32,
    /// Largest server-requested retry delay to honor; 0 means no limit.
    pub max_retry_delay_ms: Option<u64>,
    /// Cancels the request; the stream then ends with an `aborted` error.
    pub cancel: CancellationToken,
}

impl StreamOptions {
    /// The cache retention to use: the option, else `long` when
    /// `PI_CACHE_RETENTION=long`, else `short`.
    pub fn resolved_cache_retention(&self) -> CacheRetention {
        self.cache_retention.unwrap_or_else(|| {
            if std::env::var("PI_CACHE_RETENTION").as_deref() == Ok("long") {
                CacheRetention::Long
            } else {
                CacheRetention::Short
            }
        })
    }

    /// Whether `headers` sets `name` (case-insensitive) to a non-blank value.
    pub fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case(name)
                && value
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
        })
    }
}

/// Overrides for budget-based thinking, in tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingBudgets {
    /// Default 1024.
    pub minimal: Option<u64>,
    /// Default 2048.
    pub low: Option<u64>,
    /// Default 8192.
    pub medium: Option<u64>,
    /// Default 16384; also used for `xhigh` and `max`.
    pub high: Option<u64>,
}

/// One step of a streamed response.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// The response started; the message so far, with no content yet.
    Start(AssistantMessage),
    /// A content change, with the cumulative usage after it.
    Update {
        /// What changed.
        event: AssistantMessageEvent,
        /// Usage so far.
        usage: Usage,
    },
    /// Finished with `stop`, `length`, `toolUse` or `deferred`.
    Done(AssistantMessage),
    /// Failed or aborted; the message carries what arrived and `errorMessage`.
    Error(AssistantMessage),
}

/// The receiving end of a provider stream.
#[derive(Debug)]
pub struct EventStream {
    receiver: mpsc::UnboundedReceiver<StreamEvent>,
}

impl EventStream {
    /// A stream fed by `sender`.
    pub fn channel() -> (EventSender, EventStream) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (EventSender { sender }, EventStream { receiver })
    }

    /// The next event; `None` after the final one.
    pub async fn next(&mut self) -> Option<StreamEvent> {
        self.receiver.recv().await
    }

    /// Drains the stream and returns the final message.
    pub async fn result(mut self) -> Option<AssistantMessage> {
        while let Some(event) = self.next().await {
            if let StreamEvent::Done(message) | StreamEvent::Error(message) = event {
                return Some(message);
            }
        }
        None
    }
}

/// The sending end of a provider stream. Sends after the receiver is gone are
/// dropped.
#[derive(Clone, Debug)]
pub struct EventSender {
    sender: mpsc::UnboundedSender<StreamEvent>,
}

impl EventSender {
    /// Sends an event.
    pub fn send(&self, event: StreamEvent) {
        let _ = self.sender.send(event);
    }

    /// Sends a content update with the message's current usage.
    pub fn update(&self, message: &AssistantMessage, event: AssistantMessageEvent) {
        self.send(StreamEvent::Update {
            event,
            usage: message.usage.clone(),
        });
    }
}

/// An empty assistant message for `model`, as providers start one.
pub fn new_output(model: &Model, now_ms: u64) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        provider_thinking_level: None,
        usage: Usage {
            total_tokens: Some(0),
            ..Usage::default()
        },
        stop_reason: StopReason::Pending,
        timestamp: now_ms,
        response_id: None,
        response_model: None,
        raw_stop_reason: None,
        deferred: None,
        error_message: None,
        diagnostics: None,
        end_turn: None,
        thinking_level: None,
    }
}

/// Ends a stream with an error: `aborted` when the request was cancelled.
pub fn send_error(
    sender: &EventSender,
    mut output: AssistantMessage,
    cancel: &CancellationToken,
    message: String,
) {
    output.stop_reason = if cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(message);
    sender.send(StreamEvent::Error(output));
}

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}
