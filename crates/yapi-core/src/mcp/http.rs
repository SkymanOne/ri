//! The streamable HTTP transport: each message is a POST whose reply is JSON
//! or an event stream, plus a GET stream for messages the server starts. Port
//! of `transports/streamable-http.ts` in pi-mcp `v1.0.0`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_types::sync::lock;

use super::jsonrpc::{INTERNAL_ERROR, Incoming, McpError, classify};
use super::oauth::is_insufficient_scope;
use super::sign_in::McpAuth;
use super::transport::{Event, Events, MAX_MESSAGE_BYTES};

const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_MESSAGE_BODY_CHARS: usize = 500;
const RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
const RECONNECT_MAX_DELAY_MS: u64 = 30_000;
const RECONNECT_MAX_RETRIES: u32 = 5;

/// The current token of a pi provider (`/login`), by provider id.
pub type ProviderToken = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Where a server's bearer token comes from; pi's `AuthProvider`.
#[derive(Clone)]
pub enum HttpAuth {
    /// OAuth credentials, refreshed after a 401.
    OAuth(Arc<McpAuth>),
    /// The token of a provider (`auth.provider`), read for every request.
    Provider {
        /// The provider id.
        provider: String,
        /// Reads the token; without it, none is sent.
        token: Option<ProviderToken>,
    },
}

impl HttpAuth {
    async fn token(&self) -> Result<Option<String>, McpError> {
        match self {
            HttpAuth::OAuth(auth) => auth.token().await,
            HttpAuth::Provider { provider, token } => Ok(match token {
                Some(token) => token(provider.clone()).await,
                None => None,
            }),
        }
    }
}

/// How to reach a streamable HTTP server.
#[derive(Clone, Default)]
pub struct HttpOptions {
    /// The endpoint.
    pub url: String,
    /// Headers sent with every request.
    pub headers: Vec<(String, String)>,
    /// Supplies the bearer token.
    pub auth: Option<HttpAuth>,
}

/// A streamable HTTP connection. Cheap to clone; clones share it.
#[derive(Clone)]
pub struct HttpTransport {
    inner: Arc<Inner>,
}

struct Inner {
    options: HttpOptions,
    cancel: CancellationToken,
    session: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    events: Mutex<Option<Events>>,
    started: AtomicBool,
    closed: AtomicBool,
    get_stream_started: AtomicBool,
}

fn network(error: reqwest::Error) -> McpError {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    McpError::Network(message)
}

/// A request that got no response, with the message of Node's `fetch`,
/// which pi shows.
fn fetch_failed(_error: reqwest::Error) -> McpError {
    McpError::Network("fetch failed".to_owned())
}

fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
}

fn describe_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if yapi_types::js::len(text) > ERROR_MESSAGE_BODY_CHARS {
        format!(
            "{}...",
            yapi_types::js::slice(text, 0, ERROR_MESSAGE_BODY_CHARS - 3)
        )
    } else {
        text.to_owned()
    };
    if snippet.is_empty() {
        format!("MCP HTTP request failed with status {status}")
    } else {
        format!("MCP HTTP request failed with status {status}: {snippet}")
    }
}

/// Where a stream is, for resuming it with `Last-Event-ID`.
#[derive(Default)]
struct Cursor {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    /// An event arrived since the stream was (re)opened.
    received: bool,
}

/// pi-mcp's SSE reader: LF-separated lines with an optional CR, `id` reported
/// for every event, events without data dropped.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
    data_bytes: usize,
}

struct SseEvent {
    event: Option<String>,
    data: String,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8], cursor: &mut Cursor) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            self.line(&line, cursor, &mut events)?;
        }
        if self.buffer.len() > MAX_MESSAGE_BYTES {
            return Err(format!("MCP SSE event exceeds {MAX_MESSAGE_BYTES} bytes"));
        }
        Ok(events)
    }

    fn finish(&mut self, cursor: &mut Cursor) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            self.line(&line, cursor, &mut events)?;
        }
        events.extend(self.dispatch());
        Ok(events)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        self.id = None;
        if self.data.is_empty() {
            return None;
        }
        self.data_bytes = 0;
        Some(SseEvent {
            event,
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }

    fn line(
        &mut self,
        line: &str,
        cursor: &mut Cursor,
        events: &mut Vec<SseEvent>,
    ) -> Result<(), String> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            events.extend(self.dispatch());
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                self.data_bytes += value.len() + usize::from(!self.data.is_empty());
                if self.data_bytes > MAX_MESSAGE_BYTES {
                    return Err(format!("MCP SSE event exceeds {MAX_MESSAGE_BYTES} bytes"));
                }
                self.data.push(value.to_owned());
            }
            "event" => self.event = Some(value.to_owned()),
            "id" if !value.contains('\0') => {
                self.id = Some(value.to_owned());
                cursor.last_event_id = Some(value.to_owned());
            }
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                cursor.retry_ms = value.parse().ok();
            }
            _ => {}
        }
        Ok(())
    }
}

