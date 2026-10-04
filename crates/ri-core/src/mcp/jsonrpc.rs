//! JSON-RPC 2.0 messages as MCP exchanges them, and the client's errors. Port
//! of `protocol/jsonrpc.ts` in pi-mcp `v1.0.0`.

use serde_json::{Map, Value};

/// The request was not valid JSON-RPC.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The handler failed.
pub const INTERNAL_ERROR: i64 = -32603;

/// Why an MCP request or connection failed. Messages match pi-mcp's.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum McpError {
    /// The server answered with a JSON-RPC error, or sent something invalid.
    #[error("{message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// The server's message.
        message: String,
        /// Extra data the server attached.
        data: Option<Value>,
    },
    /// The connection is closed or not open.
    #[error("{0}")]
    Closed(String),
    /// No response within the timeout.
    #[error("MCP request timed out after {0}ms")]
    Timeout(u64),
    /// The caller cancelled the request.
    #[error("MCP request aborted")]
    Aborted,
    /// An HTTP request failed with a status.
    #[error("{message}")]
    Http {
        /// HTTP status.
        status: u16,
        /// Description with the start of the body.
        message: String,
    },
    /// The server answered 401.
    #[error("MCP server requires authentication")]
    AuthRequired {
        /// The `WWW-Authenticate` challenge.
        challenge: Option<String>,
    },
    /// The server no longer knows the session (404 with a session id).
    #[error("MCP session expired")]
    SessionExpired,
    /// The request could not reach the server.
    #[error("{0}")]
    Network(String),
    /// Anything else, such as a server process that could not start.
    #[error("{0}")]
    Other(String),
}

impl McpError {
    /// pi's `McpConnectionClosedError` with its default message.
    pub fn closed() -> McpError {
        McpError::Closed("MCP connection closed".into())
    }

    /// An invalid-request error with `message`.
    pub fn invalid(message: impl Into<String>) -> McpError {
        McpError::Rpc {
            code: INVALID_REQUEST,
            message: message.into(),
            data: None,
        }
    }

    /// The HTTP status, for HTTP failures.
    pub fn status(&self) -> Option<u16> {
        match self {
            McpError::Http { status, .. } => Some(*status),
            McpError::AuthRequired { .. } => Some(401),
            McpError::SessionExpired => Some(404),
            _ => None,
        }
    }

    /// Network failures and overloaded or restarting servers, worth another
    /// attempt.
    pub fn is_transient(&self) -> bool {
        match self {
            McpError::Network(_) => true,
            McpError::Http { status, .. } => {
                *status == 408 || *status == 429 || (*status >= 500 && *status != 501)
            }
            _ => false,
        }
    }
}

/// A message as received, classified.
#[derive(Debug, PartialEq)]
pub enum Incoming<'a> {
    /// A request from the server.
    Request {
        /// Its id.
        id: &'a Value,
        /// Its method.
        method: &'a str,
        /// Its parameters.
        params: Option<&'a Value>,
    },
    /// A notification from the server.
    Notification {
        /// Its method.
        method: &'a str,
        /// Its parameters.
        params: Option<&'a Value>,
    },
    /// A response to one of our requests.
    Response {
        /// The request's id.
        id: &'a Value,
        /// The result, or the error.
        outcome: Result<&'a Value, McpError>,
    },
}

/// Whether `value` can be a JSON-RPC id: a string or a finite number.
pub fn is_id(value: &Value) -> bool {
    value.is_string() || value.as_f64().is_some_and(f64::is_finite)
}

/// Classifies a message; `None` when it is not valid JSON-RPC.
pub fn classify(value: &Value) -> Option<Incoming<'_>> {
    let object = value.as_object()?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return None;
    }
    let method = object.get("method").and_then(Value::as_str);
    match (object.get("id"), method) {
        (Some(id), Some(method)) if is_id(id) => Some(Incoming::Request {
            id,
            method,
            params: object.get("params"),
        }),
        (None, Some(method)) => Some(Incoming::Notification {
            method,
            params: object.get("params"),
        }),
        (Some(id), None) if is_id(id) => response(id, object),
        _ => None,
    }
}

fn response<'a>(id: &'a Value, object: &'a Map<String, Value>) -> Option<Incoming<'a>> {
    if let Some(result) = object.get("result") {
        return (!object.contains_key("error")).then_some(Incoming::Response {
            id,
            outcome: Ok(result),
        });
    }
    let error = object.get("error")?.as_object()?;
    let code = error.get("code")?.as_f64()?;
    let message = error.get("message")?.as_str()?;
    Some(Incoming::Response {
        id,
        outcome: Err(McpError::Rpc {
            code: code as i64,
            message: message.to_owned(),
            data: error.get("data").cloned(),
        }),
    })
}

/// A key for an id, so string `"1"` and number `1` stay distinct.
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_like_pi() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        assert!(matches!(
            classify(&request),
            Some(Incoming::Request { method: "ping", .. })
        ));
        let notification =
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
        assert!(matches!(
            classify(&notification),
            Some(Incoming::Notification { .. })
        ));
        let failure =
            json!({"jsonrpc": "2.0", "id": "a", "error": {"code": -32601, "message": "nope"}});
        assert_eq!(
            classify(&failure),
            Some(Incoming::Response {
                id: &json!("a"),
                outcome: Err(McpError::Rpc {
                    code: -32601,
                    message: "nope".into(),
                    data: None
                })
            })
        );
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {}})),
            None
        );
        assert_eq!(classify(&json!({"jsonrpc": "1.0", "method": "x"})), None);
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }
}
