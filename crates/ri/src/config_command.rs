//! `ri config`: the resource configuration selector, globally or for the
//! project (`handleConfigCommand` in `package-manager-cli.ts` in pi `v1.0.0`).

use std::io::Write as _;

use ri_core::config::{PROJECT_DIR, agent_dir};
use ri_core::packages::resolve_resources;
use ri_core::settings::SettingsManager;

use crate::interactive::config_selector::ConfigSelector;
use crate::interactive::picker;

const USAGE: &str = "ri config [-l] [--approve|--no-approve]";

fn help() -> String {
    format!(
        "Usage:\n  {USAGE}\n\nOpen the resource configuration TUI to enable or disable package resources.\nWithout -l, starts in global settings (~/{PROJECT_DIR}/agent/settings.json).\nPress Tab in the TUI to switch between global and project-local modes.\n\nOptions:\n  -l, --local       Edit project overrides ({PROJECT_DIR}/settings.json)\n  -a, --approve     Trust project-local files for this command with -l\n  -na, --no-approve Ignore project-local files for this command with -l\n"
    )
}

/// Runs `ri config` with the arguments after `config`; its exit code.
pub fn run(args: &[String]) -> u8 {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        // pi's console.log adds a newline after the text's own.
        let _ = writeln!(std::io::stdout(), "{}", help());
        return 0;
    }
    let mut local = false;
    let mut trust = None;
    for arg in args {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => trust = Some(true),
            "-na" | "--no-approve" => trust = Some(false),
            option if option.starts_with('-') => {
                eprintln!("Unknown option {option} for \"config\".");
                eprintln!("Use \"ri --help\" or \"{USAGE}\".");
                return 1;
            }
            argument => {
                eprintln!("Unexpected argument {argument}.");
                eprintln!("Usage: {USAGE}");
                return 1;
            }
        }
    }
    match configure(local, trust) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn configure(local: bool, trust: Option<bool>) -> anyhow::Result<u8> {
    let cwd = std::env::current_dir()?;
    let agent_dir = agent_dir();
    let settings = crate::packages::command_settings(&cwd, &agent_dir, trust)?;
    let trusted = settings.project_trusted();
    if local && !trusted {
        eprintln!("Project is not trusted. Use --approve to modify local resource config.");
        return Ok(1);
    }
    for error in settings.errors() {
        eprintln!("Warning: {error}");
    }
    // Global mode shows what the user's settings alone resolve to.
    let global_settings = SettingsManager::load(&agent_dir, &cwd, false)?;
    let builtins = crate::startup::BUILTINS;
    let global = resolve_resources(&cwd, &agent_dir, &global_settings, &builtins);
    let project = if trusted {
        resolve_resources(&cwd, &agent_dir, &settings, &builtins)
    } else {
        global.clone()
    };
    let theme = settings.settings().theme.clone();
    let (_, rows) = ri_tui::terminal::size();
    let selector = ConfigSelector::new(
        &global,
        &project,
        settings,
        cwd,
        agent_dir.clone(),
        rows,
        local,
        trusted,
    );
    picker::configure(&agent_dir, theme.as_deref(), selector)?;
    Ok(0)
}
