//! `cargo xtask mock-sse`: serve a cassette for an out-of-process client such as pi.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use ri_mock::{Cassette, MockServer};

/// Serve a cassette with the mock provider server until Ctrl-C or SIGTERM.
///
/// Prints the base URL as the only line on stdout. On exit, writes the received
/// requests if asked and fails when the requests did not match the cassette.
#[derive(clap::Args)]
pub struct Args {
    /// Cassette file to serve.
    #[arg(long)]
    cassette: PathBuf,
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
    let cassette = Cassette::load(&args.cassette)?;
    let interactions = cassette.interactions.len();
    let server = MockServer::start(SocketAddr::from(([127, 0, 0, 1], args.port)), cassette).await?;
    println!("{}", server.url());
    eprintln!(
        "serving {interactions} interaction(s) from {}; stop with Ctrl-C or SIGTERM",
        args.cassette.display()
    );

    shutdown().await.context("waiting for a shutdown signal")?;

    if let Some(path) = &args.requests {
        let json = ri_types::json::to_string_pretty(&server.requests(), "  ")?;
        fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    }
    // An unsatisfied cassette is a result to report, not a crash.
    match server.finish() {
        Ok(requests) => {
            eprintln!("cassette satisfied by {} request(s)", requests.len());
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(ExitCode::FAILURE)
        }
    }
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
