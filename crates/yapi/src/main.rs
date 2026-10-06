#![doc = env!("CARGO_PKG_DESCRIPTION")]
#![forbid(unsafe_code)]
#![allow(
    clippy::print_stderr,
    reason = "the binary reports errors and warnings on stderr; stdout carries mode output"
)]

mod args;
mod auth_command;
mod config_command;
mod export_html;
mod help;
mod import;
mod interactive;
mod list_models;
mod mcp_command;
mod modes;
mod new_command;
mod packages;
mod runtime;
mod startup;

use std::io::Write;
use std::process::ExitCode;

use args::Mode;

/// Writes `line` to stdout; a closed stdout is not an error.
pub(crate) fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// Writes `line` to stderr; a closed stderr is not an error.
pub(crate) fn err(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Parses the arguments of `name`, a command pi does not have, with clap.
/// On `--help` or an error it prints the help or the error and returns the
/// exit code: 0 for help, 1 for an error, as yapi's other errors.
pub(crate) fn parse_command<T: clap::Parser>(name: &str, args: &[String]) -> Result<T, u8> {
    T::try_parse_from(std::iter::once(name).chain(args.iter().map(String::as_str))).map_err(
        |error| {
            let _ = error.print();
            u8::from(error.use_stderr())
        },
    )
}

/// pi's `applyHttpProxySettings`: the global `httpProxy` setting stands in for
/// unset proxy variables.
fn apply_http_proxy() {
    let path = yapi_core::config::agent_dir().join("settings.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let proxy = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|settings| settings.get("httpProxy")?.as_str().map(str::to_owned));
    yapi_ai::http::set_settings_proxy(proxy.as_deref());
}

/// Runs `future` to completion on a current-thread runtime; its exit code.
fn block_on(future: impl std::future::Future<Output = u8>) -> ExitCode {
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
    let code = runtime.block_on(future);
    // A stdin read still in progress (RPC mode after a signal) runs on a
    // blocking thread that only returns at end of input; exit without it.
    runtime.shutdown_background();
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match raw.first().map(String::as_str) {
        Some("import") => return ExitCode::from(import::run(&raw[1..])),
        Some("new") => return ExitCode::from(new_command::run(&raw[1..])),
        Some("config") => return ExitCode::from(config_command::run(&raw[1..])),
        Some("mcp") => {
            apply_http_proxy();
            return block_on(mcp_command::run(&raw[1..]));
        }
        Some("auth") => return block_on(auth_command::run(&raw)),
        Some("install" | "remove" | "uninstall" | "update" | "list") => {
            apply_http_proxy();
            return block_on(async { packages::run(&raw).await.unwrap_or(0) });
        }
        _ => {}
    }
    let mut parsed = args::parse(&raw);
    if parsed.version {
        let _ = writeln!(std::io::stdout(), "{}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    apply_http_proxy();
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
    if let Some(input) = parsed.export.clone() {
        return match export_html::export_file(&input, parsed.messages.first().map(String::as_str)) {
            Ok(path) => {
                let _ = writeln!(std::io::stdout(), "Exported to: {}", path.display());
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("Error: {message}");
                ExitCode::FAILURE
            }
        };
    }
    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        eprintln!("Error: @file arguments are not supported in RPC mode");
        return ExitCode::FAILURE;
    }
    if let Err(message) = validate_session_flags(&parsed) {
        eprintln!("Error: {message}");
        return ExitCode::FAILURE;
    }
    block_on(run(&mut parsed))
}

/// The message of a panic in the interactive session, kept until the
/// terminal is restored.
static PANIC_MESSAGE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Whether model catalogs may be fetched: pi's model runtime fetches only
/// while `PI_OFFLINE` is unset, which `--offline` sets.
fn model_network(args: &args::Args) -> bool {
    !args.offline && std::env::var_os("PI_OFFLINE").is_none()
}

/// Runs the interactive session so that a panic leaves the terminal usable,
/// as pi's crash handler does: the terminal modes are reset and the message
/// printed after them. Raw mode is restored as the session unwinds.
async fn survive_crash(run: impl std::future::Future<Output = u8>) -> u8 {
    use futures_util::FutureExt;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut slot) = PANIC_MESSAGE.lock() {
            *slot = Some(info.to_string());
        }
    }));
    let outcome = std::panic::AssertUnwindSafe(run).catch_unwind().await;
    std::panic::set_hook(previous);
    match outcome {
        Ok(code) => code,
        Err(_) => {
            use yapi_tui::terminal::{
                BRACKETED_PASTE_DISABLE, KITTY_DISABLE, MODIFY_OTHER_KEYS_DISABLE,
            };
            let _ = write!(
                std::io::stdout(),
                "{}{BRACKETED_PASTE_DISABLE}{KITTY_DISABLE}{MODIFY_OTHER_KEYS_DISABLE}\x1b[?25h\r\n",
                yapi_tui::screen::ALT_SCREEN_LEAVE
            );
            let _ = std::io::stdout().flush();
            let message = PANIC_MESSAGE
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .unwrap_or_else(|| "unknown panic".to_owned());
            eprintln!("yapi crashed: {message}");
            eprintln!("To resume, run `yapi -c`.");
            1
        }
    }
}

