#![doc = env!("CARGO_PKG_DESCRIPTION")]
//!
//! A [`Cassette`] lists the HTTP interactions a client is expected to make, in order.
//! [`MockServer`] serves them on a local port, one per request, and records every
//! request it receives. Point a provider's base URL at [`MockServer::url`]: ri's tests
//! do so in process, and pi does so through `cargo xtask mock-sse`.
#![forbid(unsafe_code)]

mod cassette;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Channel};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

pub use cassette::{Cassette, Interaction, RequestMatch, Response};

/// Errors from loading a cassette or running the server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The cassette file could not be read.
    #[error("cannot read cassette {}: {source}", path.display())]
    Read {
        /// Cassette path.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The cassette file is not valid cassette JSON.
    #[error("invalid cassette {}: {source}", path.display())]
    Parse {
        /// Cassette path.
        path: PathBuf,
        /// Underlying error.
        source: serde_json::Error,
    },
    /// The listening socket could not be opened.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        /// Requested address.
        addr: SocketAddr,
        /// Underlying error.
        source: io::Error,
    },
    /// Requests did not match the cassette, or interactions were left unused.
    #[error("cassette not satisfied:\n  {}", .0.join("\n  "))]
    Unsatisfied(Vec<String>),
}

/// A request as the server received it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Query string without the leading `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Headers with lowercase names; repeated headers are joined with `, `.
    pub headers: IndexMap<String, String>,
    /// Body as text; invalid UTF-8 is replaced.
    pub body: String,
}

/// A running mock server. Dropping it stops the server.
#[derive(Debug)]
pub struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

#[derive(Debug, Default)]
struct State {
    pending: VecDeque<Interaction>,
    requests: Vec<RecordedRequest>,
    problems: Vec<String>,
}

impl MockServer {
    /// Starts serving `cassette` on `addr`; port 0 picks a free port. Must be called
    /// inside a Tokio runtime.
    pub async fn start(addr: SocketAddr, cassette: Cassette) -> Result<Self, Error> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| Error::Bind { addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| Error::Bind { addr, source })?;
        let state = Arc::new(Mutex::new(State {
            pending: cassette.interactions.into(),
            ..State::default()
        }));
        let task = tokio::spawn(accept(listener, Arc::clone(&state)));
        Ok(Self { addr, state, task })
    }

    /// Base URL, such as `http://127.0.0.1:41234`, without a trailing slash.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Requests received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state).requests.clone()
    }

    /// Returns the received requests if every request matched the cassette and every
    /// interaction was used.
    pub fn finish(&self) -> Result<Vec<RecordedRequest>, Error> {
        let state = lock(&self.state);
        let mut problems = state.problems.clone();
        problems.extend(state.pending.iter().map(|interaction| {
            let RequestMatch { method, path } = &interaction.request;
            format!("never requested: {method} {path}")
        }));
        if problems.is_empty() {
            Ok(state.requests.clone())
        } else {
            Err(Error::Unsatisfied(problems))
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        // Aborting the accept loop drops its JoinSet, which aborts open connections.
        self.task.abort();
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // A panic while holding the lock cannot leave `State` inconsistent: every update
    // is a single push or pop.
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn accept(listener: TcpListener, state: Arc<Mutex<State>>) {
    let mut connections = JoinSet::new();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                lock(&state).problems.push(format!("accept failed: {err}"));
                return;
            }
        };
        let state = Arc::clone(&state);
        connections.spawn(async move {
            let service = service_fn(move |request| respond(Arc::clone(&state), request));
            // A client closing mid-stream ends the connection with an error; that is
            // the client's choice, not a cassette mismatch.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        while connections.try_join_next().is_some() {}
    }
}

async fn respond(
    state: Arc<Mutex<State>>,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<Channel<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    let mut headers = IndexMap::<String, String>::new();
    for (name, value) in &parts.headers {
        let value = String::from_utf8_lossy(value.as_bytes());
        headers
            .entry(name.as_str().to_owned())
            .and_modify(|joined| {
                joined.push_str(", ");
                joined.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    let recorded = RecordedRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().map(str::to_owned),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };

    let reply = {
        let mut state = lock(&state);
        let number = state.requests.len() + 1;
        let label = format!("request {number} ({} {})", recorded.method, recorded.path);
        state.requests.push(recorded);
        match state.pending.pop_front() {
            Some(interaction) if matches(&interaction.request, &state.requests[number - 1]) => {
                Ok(interaction.response)
            }
            Some(interaction) => {
                let RequestMatch { method, path } = interaction.request;
                Err(format!("{label}: expected {method} {path}"))
            }
            None => Err(format!("{label}: no interactions left")),
        }
    };
    let response = match reply.and_then(|response| stream(response).map_err(|err| err.to_string()))
    {
        Ok(response) => response,
        Err(problem) => {
            lock(&state).problems.push(problem.clone());
            failure(problem)
        }
    };
    Ok(response)
}

fn matches(expected: &RequestMatch, actual: &RecordedRequest) -> bool {
    expected.method.eq_ignore_ascii_case(&actual.method) && expected.path == actual.path
}

/// Builds the response and streams its chunks from a separate task.
fn stream(response: Response) -> Result<hyper::Response<Channel<Bytes>>, hyper::http::Error> {
    let mut builder = hyper::Response::builder().status(response.status);
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    let (mut sender, body) = Channel::new(1);
    let built = builder.body(body)?;
    let delay = Duration::from_millis(response.chunk_delay_ms);
    tokio::spawn(async move {
        for chunk in response.chunks {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if sender.send_data(Bytes::from(chunk)).await.is_err() {
                return;
            }
        }
    });
    Ok(built)
}

/// A 500 response describing why the request was not served.
fn failure(problem: String) -> hyper::Response<Channel<Bytes>> {
    let body = serde_json::json!({ "error": { "type": "ri_mock", "message": problem } });
    let (mut sender, channel) = Channel::new(1);
    // A one-frame channel always has room for the first frame.
    let _ = sender.try_send(hyper::body::Frame::data(Bytes::from(body.to_string())));
    let mut response = hyper::Response::new(channel);
    *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}
