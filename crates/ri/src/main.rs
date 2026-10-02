#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]

use std::process::ExitCode;

#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "placeholder CLI until argument parsing lands in M1"
)]
fn main() -> ExitCode {
    // Same flags and output as pi: the bare version number.
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-v")) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!("ri: not implemented yet; only --version is available");
    ExitCode::FAILURE
}
