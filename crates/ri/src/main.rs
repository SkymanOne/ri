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
mod packages;
mod runtime;
mod startup;

use std::io::Write;
use std::process::ExitCode;

use args::Mode;

/// pi's `applyHttpProxySettings`: the global `httpProxy` setting stands in for
/// unset proxy variables.
fn apply_http_proxy() {
    let path = ri_core::config::agent_dir().join("settings.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let proxy = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|settings| settings.get("httpProxy")?.as_str().map(str::to_owned));
    ri_ai::http::set_settings_proxy(proxy.as_deref());
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("import") {
        return ExitCode::from(import::run(&raw[1..]));
    }
    if raw.first().map(String::as_str) == Some("config") {
        return ExitCode::from(config_command::run(&raw[1..]));
    }
    if raw.first().map(String::as_str) == Some("mcp") {
        apply_http_proxy();
        return match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => ExitCode::from(runtime.block_on(mcp_command::run(&raw[1..]))),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        };
    }
    if raw.first().map(String::as_str) == Some("auth") {
        return match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => ExitCode::from(runtime.block_on(auth_command::run(&raw))),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        };
    }
    if matches!(
        raw.first().map(String::as_str),
        Some("install" | "remove" | "uninstall" | "update" | "list")
    ) {
        apply_http_proxy();
        return match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => ExitCode::from(runtime.block_on(packages::run(&raw)).unwrap_or(0)),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        };
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
    let code = runtime.block_on(run(&mut parsed));
    // A stdin read still in progress (RPC mode after a signal) runs on a
    // blocking thread that only returns at end of input; exit without it.
    runtime.shutdown_background();
    ExitCode::from(code)
}

/// The message of a panic in the interactive session, kept until the
/// terminal is restored.
static PANIC_MESSAGE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Runs the interactive session so that a panic leaves the terminal usable,
/// as pi's crash handler does: the terminal modes are reset and the message
/// printed after them. Raw mode is restored as the session unwinds.
/// Whether model catalogs may be fetched: pi's model runtime fetches only
/// while `PI_OFFLINE` is unset, which `--offline` sets.
fn model_network(args: &args::Args) -> bool {
    !args.offline && std::env::var_os("PI_OFFLINE").is_none()
}

