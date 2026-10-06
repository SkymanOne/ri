//! The first-run download of the docs for the model, against a mock release
//! server: it installs, replaces another version's copy, keeps a current
//! one without asking, and refuses an archive that fails its checksum.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use sha2::Digest as _;
use yapi_mock::{Cassette, Interaction, MockServer, RequestMatch, Response};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yapi-docs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A docs archive documenting `version`.
fn archive(version: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, text) in [
        (".version", format!("{version}\n")),
        ("index.md", "# yapi\n".to_owned()),
        ("pi/docs/extensions.md", "# Extensions\n".to_owned()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, text.as_bytes())
            .unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn file(path: &str, bytes: &[u8]) -> Interaction {
    Interaction {
        request: RequestMatch {
            method: "GET".into(),
            path: format!("/download/v{VERSION}/{path}"),
        },
        response: Response {
            status: 200,
            headers: IndexMap::new(),
            chunks: Vec::new(),
            body_base64: Some(STANDARD.encode(bytes)),
            chunk_delay_ms: 0,
        },
    }
}

/// A release serving `archive` with the checksum line `sums`.
async fn release(archive: &[u8], sums: &str) -> MockServer {
    MockServer::local(Cassette {
        interactions: vec![
            file("yapi-docs.tar.gz", archive),
            file("yapi-docs.tar.gz.sha256", sums.as_bytes()),
        ],
    })
    .await
    .unwrap()
}

fn sums(archive: &[u8]) -> String {
    let hex: String = sha2::Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{hex}  yapi-docs.tar.gz\n")
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_then_keeps_this_versions_docs() {
    let agent = scratch("install");
    let bytes = archive(VERSION);
    let server = release(&bytes, &sums(&bytes)).await;
    yapi_core::docs::download(&server.url(), &agent)
        .await
        .unwrap();
    server.finish().unwrap();
    assert_eq!(
        std::fs::read_to_string(agent.join("docs/pi/docs/extensions.md")).unwrap(),
        "# Extensions\n"
    );
    assert_eq!(entries(&agent), ["docs"]);

    // This version's docs are kept without a request.
    let idle = MockServer::local(Cassette::default()).await.unwrap();
    yapi_core::docs::download(&idle.url(), &agent)
        .await
        .unwrap();
    assert!(idle.finish().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn replaces_another_versions_docs() {
    let agent = scratch("replace");
    std::fs::create_dir_all(agent.join("docs")).unwrap();
    std::fs::write(agent.join("docs/.version"), "0.0.1\n").unwrap();
    std::fs::write(agent.join("docs/stale.md"), "old").unwrap();
    let bytes = archive(VERSION);
    let server = release(&bytes, &sums(&bytes)).await;
    yapi_core::docs::download(&server.url(), &agent)
        .await
        .unwrap();
    assert_eq!(entries(&agent.join("docs")), [".version", "index.md", "pi"]);
    assert_eq!(entries(&agent), ["docs"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_the_old_docs_when_the_archive_fails_its_checksum() {
    let agent = scratch("refuse");
    std::fs::create_dir_all(agent.join("docs")).unwrap();
    std::fs::write(agent.join("docs/.version"), "0.0.1\n").unwrap();
    let bytes = archive(VERSION);
    let server = release(&bytes, &sums(b"other")).await;
    let err = yapi_core::docs::download(&server.url(), &agent)
        .await
        .unwrap_err();
    assert!(
        err.ends_with("does not match its SHA-256 checksum"),
        "{err}"
    );
    assert_eq!(entries(&agent.join("docs")), [".version"]);
    assert_eq!(entries(&agent), ["docs"]);
}
