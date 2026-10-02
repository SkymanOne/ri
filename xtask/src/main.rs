#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "command-line tool: results on stdout, status on stderr"
)]

mod bench;
mod e2e;
mod js_runtime;
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
    Bench(bench::Args),
    E2e(e2e::Args),
    JsRuntime(js_runtime::Args),
    MockSse(mock_sse::Args),
    Models(models::Args),
}

fn main() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Command::Bench(args) => bench::run(args),
        Command::E2e(args) => e2e::run_command(args),
        Command::JsRuntime(args) => js_runtime::run(args),
        Command::MockSse(args) => mock_sse::run(args),
        Command::Models(args) => models::run(args).map(|()| ExitCode::SUCCESS),
    }
}
