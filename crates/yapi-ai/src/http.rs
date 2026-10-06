//! HTTP transport shared by the wire APIs: client, retries, cancellation, errors.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use indexmap::IndexMap;
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::sse;
use crate::stream::StreamOptions;

/// Message when the request is cancelled before a response arrives.
pub const ABORTED_BEFORE_RESPONSE: &str = "Request aborted";
/// Message when the request is cancelled while the response streams.
pub const ABORTED_DURING_STREAM: &str = "Request was aborted";
/// Message of the DOM `AbortError` that `fetch` raises when it is cancelled
/// during a pending body read.
pub const ABORTED_READ: &str = "This operation was aborted";

const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

static SETTINGS_PROXY: OnceLock<String> = OnceLock::new();

/// pi's default `httpIdleTimeoutMs`: five minutes.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 300_000;

static IDLE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(DEFAULT_IDLE_TIMEOUT_MS);

/// Sets how long provider requests may wait for response headers, and then
/// for each body chunk, before failing, as pi's `httpIdleTimeoutMs` sets
/// undici's `headersTimeout` and `bodyTimeout`; 0 waits forever.
pub fn set_idle_timeout_ms(ms: u64) {
    IDLE_TIMEOUT_MS.store(ms, Ordering::Relaxed);
}

fn idle_timeout() -> Option<Duration> {
    match IDLE_TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => None,
        ms => Some(Duration::from_millis(ms)),
    }
}

/// Waits for `future` within the idle timeout; `None` when it elapses.
async fn within_idle_timeout<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    match idle_timeout() {
        Some(limit) => tokio::time::timeout(limit, future).await.ok(),
        None => Some(future.await),
    }
}

/// Applies pi's `httpProxy` setting for this process: it serves as
/// `HTTP_PROXY` and `HTTPS_PROXY` where those are unset. Call it before the
/// first request; only the first call counts.
pub fn set_settings_proxy(proxy: Option<&str>) {
    if let Some(proxy) = proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) {
        let _ = SETTINGS_PROXY.set(proxy.to_owned());
    }
}

/// The proxy variables the `httpProxy` setting adds for child processes:
/// `HTTP_PROXY` and `HTTPS_PROXY`, each where the environment lacks it.
pub fn proxy_env() -> Vec<(&'static str, String)> {
    let Some(proxy) = SETTINGS_PROXY.get() else {
        return Vec::new();
    };
    ["HTTP_PROXY", "HTTPS_PROXY"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_none())
        .map(|name| (name, proxy.clone()))
        .collect()
}

/// The process-wide HTTP client. It honors the standard proxy variables and
/// the `httpProxy` setting.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder().user_agent(user_agent());
        for (name, proxy) in proxy_env() {
            let proxy = match name {
                "HTTP_PROXY" => reqwest::Proxy::http(&proxy),
                _ => reqwest::Proxy::https(&proxy),
            };
            if let Ok(proxy) = proxy {
                builder = builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_env()));
            }
        }
        builder.build().unwrap_or_default()
    })
}

/// `yapi (<os>; <arch>)`, after pi's `pi (<platform> <release>; <arch>)`.
pub fn user_agent() -> String {
    format!(
        "yapi/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Request headers as pi merges them (`providerHeadersToRecord`): names compare
/// case-insensitively, a later value replaces an earlier one at the end, and
/// `None` removes the header.
#[derive(Clone, Debug, Default)]
pub(crate) struct Headers(pub(crate) Vec<(String, String)>);

impl Headers {
    /// Sets `name` to `value`, or removes it for `None`.
    pub(crate) fn set(&mut self, name: &str, value: Option<impl Into<String>>) {
        self.0.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            self.0.push((name.to_owned(), value.into()));
        }
    }

    /// Sets each of `headers` in order, as [`Headers::set`] does.
    pub(crate) fn extend(&mut self, headers: &IndexMap<String, Option<String>>) {
        for (name, value) in headers {
            self.set(name, value.as_deref());
        }
    }

    /// Sets each of a model's configured `headers` in order.
    pub(crate) fn extend_model(&mut self, headers: Option<&IndexMap<String, String>>) {
        for (name, value) in headers.into_iter().flatten() {
            self.set(name, Some(value.as_str()));
        }
    }

    /// The value of `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `request` with these headers added.
    pub(crate) fn apply(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.0 {
            request = request.header(name.as_str(), value.as_str());
        }
        request
    }
}

/// Why a request produced no usable response.
#[derive(Debug)]
pub enum Failure {
    /// The server answered with a non-success status.
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body as text.
        body: String,
    },
    /// No response: connection, TLS or protocol failure.
    Connection(String),
    /// Cancelled by the caller.
    Aborted,
    /// The server asked to retry later than the limit allows; the message says why.
    RetryDelay(String),
}

