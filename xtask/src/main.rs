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
mod package_registrations;
mod vendor_pi;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Development tasks for yapi")]
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
    PackageRegistrations(package_registrations::Args),
    VendorPi(vendor_pi::Args),
}

fn main() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Command::Bench(args) => bench::run(args),
        Command::E2e(args) => e2e::run_command(args),
        Command::JsRuntime(args) => js_runtime::run(args),
        Command::MockSse(args) => mock_sse::run(args),
        Command::Models(args) => models::run(args).map(|()| ExitCode::SUCCESS),
        Command::PackageRegistrations(args) => package_registrations::run(args),
        Command::VendorPi(args) => vendor_pi::run(args).map(|()| ExitCode::SUCCESS),
    }
}
