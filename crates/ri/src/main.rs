#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stderr,
    reason = "the binary reports errors and warnings on stderr; stdout carries mode output"
)]

mod args;
mod help;
mod list_models;
mod modes;
mod startup;

use std::io::Write;
use std::process::ExitCode;

use args::Mode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut parsed = args::parse(&raw);
    if parsed.version {
        let _ = writeln!(std::io::stdout(), "{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut failed = false;
    for diagnostic in &parsed.diagnostics {
        if diagnostic.error {
            eprintln!("Error: {}", diagnostic.message);
            failed = true;
        } else {
            eprintln!("Warning: {}", diagnostic.message);
        }
    }
    if failed {
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    ExitCode::from(runtime.block_on(run(&mut parsed)))
}

async fn run(parsed: &mut args::Args) -> u8 {
    use std::io::IsTerminal;
    if parsed.help {
        help::print();
        return 0;
    }
    if let Some(pattern) = &parsed.list_models {
        return list_models::run(pattern.as_deref());
    }
    let interactive = parsed.mode.is_none()
        && !parsed.print
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if interactive {
        eprintln!("ri: interactive mode is not implemented yet; use -p or --mode json");
        return 1;
    }
    if parsed.mode == Some(Mode::Rpc) {
        eprintln!("ri: rpc mode is not implemented yet");
        return 1;
    }
    let stdin = startup::read_piped_stdin();
    let startup = match startup::start(parsed, stdin) {
        Ok(startup) => startup,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    modes::print::run(startup, parsed.mode == Some(Mode::Json)).await
}