impl Failure {
    /// The message pi reports for failures that carry no provider error body.
    pub fn plain_message(&self) -> Option<String> {
        match self {
            Failure::Status { .. } => None,
            Failure::Connection(message) | Failure::RetryDelay(message) => Some(message.clone()),
            Failure::Aborted => Some(ABORTED_BEFORE_RESPONSE.to_owned()),
        }
    }
}

fn retryable(failure: &Failure, headers: &HeaderMap) -> bool {
    match failure {
        Failure::Status { status, .. } => {
            match headers.get("x-should-retry").and_then(|v| v.to_str().ok()) {
                Some("true") => true,
                Some("false") => false,
                _ => matches!(status, 408 | 409 | 429) || *status >= 500,
            }
        }
        Failure::Connection(_) => true,
        Failure::Aborted | Failure::RetryDelay(_) => false,
    }
}

/// Sends a request built by `build`, retrying retryable failures up to
/// `options.max_retries` times with pi's backoff and honoring `retry-after`.
pub async fn send(
    build: impl Fn() -> reqwest::RequestBuilder,
    options: &StreamOptions,
) -> Result<reqwest::Response, Failure> {
    let mut retry = 0;
    loop {
        let result = tokio::select! {
            () = options.cancel.cancelled() => Err(Box::new((Failure::Aborted, HeaderMap::new()))),
            result = attempt(build()) => result,
        };
        let (failure, headers) = match result {
            Ok(response) => return Ok(response),
            Err(failure) => *failure,
        };
        if options.cancel.is_cancelled() {
            return Err(Failure::Aborted);
        }
        if retry >= options.max_retries || !retryable(&failure, &headers) {
            return Err(failure);
        }
        let delay = retry_delay(&headers, retry, options.max_retry_delay_ms)?;
        retry += 1;
        tokio::select! {
            () = options.cancel.cancelled() => return Err(Failure::Aborted),
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// A failed attempt, with the response headers that steer retries.
type Attempt = Box<(Failure, HeaderMap)>;

async fn attempt(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Attempt> {
    let failed = |message: String| Box::new((Failure::Connection(message), HeaderMap::new()));
    // Headers that do not arrive in time fail the fetch, which the SDKs
    // report as a connection error.
    let response = match within_idle_timeout(request.send()).await {
        Some(Ok(response)) => response,
        Some(Err(err)) => return Err(failed(connection_message(&err))),
        None => return Err(failed("Connection error.".to_owned())),
    };
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    Err(Box::new((Failure::Status { status, body }, headers)))
}

fn connection_message(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "Request timed out.".to_owned()
    } else {
        "Connection error.".to_owned()
    }
}

fn retry_delay(headers: &HeaderMap, retry: u32, max_ms: Option<u64>) -> Result<Duration, Failure> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let requested = header("retry-after-ms")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .or_else(|| {
            header("retry-after")
                .and_then(|value| value.trim().parse::<f64>().ok())
                .map(|seconds| seconds * 1000.0)
        });
    if let Some(ms) = requested.filter(|ms| ms.is_finite()) {
        let max = max_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
        if max > 0 && ms > max as f64 {
            return Err(Failure::RetryDelay(format!(
                "Server requested {}s retry delay (max: {}s).",
                (ms / 1000.0).ceil(),
                max.div_ceil(1000)
            )));
        }
        return Ok(Duration::from_millis(ms.max(0.0) as u64));
    }
    let exponential = (0.5 * 2f64.powi(retry as i32)).min(8.0) * 1000.0;
    // pi jitters by up to 25%; a fixed 12.5% keeps tests deterministic.
    Ok(Duration::from_millis((exponential * 0.875) as u64))
}

/// The error message the Stainless SDKs (Anthropic, OpenAI) give a failed status:
/// `<status> <error.message>`, the error as JSON, or the raw body.
pub fn sdk_status_message(status: u16, error: Option<&Value>, raw: Option<&str>) -> String {
    let message = match error {
        Some(error) => match error.get("message") {
            Some(Value::String(message)) if !message.is_empty() => Some(message.clone()),
            Some(message) if is_truthy(message) => Some(yapi_types::json::stringify(message)),
            _ if is_truthy(error) => Some(yapi_types::json::stringify(error)),
            _ => raw.map(str::to_owned),
        },
        None => raw.map(str::to_owned),
    };
    match message.filter(|message| !message.is_empty()) {
        Some(message) => format!("{status} {message}"),
        None => format!("{status} status code (no body)"),
    }
}

/// pi's `formatProviderError` for an OpenAI SDK status error: the SDK message
/// from the body's `error`, or from the raw body when it is not JSON.
pub(crate) fn openai_status_message(status: u16, body: &str, prefix: Option<&str>) -> String {
    let json = serde_json::from_str::<Value>(body).ok();
    let error = json.as_ref().and_then(|json| json.get("error"));
    let message = sdk_status_message(status, error, json.is_none().then_some(body));
    provider_error_message(&message, Some(status), error, prefix)
}

/// The message of an error the OpenAI SDK raises without a status, such as an
/// error payload inside a stream: its `message`, or the error as JSON.
pub fn sdk_error_message(error: &Value) -> String {
    match error.get("message") {
        Some(Value::String(message)) if !message.is_empty() => message.clone(),
        Some(message) if is_truthy(message) => yapi_types::json::stringify(message),
        _ if is_truthy(error) => yapi_types::json::stringify(error),
        _ => "(no status code or body)".to_owned(),
    }
}

/// Message for a stream event whose data is not JSON, as the OpenAI SDK words it.
pub const MALFORMED_SSE_JSON: &str = "Error reading response: malformed server-sent event JSON.";

/// The next JSON chunk of an OpenAI SDK stream. Ends at `[DONE]` or the end of
/// the body; skips `thread.*` events; fails on malformed JSON, an `error` event
/// or a payload with a truthy `error`.
pub async fn next_openai_chunk(
    reader: &mut SseReader,
    cancel: &CancellationToken,
) -> Result<Option<Value>, String> {
    loop {
        let Some(sse) = reader.next(cancel).await? else {
            return Ok(None);
        };
        if sse.data == "[DONE]" {
            return Ok(None);
        }
        let data: Value =
            serde_json::from_str(&sse.data).map_err(|_| MALFORMED_SSE_JSON.to_owned())?;
        match sse.event.as_deref() {
            Some(event) if event.starts_with("thread.") => continue,
            Some("error") => {
                let error = data.get("error").filter(|error| !error.is_null());
                return Err(sdk_error_message(error.unwrap_or(&data)));
            }
            _ => {}
        }
        if let Some(error) = data.get("error").filter(|error| is_truthy(error)) {
            return Err(sdk_error_message(error));
        }
        return Ok(Some(data));
    }
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

const MAX_ERROR_BODY_CHARS: usize = 4000;

/// pi's `formatProviderError`: the SDK message, or the status and body when the
/// message does not already carry the error body. With a prefix, either form reads
/// `<prefix> (<status>): <text>`; without one, a body reads `<status>: <body>`.
pub fn provider_error_message(
    message: &str,
    status: Option<u16>,
    body: Option<&Value>,
    prefix: Option<&str>,
) -> String {
    let body = body
        .filter(|body| body.as_object().is_some_and(|object| !object.is_empty()))
        .map(yapi_types::json::stringify)
        .map(|text| truncate_chars(text.trim(), MAX_ERROR_BODY_CHARS))
        .filter(|body| !body.is_empty() && !message.contains(body.as_str()));
    match (status, body, prefix) {
        (Some(status), Some(body), Some(prefix)) => format!("{prefix} ({status}): {body}"),
        (Some(status), Some(body), None) => format!("{status}: {body}"),
        (Some(status), None, Some(prefix)) => format!("{prefix} ({status}): {message}"),
        _ => message.to_owned(),
    }
}

/// `text` cut to `max` UTF-16 units with a `... [truncated N chars]` note, as
/// pi shortens error bodies.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let len = yapi_types::js::len(text);
    if len <= max {
        return text.to_owned();
    }
    format!(
        "{}... [truncated {} chars]",
        yapi_types::js::slice(text, 0, max),
        len - max
    )
}

/// The next chunk of a response body; `None` at the end. Fails with pi's
/// message when `cancel` fires or the connection drops: `interrupted` when it
/// fires during the read.
pub async fn read_chunk(
    response: &mut reqwest::Response,
    cancel: &CancellationToken,
    interrupted: &str,
) -> Result<Option<Vec<u8>>, String> {
    if cancel.is_cancelled() {
        return Err(ABORTED_DURING_STREAM.to_owned());
    }
    // A body that goes quiet for longer than the idle timeout fails as
    // undici's body timeout does.
    let chunk = tokio::select! {
        () = cancel.cancelled() => return Err(interrupted.to_owned()),
        chunk = within_idle_timeout(response.chunk()) => chunk,
    };
    chunk
        .and_then(Result::ok)
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .ok_or_else(|| "terminated".to_owned())
}

/// Reads server-sent events from a response until it ends or `cancel` fires.
pub struct SseReader {
    response: reqwest::Response,
    decoder: sse::Decoder,
    queue: std::collections::VecDeque<sse::Event>,
    done: bool,
    interrupted: &'static str,
}

impl SseReader {
    /// Reads `response` as an event stream, as the provider SDKs do: a
    /// cancelled read fails with [`ABORTED_DURING_STREAM`].
    pub fn new(response: reqwest::Response) -> SseReader {
        SseReader {
            response,
            decoder: sse::Decoder::default(),
            queue: Default::default(),
            done: false,
            interrupted: ABORTED_DURING_STREAM,
        }
    }

    /// Reads with plain `fetch`, as pi's Anthropic client does: a cancelled
    /// read fails with [`ABORTED_READ`].
    pub fn fetch(response: reqwest::Response) -> SseReader {
        SseReader {
            interrupted: ABORTED_READ,
            ..SseReader::new(response)
        }
    }

    /// The next event, `Ok(None)` at the end, or an error message.
    pub async fn next(&mut self, cancel: &CancellationToken) -> Result<Option<sse::Event>, String> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            match read_chunk(&mut self.response, cancel, self.interrupted).await? {
                Some(bytes) => self.queue.extend(self.decoder.push(&bytes)),
                None => {
                    self.done = true;
                    self.queue.extend(self.decoder.finish());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_proxy_fills_unset_variables() {
        assert!(proxy_env().is_empty());
        set_settings_proxy(Some("  "));
        assert!(proxy_env().is_empty());
        set_settings_proxy(Some(" http://proxy.test:8080 "));
        set_settings_proxy(Some("http://ignored.test"));
        let expected: Vec<(&str, String)> = ["HTTP_PROXY", "HTTPS_PROXY"]
            .into_iter()
            .filter(|name| std::env::var_os(name).is_none())
            .map(|name| (name, "http://proxy.test:8080".to_owned()))
            .collect();
        assert_eq!(proxy_env(), expected);
    }

    #[test]
    fn formats_like_the_sdks() {
        let anthropic =
            json!({"type":"error","error":{"type":"invalid_request_error","message":"bad"}});
        assert_eq!(
            sdk_status_message(400, Some(&anthropic), None),
            r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#
        );
        assert_eq!(
            sdk_status_message(404, Some(&json!({"message": "Not found"})), None),
            "404 Not found"
        );
        assert_eq!(
            sdk_status_message(502, None, Some("Bad gateway")),
            "502 Bad gateway"
        );
        assert_eq!(
            sdk_status_message(500, None, Some("")),
            "500 status code (no body)"
        );
        assert_eq!(
            sdk_status_message(500, None, None),
            "500 status code (no body)"
        );
    }

    #[test]
    fn provider_errors_carry_the_body() {
        let error = json!({"message": "Invalid model", "type": "invalid_request_error"});
        assert_eq!(
            provider_error_message("400 Invalid model", Some(400), Some(&error), None),
            r#"400: {"message":"Invalid model","type":"invalid_request_error"}"#
        );
        assert_eq!(
            provider_error_message("Connection error.", None, None, None),
            "Connection error."
        );
        assert_eq!(
            provider_error_message(
                "400 Invalid model",
                Some(400),
                Some(&error),
                Some("OpenAI API error")
            ),
            r#"OpenAI API error (400): {"message":"Invalid model","type":"invalid_request_error"}"#
        );
        assert_eq!(
            provider_error_message(
                "502 status code (no body)",
                Some(502),
                None,
                Some("xai API error")
            ),
            "xai API error (502): 502 status code (no body)"
        );
    }

    #[test]
    fn honors_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "2".parse().unwrap());
        let failure = Failure::Status {
            status: 429,
            body: String::new(),
        };
        assert!(retryable(&failure, &headers));
        assert_eq!(
            retry_delay(&headers, 0, None).unwrap(),
            Duration::from_secs(2)
        );
        assert!(matches!(
            retry_delay(&headers, 0, Some(1000)),
            Err(Failure::RetryDelay(_))
        ));
    }
}
