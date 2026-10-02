#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "command-line tool: results on stdout, status on stderr"
)]

mod mock_sse;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Development tasks for ri")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    MockSse(mock_sse::Args),
}

fn main() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Command::MockSse(args) => mock_sse::run(args),
    }
}
