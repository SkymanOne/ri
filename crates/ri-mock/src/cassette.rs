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
        let text = fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| Error::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// One expected request and the response to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    /// What the request must look like.
    pub request: RequestMatch,
    /// What the server replies.
    pub response: Response,
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
    /// Pause before each chunk in milliseconds, for streaming and abort tests.
    #[serde(default)]
    pub chunk_delay_ms: u64,
}

fn ok() -> u16 {
    200
}
