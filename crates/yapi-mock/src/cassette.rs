//! The cassette file format.

use std::fs;
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::Error;

/// HTTP interactions a client is expected to make, in order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cassette {
    /// Served one per request, first to last.
    pub interactions: Vec<Interaction>,
}

impl Cassette {
    /// Reads a cassette from a JSON file.
    pub fn load(path: &Path) -> Result<Self, Error> {
        load_json(path)
    }
}

/// Reads a JSON file as `T`.
pub(crate) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| Error::Parse {
        path: path.to_owned(),
        source,
    })
}

/// One expected request and the response to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    /// What the request must look like.
    pub request: RequestMatch,
    /// What the server replies.
    pub response: Response,
}

impl Interaction {
    /// Answers `method` `path` with `status` and `body` as JSON.
    pub fn json(method: &str, path: &str, status: u16, body: &serde_json::Value) -> Interaction {
        Interaction {
            request: RequestMatch {
                method: method.into(),
                path: path.into(),
            },
            response: Response {
                status,
                headers: [("content-type".to_owned(), "application/json".to_owned())]
                    .into_iter()
                    .collect(),
                chunks: vec![body.to_string()],
                body_base64: None,
                chunk_delay_ms: 0,
            },
        }
    }
}

/// Method and path a request must have. The query string is not compared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestMatch {
    /// HTTP method, such as `POST`.
    pub method: String,
    /// Path without the query string, such as `/v1/messages`.
    pub path: String,
}

/// A response, written chunk by chunk so streaming parsers see the recorded
/// boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    /// HTTP status code; 200 when omitted.
    #[serde(default = "ok")]
    pub status: u16,
    /// Response headers in order. The server chooses the transfer encoding.
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// Body chunks; for SSE, events may span chunk boundaries.
    #[serde(default)]
    pub chunks: Vec<String>,
    /// A binary body, base64-encoded, sent after the chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    /// Pause before each chunk in milliseconds, for streaming and abort tests.
    #[serde(default)]
    pub chunk_delay_ms: u64,
}

fn ok() -> u16 {
    200
}
