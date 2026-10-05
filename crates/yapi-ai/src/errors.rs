//! Classifying failed responses: context overflow, recoverable truncation, and
//! transient errors worth retrying.
//!
//! Port of `packages/ai/src/utils/overflow.ts` and `retry.ts` in pi `v1.0.0`.

use std::sync::OnceLock;

use regex_lite::{Regex, RegexBuilder};
use yapi_types::message::{AssistantMessage, StopReason};

/// Error texts that mean the input exceeded the context window.
const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt (?:is )?too long",
    r"prompt exceeds max length",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
];

/// Error texts that are never overflow, even when an overflow pattern matches.
const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable):",
    r"rate limit",
    r"too many requests",
];

const CEREBRAS_BODYLESS_OVERFLOW: &str = r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)";

/// Subscription and quota limits: they do not clear on retry.
const NON_RETRYABLE: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
    "subscription_sharing_usage_limit_exceeded",
];

/// Transient provider, transport and stream failures.
const RETRYABLE: &[&str] = &[
    "overloaded",
    "currently experiencing high demand",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "520",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "ResourceExhausted",
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
];

/// Default cap on one agent-level retry delay.
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

fn insensitive(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("constant pattern; `patterns_compile` checks each one")
}

struct Patterns {
    overflow: Vec<Regex>,
    non_overflow: Vec<Regex>,
    cerebras: Regex,
    non_retryable: Regex,
    retryable: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        overflow: OVERFLOW_PATTERNS.iter().map(|p| insensitive(p)).collect(),
        non_overflow: NON_OVERFLOW_PATTERNS
            .iter()
            .map(|p| insensitive(p))
            .collect(),
        cerebras: insensitive(CEREBRAS_BODYLESS_OVERFLOW),
        non_retryable: insensitive(&NON_RETRYABLE.join("|")),
        retryable: insensitive(&RETRYABLE.join("|")),
    })
}

/// Whether a response failed or was cut short because the input exceeded the
/// context window: an overflow error text, input above `context_window` on a
/// completed response, or a length stop with no output and a full window.
/// A `context_window` of 0 checks the error text only.
pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    let patterns = patterns();
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref().filter(|e| !e.is_empty())
        && !patterns.non_overflow.iter().any(|p| p.is_match(error))
        && (patterns.overflow.iter().any(|p| p.is_match(error))
            || (message.provider == "cerebras" && patterns.cerebras.is_match(error)))
    {
        return true;
    }
    let input = message.usage.input + message.usage.cache_read;
    if context_window > 0 && message.stop_reason == StopReason::Stop && input > context_window {
        return true;
    }
    context_window > 0
        && message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input as f64 >= context_window as f64 * 0.99
}

/// Whether a length stop ended below the intended output limit, so one
/// compact-and-retry attempt may recover it. `desired_max_output` is the limit
/// before any clamping to the context.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

/// Whether a failed response is transient and worth retrying.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    let Some(error) = message
        .error_message
        .as_deref()
        .filter(|error| message.stop_reason == StopReason::Error && !error.is_empty())
    else {
        return false;
    };
    let patterns = patterns();
    !patterns.non_retryable.is_match(error) && patterns.retryable.is_match(error)
}

/// The backoff before retry `attempt` (1-based): `base * 2^(attempt-1)`, capped.
pub fn retry_delay_ms(base_delay_ms: u64, max_delay_ms: Option<u64>, attempt: u32) -> u64 {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    base_delay_ms
        .saturating_mul(factor)
        .min(max_delay_ms.unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        let mut message = crate::stream::new_output(
            &serde_json::from_value(serde_json::json!({
                "id": "m", "name": "m", "api": "openai-responses", "provider": "openai",
                "baseUrl": "", "reasoning": false, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 100, "maxTokens": 10,
            }))
            .unwrap(),
            0,
        );
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error.into());
        message
    }

    #[test]
    fn patterns_compile() {
        for pattern in OVERFLOW_PATTERNS.iter().chain(NON_OVERFLOW_PATTERNS) {
            RegexBuilder::new(pattern).build().unwrap();
        }
        RegexBuilder::new(CEREBRAS_BODYLESS_OVERFLOW)
            .build()
            .unwrap();
    }

    #[test]
    fn classifies_errors() {
        assert!(is_context_overflow(
            &failed("prompt is too long: 213462 tokens > 200000 maximum"),
            0
        ));
        assert!(!is_context_overflow(
            &failed("ThrottlingException: Too many tokens, rate limit"),
            0
        ));
        assert!(is_retryable_assistant_error(&failed(
            "server_error: The server had an error"
        )));
        assert!(is_retryable_assistant_error(&failed(
            "OpenAI API error (500): {}"
        )));
        assert!(!is_retryable_assistant_error(&failed(
            "429 insufficient_quota"
        )));
        assert!(!is_retryable_assistant_error(&failed("400 bad request")));
        assert_eq!(retry_delay_ms(2000, None, 3), 8000);
        assert_eq!(retry_delay_ms(2000, None, 64), 60_000);
    }
}
