#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stderr,
    reason = "the binary reports errors and warnings on stderr; stdout carries mode output"
)]

mod args;
mod help;
mod interactive;
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
    interactive::keybindings::migrate_file(&ri_core::config::agent_dir());
    if interactive {
        let tui_mode = match parsed.tui_mode.as_deref() {
            Some("regular") => Some(ri_types::settings::TuiMode::Regular),
            Some(_) => Some(ri_types::settings::TuiMode::Fullscreen),
            None => None,
        };
        if parsed.resume {
            match pick_session(parsed) {
                Ok(Some(path)) => {
                    parsed.session = Some(path.display().to_string());
                    parsed.resume = false;
                }
                Ok(None) => {
                    let _ = writeln!(std::io::stdout(), "\x1b[2mNo session selected\x1b[22m");
                    return 0;
                }
                Err(err) => {
                    eprintln!("{err}");
                    return 1;
                }
            }
        }
        let startup = match startup::start(parsed, None) {
            Ok(startup) => startup,
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        };
        let mut initial: Vec<String> = startup.initial_message.into_iter().collect();
        initial.extend(startup.messages);
        let args = parsed.clone();
        return interactive::run(
            startup.session,
            ri_core::config::agent_dir(),
            interactive::Options {
                tui_mode,
                verbose: parsed.verbose,
                initial,
                factory: Box::new(move |session| startup::create(&args, session, false)),
            },
        )
        .await;
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

/// pi's `--resume` picker over this project's sessions, then all of them.
fn pick_session(parsed: &args::Args) -> anyhow::Result<Option<std::path::PathBuf>> {
    let agent_dir = ri_core::config::agent_dir();
    let (cwd, custom, theme) = startup::resume_context(parsed)?;
    let default_dir = ri_core::config::default_session_dir(&agent_dir, &cwd);
    let custom = custom.filter(|dir| *dir != default_dir);
    let sources = interactive::session_sources(&agent_dir, &cwd, custom);
    Ok(interactive::picker::pick_session(
        &agent_dir,
        sources,
        theme.as_deref(),
    )?)
}
