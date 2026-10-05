//! The two MCP transports behind one type.

use serde_json::Value;

use super::http::HttpTransport;
use super::jsonrpc::McpError;
use super::stdio::StdioTransport;

/// The largest message a transport accepts.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// What a transport delivers to its client.
#[derive(Debug)]
pub enum Event {
    /// A valid JSON-RPC message.
    Message(Value),
    /// A problem that does not end the connection, such as a malformed line.
    Error(String),
    /// The connection ended.
    Closed,
}

/// Where a transport delivers its events.
pub type Events = tokio::sync::mpsc::UnboundedSender<Event>;

/// A connection to a server.
pub enum Transport {
    /// A server process on stdin and stdout.
    Stdio(Box<StdioTransport>),
    /// A streamable HTTP endpoint.
    Http(HttpTransport),
}

impl Transport {
    /// Opens the connection.
    pub async fn start(&self, events: Events) -> Result<(), McpError> {
        match self {
            Transport::Stdio(stdio) => stdio.start(events).await,
            Transport::Http(http) => http.start(events),
        }
    }

    /// Sends one message.
    pub async fn send(&self, message: &Value) -> Result<(), McpError> {
        match self {
            Transport::Stdio(stdio) => stdio.send(message).await,
            Transport::Http(http) => http.send(message).await,
        }
    }

    /// Closes the connection.
    pub async fn close(&self) {
        match self {
            Transport::Stdio(stdio) => stdio.close().await,
            Transport::Http(http) => http.close().await,
        }
    }

    /// Records the negotiated protocol version, which HTTP sends as a header.
    pub fn set_protocol_version(&self, version: &str) {
        if let Transport::Http(http) = self {
            http.set_protocol_version(version);
        }
    }

    /// The end of a stdio server's stderr.
    pub fn stderr(&self) -> Option<String> {
        match self {
            Transport::Stdio(stdio) => Some(stdio.stderr()),
            Transport::Http(_) => None,
        }
    }
}