async fn survive_crash(run: impl std::future::Future<Output = u8>) -> u8 {
    use std::task::Poll;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut slot) = PANIC_MESSAGE.lock() {
            *slot = Some(info.to_string());
        }
    }));
    let mut run = Box::pin(run);
    let outcome = std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.as_mut().poll(cx))) {
            Ok(Poll::Ready(code)) => Poll::Ready(Ok(code)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await;
    std::panic::set_hook(previous);
    match outcome {
        Ok(code) => code,
        Err(_) => {
            use ri_tui::terminal::{
                BRACKETED_PASTE_DISABLE, KITTY_DISABLE, MODIFY_OTHER_KEYS_DISABLE,
            };
            let _ = write!(
                std::io::stdout(),
                "{}{BRACKETED_PASTE_DISABLE}{KITTY_DISABLE}{MODIFY_OTHER_KEYS_DISABLE}\x1b[?25h\r\n",
                ri_tui::screen::ALT_SCREEN_LEAVE
            );
            let _ = std::io::stdout().flush();
            let message = PANIC_MESSAGE
                .lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .unwrap_or_else(|| "unknown panic".to_owned());
            eprintln!("ri crashed: {message}");
            eprintln!("To resume, run `ri -c`.");
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
        ri_core::session::validate_session_id(id).map_err(|err| err.to_string())?;
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
        let flags: Vec<ri_ext::Flag> = loaded
            .as_ref()
            .map(|extensions| {
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
    let extensions = match loaded {
        Ok(extensions) => extensions,
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
    interactive::keybindings::migrate_file(&ri_core::config::agent_dir());
    // pi picks the session for `--resume` in print and JSON mode too; ri
    // needs a terminal for the picker.
    if parsed.resume
        && parsed.mode != Some(Mode::Rpc)
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
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
    if interactive {
        let tui_mode = match parsed.tui_mode.as_deref() {
            Some("regular") => Some(ri_types::settings::TuiMode::Regular),
            Some(_) => Some(ri_types::settings::TuiMode::Fullscreen),
            None => None,
        };
        let startup = match startup::start(parsed, None, &extensions, true) {
            Ok(startup) => startup,
            Err(err) if err.is::<startup::Cancelled>() => return 0,
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        };
        let mut initial: Vec<String> = startup.initial_message.into_iter().collect();
        initial.extend(startup.messages);
        let args = parsed.clone();
        let run = interactive::run(
            startup.session,
            ri_core::config::agent_dir(),
            interactive::Options {
                tui_mode,
                verbose: parsed.verbose,
                initial,
                factory: Box::new(move |session| {
                    startup::create(&args, session, false, &extensions)
                }),
                use_theme: parsed.use_theme.clone(),
                model_fallback: startup.model_fallback,
                model_network: model_network(parsed),
            },
        );
        return survive_crash(run).await;
    }
    if parsed.mode == Some(Mode::Rpc) {
        let startup = match startup::start(parsed, None, &extensions, false) {
            Ok(startup) => startup,
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        };
        for error in startup.session.settings_errors() {
            eprintln!("Warning: {error}");
        }
        // pi refreshes model catalogs in the background for RPC.
        if model_network(parsed) {
            let session = startup.session.clone();
            tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                let options = ri_ai::model_catalog::RefreshOptions {
                    cancel: cancel.clone(),
                    ..Default::default()
                };
                let timer = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    cancel.cancel();
                });
                session.refresh_model_catalogs(options).await;
                timer.abort();
            });
        }
        // pi's RPC mode starts without a model, on a placeholder; prompts
        // then fail with the missing-key message.
        let args = parsed.clone();
        return modes::rpc::run(
            startup.session,
            Box::new(move |session| startup::create(&args, session, false, &extensions)),
        )
        .await;
    }
    let stdin = startup::read_piped_stdin();
    let startup = match startup::start(parsed, stdin, &extensions, false) {
        Ok(startup) => startup,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    for error in startup.session.settings_errors() {
        eprintln!("Warning: {error}");
    }
    // pi's print mode kills running commands and exits with the signal's
    // code on SIGTERM and SIGHUP. The run stays alive until they are killed:
    // dropping it would stop tracking its commands without killing them.
    let print = modes::print::run(startup, parsed.mode == Some(Mode::Json));
    tokio::pin!(print);
    tokio::select! {
        code = &mut print => code,
        code = modes::rpc::termination() => {
            ri_core::tools::bash::kill_tracked_children();
            code
        }
    }
}

/// pi's `--resume` picker over this project's sessions, then all of them.
/// Asks whether to trust the working directory when its project resources
/// need trust and nothing decides it, and applies the answer to this run. A
/// cancelled prompt leaves the project untrusted.
fn ask_project_trust(parsed: &mut args::Args) -> anyhow::Result<()> {
    let agent_dir = ri_core::config::agent_dir();
    let cwd = std::env::current_dir()?;
    let settings = ri_core::settings::SettingsManager::load(&agent_dir, &cwd, false)?;
    let view = settings.settings();
    let store = ri_core::trust::TrustStore::new(&agent_dir);
    if !ri_core::trust::needs_prompt(
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

fn pick_session(parsed: &args::Args) -> anyhow::Result<Option<std::path::PathBuf>> {
    let agent_dir = ri_core::config::agent_dir();
    let (cwd, custom, theme) = startup::resume_context(parsed)?;
    let default_dir = ri_core::config::default_session_dir(&agent_dir, &cwd);
    let custom = custom.filter(|dir| *dir != default_dir);
    let sources = interactive::session_sources(&agent_dir, &cwd, custom);
    Ok(interactive::picker::pick_session(
        &agent_dir,
        sources,
        parsed.use_theme.as_deref().or(theme.as_deref()),
    )?)
}