impl HttpTransport {
    /// A transport for `options`.
    pub fn new(options: HttpOptions) -> HttpTransport {
        HttpTransport {
            inner: Arc::new(Inner {
                options,
                cancel: CancellationToken::new(),
                session: Mutex::new(None),
                protocol_version: Mutex::new(None),
                events: Mutex::new(None),
                started: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                get_stream_started: AtomicBool::new(false),
            }),
        }
    }

    /// The session id the server assigned.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.inner.session).clone()
    }

    /// Begins delivering messages to `events`.
    pub fn start(&self, events: Events) -> Result<(), McpError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(McpError::closed());
        }
        *lock(&self.inner.events) = Some(events);
        self.inner.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Sends `MCP-Protocol-Version` from now on.
    pub fn set_protocol_version(&self, version: &str) {
        *lock(&self.inner.protocol_version) = Some(version.to_owned());
    }

    fn emit(&self, event: Event) {
        if let Some(events) = lock(&self.inner.events).as_ref() {
            let _ = events.send(event);
        }
    }

    /// Posts one message; its reply arrives as events.
    pub async fn send(&self, message: &Value) -> Result<(), McpError> {
        if !self.inner.started.load(Ordering::SeqCst) || self.inner.closed.load(Ordering::SeqCst) {
            return Err(McpError::closed());
        }
        let body = yapi_types::json::stringify(message);
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = self
            .authorized(reqwest::Method::POST, Some(body), headers)
            .await?;
        let response = self.check(response).await?;
        self.capture_session(&response);
        let request_id = match classify(message) {
            Some(Incoming::Request { id, .. }) => id.clone(),
            _ => {
                // Notifications and responses get 202 and no reply.
                if message["method"] == "notifications/initialized" {
                    self.start_get_stream();
                }
                return Ok(());
            }
        };
        let status = response.status().as_u16();
        if status == 202 || status == 204 {
            return Err(McpError::Http {
                status,
                message: format!(
                    "MCP server accepted request {} without a response",
                    message["method"].as_str().unwrap_or_default()
                ),
            });
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let bytes = response.bytes().await.map_err(network)?;
                let body: Value = serde_json::from_slice(&bytes)
                    .map_err(|error| McpError::Other(error.to_string()))?;
                let items = match body {
                    Value::Array(items) => items,
                    item => vec![item],
                };
                for item in items {
                    match classify(&item) {
                        Some(_) => self.emit(Event::Message(item)),
                        None => return Err(McpError::invalid("Invalid JSON-RPC message")),
                    }
                }
                Ok(())
            }
            Some("text/event-stream") => {
                let transport = self.clone();
                tokio::spawn(async move { transport.read_response(response, request_id).await });
                Ok(())
            }
            other => Err(McpError::Http {
                status,
                message: format!(
                    "Unsupported MCP response content type: {}",
                    other.unwrap_or("missing")
                ),
            }),
        }
    }

    /// Ends the session: pending reads stop and the server is told with DELETE.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.cancel.cancel();
        if self.inner.started.load(Ordering::SeqCst) && self.session_id().is_some() {
            let request = async {
                // Without a token, the session expires on the server.
                let Ok(token) = self.token().await else {
                    return;
                };
                let _ = yapi_ai::http::client()
                    .delete(&self.inner.options.url)
                    .headers(self.headers(&HeaderMap::new(), token.as_deref()))
                    .send()
                    .await;
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), request).await;
        }
        self.emit(Event::Closed);
    }

    async fn token(&self) -> Result<Option<String>, McpError> {
        match &self.inner.options.auth {
            Some(auth) => auth.token().await,
            None => Ok(None),
        }
    }

    fn headers(&self, extra: &HeaderMap, token: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.inner.options.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        for (name, value) in extra {
            headers.insert(name, value.clone());
        }
        if let Some(session) = self.session_id()
            && let Ok(value) = HeaderValue::from_str(&session)
        {
            headers.insert("mcp-session-id", value);
        }
        if let Some(version) = lock(&self.inner.protocol_version).clone()
            && let Ok(value) = HeaderValue::from_str(&version)
        {
            headers.insert("mcp-protocol-version", value);
        }
        if let Some(token) = token.filter(|token| !token.is_empty())
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
        {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        headers
    }

    /// Sends a request with the auth headers. A 401, or a 403 asking for more
    /// scope, goes to the OAuth credentials once, and the request is retried
    /// with whatever they left behind.
    async fn authorized(
        &self,
        method: reqwest::Method,
        body: Option<String>,
        extra: HeaderMap,
    ) -> Result<reqwest::Response, McpError> {
        let mut attempt = 0;
        loop {
            let token = self.token().await?;
            let mut request = yapi_ai::http::client()
                .request(method.clone(), &self.inner.options.url)
                .headers(self.headers(&extra, token.as_deref()));
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let response = tokio::select! {
                () = self.inner.cancel.cancelled() => Err(McpError::closed()),
                response = request.send() => response.map_err(fetch_failed),
            }?;
            let Some(HttpAuth::OAuth(auth)) = &self.inner.options.auth else {
                return Ok(response);
            };
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let status = response.status().as_u16();
            let unauthorized =
                status == 401 || (status == 403 && is_insufficient_scope(challenge.as_deref()));
            if attempt > 0 || !unauthorized {
                return Ok(response);
            }
            drop(response);
            auth.on_unauthorized(challenge.as_deref(), token.as_deref())
                .await?;
            attempt += 1;
        }
    }

    async fn check(&self, response: reqwest::Response) -> Result<reqwest::Response, McpError> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(MAX_ERROR_BODY_BYTES).collect();
        if status == 401 {
            return Err(McpError::AuthRequired { challenge });
        }
        if status == 404 && self.session_id().is_some() {
            return Err(McpError::SessionExpired);
        }
        Err(McpError::Http {
            status,
            message: describe_failure(status, &body),
        })
    }

    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
        {
            *lock(&self.inner.session) = Some(session.to_owned());
        }
    }

    /// Reads an event stream to its end; whether it carried the response to
    /// `request_id`.
    async fn consume(
        &self,
        mut response: reqwest::Response,
        cursor: &mut Cursor,
        request_id: Option<&Value>,
    ) -> Result<bool, McpError> {
        let mut parser = SseParser::default();
        let mut answered = false;
        loop {
            let chunk = tokio::select! {
                () = self.inner.cancel.cancelled() => return Ok(answered),
                chunk = response.chunk() => chunk.map_err(network)?,
            };
            let done = chunk.is_none();
            let events = match chunk {
                Some(bytes) => parser.push(&bytes, cursor),
                None => parser.finish(cursor),
            }
            .map_err(McpError::Other)?;
            for event in events {
                cursor.received = true;
                if event.data.trim().is_empty()
                    || event.event.as_deref().is_some_and(|name| name != "message")
                {
                    continue;
                }
                let message = match serde_json::from_str::<Value>(&event.data) {
                    Ok(message) if classify(&message).is_some() => message,
                    Ok(_) => {
                        self.emit(Event::Error("Invalid JSON-RPC message".into()));
                        continue;
                    }
                    Err(error) => {
                        self.emit(Event::Error(error.to_string()));
                        continue;
                    }
                };
                if let (Some(id), Some(Incoming::Response { id: answer, .. })) =
                    (request_id, classify(&message))
                    && answer == id
                {
                    answered = true;
                }
                self.emit(Event::Message(message));
            }
            if done {
                return Ok(answered);
            }
        }
    }

    /// The stream answering one request. When it ends or breaks first and the
    /// server assigned event ids, it is resumed with `Last-Event-ID`;
    /// otherwise the request fails.
    async fn read_response(&self, response: reqwest::Response, request_id: Value) {
        let mut cursor = Cursor::default();
        let mut stream = Some(response);
        let mut failure: Option<McpError> = None;
        let mut attempt = 0;
        loop {
            if let Some(response) = stream.take() {
                match self.consume(response, &mut cursor, Some(&request_id)).await {
                    Ok(true) => return,
                    Ok(false) => failure = None,
                    Err(error) => failure = Some(error),
                }
            }
            if self.inner.closed.load(Ordering::SeqCst) {
                return;
            }
            if failure.as_ref().is_some_and(|error| !retryable(error)) {
                break;
            }
            if cursor.last_event_id.is_none() || attempt >= RECONNECT_MAX_RETRIES {
                break;
            }
            if cursor.received {
                attempt = 0;
            }
            cursor.received = false;
            let delay = reconnect_delay(attempt, cursor.retry_ms);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
            match self.open_stream(cursor.last_event_id.clone()).await {
                Ok(response) => stream = response,
                Err(error) => {
                    let stop = !retryable(&error);
                    failure = Some(error);
                    if stop {
                        break;
                    }
                }
            }
        }
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let reason = failure.map_or_else(
            || "stream ended without a response".to_owned(),
            |error| error.to_string(),
        );
        self.emit(Event::Message(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": INTERNAL_ERROR, "message": format!("MCP response stream failed: {reason}")},
        })));
    }

    fn start_get_stream(&self) {
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.get_stream_started.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let transport = self.clone();
        tokio::spawn(async move { transport.run_get_stream().await });
    }

    /// Keeps the server-to-client stream open, reconnecting with backoff.
    async fn run_get_stream(&self) {
        let mut cursor = Cursor::default();
        let mut attempt = 0;
        while !self.inner.closed.load(Ordering::SeqCst) {
            match self.open_stream(cursor.last_event_id.clone()).await {
                // The server offers no GET stream.
                Ok(None) => return,
                Ok(Some(response)) => {
                    let opened = Instant::now();
                    let result = self.consume(response, &mut cursor, None).await;
                    if let Err(error) = result {
                        if self.inner.closed.load(Ordering::SeqCst) {
                            return;
                        }
                        if !retryable(&error) {
                            self.emit(Event::Error(error.to_string()));
                            return;
                        }
                    } else if cursor.received
                        || opened.elapsed() > Duration::from_millis(RECONNECT_MAX_DELAY_MS)
                    {
                        attempt = 0;
                    }
                }
                Err(error) => {
                    if self.inner.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    if !retryable(&error) {
                        self.emit(Event::Error(error.to_string()));
                        return;
                    }
                }
            }
            cursor.received = false;
            if attempt >= RECONNECT_MAX_RETRIES {
                self.emit(Event::Error(
                    "MCP server-to-client stream dropped and could not be reopened".into(),
                ));
                return;
            }
            let delay = reconnect_delay(attempt, cursor.retry_ms);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
        }
    }

    /// Opens a GET stream; `None` when the server answers 405.
    async fn open_stream(
        &self,
        last_event_id: Option<String>,
    ) -> Result<Option<reqwest::Response>, McpError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        if let Some(id) = last_event_id
            && let Ok(value) = HeaderValue::from_str(&id)
        {
            headers.insert("last-event-id", value);
        }
        let response = self.authorized(reqwest::Method::GET, None, headers).await?;
        if response.status().as_u16() == 405 {
            return Ok(None);
        }
        let response = self.check(response).await?;
        self.capture_session(&response);
        match content_type(&response).as_deref() {
            Some("text/event-stream") => Ok(Some(response)),
            other => Err(McpError::Http {
                status: response.status().as_u16(),
                message: format!(
                    "Unsupported MCP GET response content type: {}",
                    other.unwrap_or("missing")
                ),
            }),
        }
    }

    /// False when the transport closed while waiting.
    async fn sleep(&self, delay: Duration) -> bool {
        tokio::select! {
            () = self.inner.cancel.cancelled() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }
}

fn retryable(error: &McpError) -> bool {
    match error {
        McpError::Network(_) => true,
        error => error
            .status()
            .is_some_and(|status| status == 408 || status == 429 || status >= 500),
    }
}

fn reconnect_delay(attempt: u32, server_ms: Option<u64>) -> Duration {
    let ms = server_ms.unwrap_or_else(|| {
        RECONNECT_INITIAL_DELAY_MS
            .saturating_mul(2u64.saturating_pow(attempt))
            .min(RECONNECT_MAX_DELAY_MS)
    });
    Duration::from_millis(ms)
}
