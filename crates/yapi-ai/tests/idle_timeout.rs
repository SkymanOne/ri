//! `httpIdleTimeoutMs`: provider requests fail when headers or body chunks
//! stop arriving, as undici's `headersTimeout` and `bodyTimeout` make pi's
//! requests fail. The timeout is process-wide, so these tests have their
//! own binary.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use yapi_ai::http::{self, Failure};
use yapi_ai::stream::StreamOptions;

/// A server that reads one request, writes `head` and then stays silent.
async fn silent_server(head: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        socket.write_all(head.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    format!("http://{address}/")
}

#[tokio::test]
async fn idle_requests_fail_like_pi() {
    http::set_idle_timeout_ms(200);
    let options = StreamOptions::default();

    // No headers: the fetch fails, which the SDKs report as a connection error.
    let url = silent_server("").await;
    let started = std::time::Instant::now();
    let result = http::send(|| http::client().get(&url), &options).await;
    assert!(
        matches!(&result, Err(Failure::Connection(message)) if message == "Connection error."),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // Headers, then a body that goes quiet: the read fails with undici's
    // `terminated`.
    let url = silent_server(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
    )
    .await;
    let mut response = http::send(|| http::client().get(&url), &options)
        .await
        .unwrap();
    let cancel = options.cancel.clone();
    assert_eq!(
        http::read_chunk(&mut response, &cancel, "aborted").await,
        Ok(Some(b"hello".to_vec()))
    );
    assert_eq!(
        http::read_chunk(&mut response, &cancel, "aborted").await,
        Err("terminated".to_owned())
    );
}