/// pi's `validateForkFlags` and `validateSessionIdFlags`.
fn validate_session_flags(parsed: &args::Args) -> Result<(), String> {
    let conflicts = |flags: &[(bool, &str)]| -> Vec<String> {
        flags
            .iter()
            .filter(|(set, _)| *set)
            .map(|(_, flag)| (*flag).to_owned())
            .collect()
    };
    if parsed.fork.is_some() {
        let found = conflicts(&[
            (parsed.session.is_some(), "--session"),
            (parsed.continue_, "--continue"),
            (parsed.resume, "--resume"),
            (parsed.no_session, "--no-session"),
        ]);
        if !found.is_empty() {
            return Err(format!(
                "--fork cannot be combined with {}",
                found.join(", ")
            ));
        }
    }
    if let Some(id) = &parsed.session_id {
        let found = conflicts(&[
            (parsed.session.is_some(), "--session"),
            (parsed.continue_, "--continue"),
            (parsed.resume, "--resume"),
        ]);
        if !found.is_empty() {
            return Err(format!(
                "--session-id cannot be combined with {}",
                found.join(", ")
            ));
        }
        yapi_core::session::validate_session_id(id).map_err(|err| err.to_string())?;
    }
    Ok(())
}

async fn run(parsed: &mut args::Args) -> u8 {
    use std::io::IsTerminal;
    // Once print or a mode owns stdout, pi sends help and the model list to
    // stderr.
    let metadata_to_stderr = parsed.print || parsed.mode.is_some();
    if let Some(pattern) = &parsed.list_models {
        return list_models::run(pattern.as_deref(), metadata_to_stderr);
    }
    let interactive = !parsed.help
        && parsed.mode.is_none()
        && !parsed.print
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    // pi asks before anything project-local loads, extensions included.
    if interactive && let Err(err) = ask_project_trust(parsed) {
        eprintln!("{err}");
        return 1;
    }
    let loaded = startup::load_extensions(parsed).await;
    // pi's help lists the flags extensions register.
    if parsed.help {
        let flags: Vec<yapi_ext::Flag> = loaded
            .as_ref()
            .map(|(extensions, _)| {
                extensions
                    .hosts
                    .iter()
                    .flat_map(|host| host.flags())
                    .collect()
            })
            .unwrap_or_default();
        help::print(&flags, metadata_to_stderr);
        return 0;
    }
    let (extensions, run_settings) = match loaded {
        Ok(loaded) => loaded,
        Err(errors) => {
            for message in &errors.messages {
                eprintln!("Error: {message}");
            }
            if errors.load_failed {
                eprintln!("{}", startup::EXTENSION_LOAD_FAILURE_HINT);
            }
            return 1;
        }
    };
    interactive::keybindings::migrate_file(&yapi_core::config::agent_dir());
    // pi picks the session for `--resume` in print and JSON mode too; yapi
    // needs a terminal for the picker.
    if parsed.resume
        && parsed.mode != Some(Mode::Rpc)
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        match pick_session(parsed, &run_settings) {
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
    if interactive {
        let tui_mode = match parsed.tui_mode.as_deref() {
            Some("regular") => Some(yapi_types::settings::TuiMode::Regular),
            Some(_) => Some(yapi_types::settings::TuiMode::Fullscreen),
            None => None,
        };
        let startup = match start(parsed, None, &extensions, true, run_settings) {
            Ok(startup) => startup,
            Err(code) => return code,
        };
        // pi attaches `@` images to the initial message only.
        let initial_images = match startup.initial_message {
            Some(_) => startup.initial_images,
            None => Vec::new(),
        };
        let mut initial: Vec<String> = startup.initial_message.into_iter().collect();
        initial.extend(startup.messages);
        let run = interactive::run(
            startup.session,
            yapi_core::config::agent_dir(),
            interactive::Options {
                tui_mode,
                verbose: parsed.verbose,
                initial,
                initial_images,
                factory: startup::factory(parsed, extensions),
                use_theme: parsed.use_theme.clone(),
                model_fallback: startup.model_fallback,
                model_network: model_network(parsed),
            },
        );
        return survive_crash(run).await;
    }
    let rpc = parsed.mode == Some(Mode::Rpc);
    // RPC commands arrive on stdin.
    let stdin = if rpc {
        None
    } else {
        startup::read_piped_stdin()
    };
    let startup = match start(parsed, stdin, &extensions, false, run_settings) {
        Ok(startup) => startup,
        Err(code) => return code,
    };
    for error in startup.session.settings_errors() {
        eprintln!("Warning: {error}");
    }
    if rpc {
        // pi refreshes model catalogs in the background for RPC.
        if model_network(parsed) {
            let session = startup.session.clone();
            tokio::spawn(async move {
                use yapi_ai::model_catalog::{REFRESH_TIMEOUT, RefreshOptions, cancel_after};
                let options = RefreshOptions {
                    cancel: cancel_after(REFRESH_TIMEOUT),
                    ..Default::default()
                };
                session.refresh_model_catalogs(options).await;
            });
        }
        // pi's RPC mode starts without a model, on a placeholder; prompts
        // then fail with the missing-key message.
        return modes::rpc::run(startup.session, startup::factory(parsed, extensions)).await;
    }
    // pi's print mode kills running commands and exits with the signal's
    // code on SIGTERM and SIGHUP. The run stays alive until they are killed:
    // dropping it would stop tracking its commands without killing them.
    let print = modes::print::run(startup, parsed.mode == Some(Mode::Json));
    tokio::pin!(print);
    tokio::select! {
        code = &mut print => code,
        code = modes::rpc::termination() => {
            yapi_core::tools::bash::kill_tracked_children();
            code
        }
    }
}

/// [`startup::start`], or the exit code once its error is printed; a
/// cancelled prompt exits quietly.
fn start(
    parsed: &mut args::Args,
    stdin: Option<String>,
    extensions: &startup::Extensions,
    interactive: bool,
    run_settings: startup::RunSettings,
) -> Result<startup::Startup, u8> {
    startup::start(parsed, stdin, extensions, interactive, run_settings).map_err(|err| {
        if err.is::<startup::Cancelled>() {
            return 0;
        }
        eprintln!("{err}");
        1
    })
}

/// Asks whether to trust the working directory when its project resources
/// need trust and nothing decides it, and applies the answer to this run. A
/// cancelled prompt leaves the project untrusted.
fn ask_project_trust(parsed: &mut args::Args) -> anyhow::Result<()> {
    let agent_dir = yapi_core::config::agent_dir();
    let cwd = std::env::current_dir()?;
    let settings = yapi_core::settings::SettingsManager::load(&agent_dir, &cwd, false)?;
    let view = settings.settings();
    let store = yapi_core::trust::TrustStore::new(&agent_dir);
    if !yapi_core::trust::needs_prompt(
        &cwd,
        &store,
        parsed.project_trust_override,
        view.default_project_trust,
    ) {
        return Ok(());
    }
    let theme = parsed.use_theme.clone().or(view.theme.clone());
    let trusted = interactive::picker::ask_project_trust(&agent_dir, &cwd, theme.as_deref())?;
    parsed.project_trust_override = Some(trusted.unwrap_or(false));
    Ok(())
}

/// pi's `--resume` picker over this project's sessions, then all of them.
fn pick_session(
    parsed: &args::Args,
    run_settings: &startup::RunSettings,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let agent_dir = yapi_core::config::agent_dir();
    let cwd = &run_settings.cwd;
    let (custom, theme) = startup::resume_context(parsed, run_settings);
    let default_dir = yapi_core::config::default_session_dir(&agent_dir, cwd);
    let custom = custom.filter(|dir| *dir != default_dir);
    let sources = interactive::session_sources(&agent_dir, cwd, custom);
    Ok(interactive::picker::pick_session(
        &agent_dir,
        sources,
        parsed.use_theme.as_deref().or(theme.as_deref()),
    )?)
}
