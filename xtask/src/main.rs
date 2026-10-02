#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "command-line tool: results on stdout, status on stderr"
)]

mod e2e;
mod mock_sse;
mod models;

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
    E2e(e2e::Args),
    MockSse(mock_sse::Args),
    Models(models::Args),
}

fn main() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Command::E2e(args) => e2e::run_command(args),
        Command::MockSse(args) => mock_sse::run(args),
        Command::Models(args) => models::run(args).map(|()| ExitCode::SUCCESS),
    }
}
