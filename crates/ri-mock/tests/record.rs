//! Recording through the proxy, with a replaying server standing in for the provider.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::net::SocketAddr;

use ri_mock::{Cassette, Interaction, MockServer, REDACTED, RequestMatch, Response};

fn any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn provider_cassette() -> Cassette {
    let mut headers = indexmap::IndexMap::new();
    headers.insert("content-type".to_owned(), "text/event-stream".to_owned());
    headers.insert("request-id".to_owned(), "req_1".to_owned());
    Cassette {
        interactions: vec![Interaction {
            request: RequestMatch {
                method: "POST".into(),
                path: "/v1/messages".into(),
            },
            response: Response {
                status: 200,
                headers,
                chunks: vec![
                    "event: a\ndata: {}\n\n".into(),
                    "event: b\ndata: 🦀\n\n".into(),
                ],
                body_base64: None,
                chunk_delay_ms: 0,
            },
        }],
    }
}

#[tokio::test]
async fn records_what_the_upstream_sends() {
    let provider = MockServer::start(any_port(), provider_cassette())
        .await
        .unwrap();
    let recorder = MockServer::record(any_port(), &provider.url())
        .await
        .unwrap();

    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/messages?key=secret&beta=true",
            recorder.url()
        ))
        .header("x-api-key", "secret")
        .header("authorization", "Bearer secret")
        .body(r#"{"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["request-id"], "req_1");
    let body = response.text().await.unwrap();
    assert_eq!(body, "event: a\ndata: {}\n\nevent: b\ndata: 🦀\n\n");

    // The provider got the request, credentials included (redacted only in records).
    let upstream = &provider.finish().unwrap()[0];
    assert_eq!(upstream.headers["x-api-key"], REDACTED);
    assert_eq!(upstream.body, r#"{"stream":true}"#);
    assert_eq!(upstream.query.as_deref(), Some("key=<redacted>&beta=true"));

    let recorded = &recorder.finish().unwrap()[0];
    assert_eq!(recorded.headers["authorization"], REDACTED);

    // The interaction is stored once its stream has ended.
    let cassette = loop {
        let cassette = recorder.recording();
        if !cassette.interactions.is_empty() {
            break cassette;
        }
        tokio::task::yield_now().await;
    };
    let interaction = &cassette.interactions[0];
    assert_eq!(interaction.request.path, "/v1/messages");
    assert_eq!(interaction.response.status, 200);
    assert_eq!(
        interaction.response.headers["content-type"],
        "text/event-stream"
    );
    assert_eq!(interaction.response.chunks.concat(), body);

    // The recording replays to the same bytes.
    let replay = MockServer::start(any_port(), cassette).await.unwrap();
    let again = reqwest::Client::new()
        .post(format!("{}/v1/messages", replay.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(again.text().await.unwrap(), body);
    replay.finish().unwrap();
}

#[tokio::test]
async fn unreachable_upstream_is_a_problem() {
    // Port 9 (discard) is closed on loopback.
    let recorder = MockServer::record(any_port(), "http://127.0.0.1:9")
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", recorder.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert!(recorder.finish().is_err());
}
