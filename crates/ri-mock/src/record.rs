//! Recording proxy: forwards requests to a real provider and records the responses.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::Channel;
use indexmap::IndexMap;

use crate::{Cassette, Error, Interaction, Mode, RequestMatch, Response, State, lock};

/// Request headers not forwarded: hop-by-hop headers, and ones the client sets itself.
const SKIPPED_REQUEST_HEADERS: [&str; 9] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "upgrade",
    "accept-encoding",
];

/// Response headers left out of recordings: transport details, cookies and
/// per-request noise that replay must not repeat.
const DROPPED_RESPONSE_HEADERS: [&str; 14] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "content-encoding",
    "set-cookie",
    "date",
    "server",
    "via",
    "alt-svc",
    "nel",
    "report-to",
    "server-timing",
    "cf-ray",
];

#[derive(Debug)]
pub(crate) struct Recorder {
    upstream: Upstream,
    recorded: Vec<Interaction>,
}

/// Where to forward requests; cheap to clone out of the server state.
#[derive(Clone, Debug)]
pub(crate) struct Upstream {
    base: Arc<str>,
    client: reqwest::Client,
}

impl Recorder {
    pub(crate) fn new(upstream: &str) -> Result<Self, Error> {
        Ok(Self {
            upstream: Upstream {
                base: upstream.trim_end_matches('/').into(),
                client: reqwest::Client::builder().build()?,
            },
            recorded: Vec::new(),
        })
    }

    pub(crate) fn upstream(&self) -> Upstream {
        self.upstream.clone()
    }

    pub(crate) fn cassette(&self) -> Cassette {
        Cassette {
            interactions: self.recorded.clone(),
        }
    }
}

impl Upstream {
    /// Sends the request upstream and streams the response back while recording it.
    pub(crate) async fn forward(
        self,
        state: Arc<Mutex<State>>,
        parts: hyper::http::request::Parts,
        body: Bytes,
    ) -> Result<hyper::Response<Channel<Bytes>>, String> {
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let label = format!("{} {}", parts.method, parts.uri.path());
        let mut request = self
            .client
            .request(parts.method.clone(), format!("{}{target}", self.base))
            .body(body);
        for (name, value) in &parts.headers {
            if !SKIPPED_REQUEST_HEADERS.contains(&name.as_str()) {
                request = request.header(name, value);
            }
        }
        // Uncompressed streams keep the recorded chunks readable.
        let mut upstream = request
            .header(hyper::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|err| format!("{label}: upstream request failed: {err}"))?;

        let status = upstream.status().as_u16();
        let mut builder = hyper::Response::builder().status(status);
        let mut headers = IndexMap::new();
        for (name, value) in upstream.headers() {
            if DROPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
                continue;
            }
            builder = builder.header(name, value);
            if let Ok(value) = value.to_str() {
                headers.insert(name.as_str().to_owned(), value.to_owned());
            }
        }
        let (mut sender, channel) = Channel::new(1);
        let response = builder.body(channel).map_err(|err| err.to_string())?;
        let request = RequestMatch {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
        };

        tokio::spawn(async move {
            let mut chunks = Vec::new();
            let mut partial = Vec::new();
            loop {
                match upstream.chunk().await {
                    Ok(Some(bytes)) => {
                        partial.extend_from_slice(&bytes);
                        let text = take_utf8(&mut partial);
                        if !text.is_empty() {
                            chunks.push(text);
                        }
                        // A client that stops reading ends the recording there, as an
                        // aborting client would.
                        if sender.send_data(bytes).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        lock(&state)
                            .problems
                            .push(format!("{label}: upstream stream failed: {err}"));
                        break;
                    }
                }
            }
            if !partial.is_empty() {
                chunks.push(String::from_utf8_lossy(&partial).into_owned());
            }
            let interaction = Interaction {
                request,
                response: Response {
                    status,
                    headers,
                    chunks,
                    body_base64: None,
                    chunk_delay_ms: 0,
                },
            };
            if let Mode::Record(recorder) = &mut lock(&state).mode {
                recorder.recorded.push(interaction);
            }
        });
        Ok(response)
    }
}

/// Removes and returns the longest valid UTF-8 prefix, keeping an incomplete
/// trailing character for the next chunk. Invalid bytes are replaced.
fn take_utf8(buffer: &mut Vec<u8>) -> String {
    let complete = match std::str::from_utf8(buffer) {
        Ok(_) => buffer.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(_) => buffer.len(),
    };
    let rest = buffer.split_off(complete);
    let text = String::from_utf8_lossy(buffer).into_owned();
    *buffer = rest;
    text
}

#[cfg(test)]
mod tests {
    use super::take_utf8;

    #[test]
    fn keeps_split_characters_for_the_next_chunk() {
        let crab = "🦀".as_bytes();
        let mut buffer = [b"ab".as_slice(), &crab[..2]].concat();
        assert_eq!(take_utf8(&mut buffer), "ab");
        buffer.extend_from_slice(&crab[2..]);
        assert_eq!(take_utf8(&mut buffer), "🦀");
        assert!(buffer.is_empty());
    }
}
