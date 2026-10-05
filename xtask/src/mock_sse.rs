//! `cargo xtask mock-sse`: serve or record a cassette for an out-of-process client
//! such as pi.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use yapi_mock::{Cassette, MockServer};

/// Serve a cassette with the mock provider server, or record one through a proxy to
/// a real provider, until Ctrl-C or SIGTERM.
///
/// Prints the base URL as the only line on stdout. On exit, writes the received
/// requests and the recording if asked, and fails when requests did not match the
/// cassette or the upstream failed.
#[derive(clap::Args)]
pub struct Args {
    /// Cassette file to serve.
    #[arg(long, required_unless_present = "record", conflicts_with = "record")]
    cassette: Option<PathBuf>,
    /// Record instead: forward requests to this base URL, such as
    /// https://api.anthropic.com. Credentials are never written to disk.
    #[arg(long, requires = "out")]
    record: Option<String>,
    /// Where to write the recorded cassette on exit.
    #[arg(long, requires = "record")]
    out: Option<PathBuf>,
    /// Port on 127.0.0.1; 0 picks a free port.
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Write the received requests to this file as JSON on exit.
    #[arg(long)]
    requests: Option<PathBuf>,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(args))
}

async fn serve(args: Args) -> anyhow::Result<ExitCode> {
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let server = match (&args.cassette, &args.record) {
        (Some(path), _) => {
            let cassette = Cassette::load(path)?;
            eprintln!(
                "serving {} interaction(s) from {}",
                cassette.interactions.len(),
                path.display()
            );
            MockServer::start(addr, cassette).await?
        }
        (None, Some(upstream)) => {
            eprintln!("recording requests forwarded to {upstream}");
            MockServer::record(addr, upstream).await?
        }
        (None, None) => unreachable!("clap requires --cassette or --record"),
    };
    println!("{}", server.url());
    eprintln!("stop with Ctrl-C or SIGTERM");

    shutdown().await.context("waiting for a shutdown signal")?;

    if let Some(path) = &args.requests {
        write_json(path, &server.requests())?;
    }
    if let Some(path) = &args.out {
        let cassette = server.recording();
        write_json(path, &cassette)?;
        eprintln!(
            "recorded {} interaction(s) to {}",
            cassette.interactions.len(),
            path.display()
        );
    }
    // An unsatisfied cassette is a result to report, not a crash.
    match server.finish() {
        Ok(requests) => {
            eprintln!("done: {} request(s)", requests.len());
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn write_json(path: &std::path::Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let json = yapi_types::json::to_string_pretty(value, "  ")?;
    fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))
}

async fn shutdown() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
