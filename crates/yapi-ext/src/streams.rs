//! Assistant message streams across the guest boundary, in both directions:
//! extension streams the session uses, and yapi's wire APIs extensions call
//! through pi-ai.
//!
//! An event crosses as `{type, ...}`: `start` with the message so far,
//! `update` with pi's event (without the live message) and the usage so far,
//! `done` and `error` with the final message. Options cross as pi's
//! `SimpleStreamOptions`.

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use yapi_ai::stream::{CacheRetention, StreamEvent, StreamOptions, ThinkingBudgets};
use yapi_types::message::ThinkingLevel;

/// pi's `SimpleStreamOptions` for `options`: what pi's agent passes, which
/// leaves retries to the provider's default.
pub(crate) fn options_json(options: &StreamOptions) -> Value {
    let mut out = Map::new();
    let mut set = |key: &str, value: Value| {
        if !value.is_null() {
            out.insert(key.to_owned(), value);
        }
    };
    set("apiKey", json!(options.api_key));
    if !options.headers.is_empty() {
        set("headers", json!(options.headers));
    }
    set(
        "reasoning",
        json!(
            options
                .reasoning
                .filter(|level| *level != ThinkingLevel::Off)
                .map(ThinkingLevel::as_str)
        ),
    );
    set("maxTokens", json!(options.max_tokens));
    set("temperature", json!(options.temperature));
    set("sessionId", json!(options.session_id));
    set(
        "cacheRetention",
        json!(options.cache_retention.map(CacheRetention::as_str)),
    );
    let budgets = &options.thinking_budgets;
    if *budgets != ThinkingBudgets::default() {
        set(
            "thinkingBudgets",
            json!({"minimal": budgets.minimal, "low": budgets.low, "medium": budgets.medium, "high": budgets.high}),
        );
    }
    set("maxRetryDelayMs", json!(options.max_retry_delay_ms));
    set("env", json!(options.env));
    Value::Object(out)
}

/// The options pi's `SimpleStreamOptions` in `value` describe, cancelled by
/// `cancel`. Unknown fields are ignored.
pub(crate) fn options_from_json(value: &Value, cancel: CancellationToken) -> StreamOptions {
    fn parse<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Option<T> {
        serde_json::from_value(value[key].clone()).ok()
    }
    let budgets = &value["thinkingBudgets"];
    StreamOptions {
        api_key: parse(value, "apiKey"),
        headers: parse(value, "headers").unwrap_or_default(),
        reasoning: value["reasoning"].as_str().and_then(ThinkingLevel::parse),
        max_tokens: parse(value, "maxTokens"),
        temperature: parse(value, "temperature"),
        session_id: parse(value, "sessionId"),
        cache_retention: value["cacheRetention"]
            .as_str()
            .and_then(CacheRetention::parse),
        thinking_budgets: ThinkingBudgets {
            minimal: budgets["minimal"].as_u64(),
            low: budgets["low"].as_u64(),
            medium: budgets["medium"].as_u64(),
            high: budgets["high"].as_u64(),
        },
        max_retries: parse(value, "maxRetries").unwrap_or_default(),
        max_retry_delay_ms: parse(value, "maxRetryDelayMs"),
        cancel,
        env: parse(value, "env"),
        hooks: Default::default(),
    }
}

/// `event` as it crosses into the guest.
pub(crate) fn event_json(event: &StreamEvent) -> Value {
    match event {
        StreamEvent::Start(message) => json!({"type": "start", "message": message}),
        StreamEvent::Update { event, usage } => {
            json!({"type": "update", "event": event, "usage": usage})
        }
        StreamEvent::Done(message) => json!({"type": "done", "message": message}),
        StreamEvent::Error(message) => json!({"type": "error", "message": message}),
    }
}

/// The event the guest sent in `payload`.
pub(crate) fn event_from_json(payload: &Value) -> Result<StreamEvent, String> {
    let message = || {
        serde_json::from_value(payload["message"].clone())
            .map_err(|err| format!("Invalid assistant message from extension stream: {err}"))
    };
    Ok(match payload["type"].as_str() {
        Some("start") => StreamEvent::Start(message()?),
        Some("done") => StreamEvent::Done(message()?),
        Some("error") => StreamEvent::Error(message()?),
        _ => StreamEvent::Update {
            event: serde_json::from_value(payload["event"].clone())
                .map_err(|err| format!("Invalid stream event from extension: {err}"))?,
            usage: serde_json::from_value(payload["usage"].clone()).unwrap_or_default(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Options survive the trip into the guest and back.
    #[test]
    fn options_round_trip() {
        let mut options = StreamOptions {
            api_key: Some("key".into()),
            reasoning: Some(ThinkingLevel::High),
            max_tokens: Some(100),
            session_id: Some("session".into()),
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        options.headers.insert("x-a".into(), Some("1".into()));
        options.headers.insert("x-b".into(), None);
        options.thinking_budgets.low = Some(10);
        let value = options_json(&options);
        assert_eq!(value["headers"], json!({"x-a": "1", "x-b": null}));
        assert_eq!(value["thinkingBudgets"]["low"], 10);
        let back = options_from_json(&value, CancellationToken::new());
        assert_eq!(options_json(&back), value);
        // Off is no reasoning, as pi's agent sends it.
        let off = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..StreamOptions::default()
        };
        assert_eq!(options_json(&off), json!({}));
    }
}
