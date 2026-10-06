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

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
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
        Command::Bench(args) => bench::run(args).map(|()| ExitCode::SUCCESS),
        Command::E2e(args) => e2e::run_command(args),
        Command::JsRuntime(args) => js_runtime::run(args),
        Command::MockSse(args) => mock_sse::run(args),
        Command::Models(args) => models::run(args).map(|()| ExitCode::SUCCESS),
        Command::PackageRegistrations(args) => package_registrations::run(args),
        Command::VendorPi(args) => vendor_pi::run(args).map(|()| ExitCode::SUCCESS),
    }
}

/// Reads and parses a JSON file.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes `value` as pi's pretty JSON with a trailing newline.
fn write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let json = yapi_types::json::to_string_pretty(value, "  ")?;
    std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))
}
